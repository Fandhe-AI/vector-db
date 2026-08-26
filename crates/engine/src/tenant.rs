//! 行単位テナント境界の行ストア統合層（TASK-89・対象ビヘイビア: TABLE-9, TABLE-11。
//! TASK-95・対象ビヘイビア: RECOVER-4 の書き込みガード API を追加）。
//!
//! ## `operation_id` 必須化ガードと公開 API の構造（TASK-92・対象ビヘイビア: RECOVER-1）
//!
//! `pub fn` の [`insert_row`]・[`insert_rows`]・[`insert_typed_row`]・[`update_row`]・
//! [`delete_row`] は `operation_id: &OperationId` を必須引数として要求する
//! （codex-review P1 指摘・PR #217 対応。詳細は `recovery::required_op_id`
//! モジュールドキュメント参照）。ガード検証を持たない `pub(crate)` の
//! `*_unchecked` 版はクレート外へ公開しない。
//!
//! `policy.rs::PolicyContext::is_visible` の単一照合パス（CORE-2）へすべての可視性
//! 判定を委譲し、本モジュール独自のテナント比較は持たない（security.md P0）。
//! 提供する API は大きく 2 系統:
//!
//! - 読み取り側（[`visible_rows`]・[`verify_hits`]）: 行ストア（`catalog.rs` のテーブル
//!   スコープ行 API）を安全な上限内で走査し、可視行だけを列挙・検証する統合層。
//!   呼び出し元は主に `tests/tenant_isolation.rs`（TABLE-11 の 200 試行 × 4 テナント
//!   巡回検証）で、独立に期待集合を算出するための参照実装として使う。
//! - 書き込み側（[`insert_row`]・[`update_row`]・[`delete_row`]）: `PolicyContext::is_owner`
//!   （書き込み認可の単一照合パス）による所有権判定を経由してのみ行ストアを変更する
//!   ガード API（RECOVER-4）。`crate::core::EngineCore` の薄い委譲メソッドを経由して
//!   wire 層が DML を行う唯一の入口として設計している。生の UPDATE/DELETE を
//!   `Storage` の公開 API として新設しない（ガードを迂回できる経路を増やさない）。
//!
//! ## 設計記録: テーブル単位の物理分離は本タスクのスコープ外
//!
//! テナント境界は本モジュールが提供する「行単位」の可視性フィルタ（`PolicyContext`
//! 経由）を主軸として MVP を構成し、テナントごとにテーブルを動的構築する物理分離は
//! 実装しない（対象ビヘイビア: TABLE-9。詳細は spec 側のポインタ参照）。将来
//! テーブル単位分離を検討する場合は、本モジュールの可視性フィルタと独立した設計判断
//! として扱うこと。

use redb::ReadableTable;

use crate::catalog::{
    map_row_table_error, require_table_schema_write, user_rows_table_def, user_rows_table_name,
    validate_identifier, CatalogError,
};
use crate::kernel::SearchHit;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::{LedgerMode, OperationId};
use crate::storage::{
    bump_generation_and_commit, decode_row_tenant_and_visibility, encode_row, Row, RowInput,
    Storage, StorageError,
};

/// 1 ページあたりの走査件数（`catalog.rs::Storage::scan_table_page` の内部上限
/// `MAX_SCAN_PAGE_LIMIT` と同じ桁）。
const PAGE_LIMIT: u32 = 10_000;

/// [`visible_rows`] が保持してよい可視行数の上限。無制限 `Vec` 確保を避ける
/// （coding-rust.md「長さフィールドは上限検証してからアロケーションに使う」対応）。
/// テーブル全体の総行数ではなく可視行数を上限にすることで、大量の不可視行を持つ
/// テーブルでも呼び出し元テナントの可視行数だけに比例した確保量に収まる。
const MAX_VISIBLE_ROWS: usize = 100_000;

/// [`visible_rows`] が 1 回の呼び出しで走査してよい総行数（可視・不可視を問わない）の
/// 上限。`MAX_VISIBLE_ROWS` は出力（確保量）を抑えるが、他テナントの不可視行を
/// 大量に格納したテーブルでは出力がほぼ増えないまま `next` が尽きるまで全ページの
/// デコード・`PolicyContext::is_visible` 評価が実行され、計算量 DoS 経路になる
/// （codex-review 指摘・PR #153）。総走査行数にも明示的な上限を設け、超過時は
/// 部分結果を返さず [`TenantError::TooManyRowsScanned`] で fail-closed に拒否する。
const MAX_SCANNED_ROWS: usize = 1_000_000;

/// [`visible_rows`]・[`verify_hits`] のエラー型。`Display`・`Debug`・
/// `std::error::Error::source` のいずれにもテナント ID・行 id・テーブル名を含めず、
/// 他テナントの存在情報を漏らさない（`rls.rs::RlsError` と同じ契約。security.md P0）。
/// `CatalogError` を内部に保持するが、識別子を含む詳細は外部へ一切露出しない
/// （下記 `Debug`・`Error::source` の手書き実装を参照）。
pub enum TenantError {
    /// [`crate::catalog`] 側のエラー（テーブル不存在・行破損・redb バックエンドエラー等）。
    Catalog(CatalogError),
    /// 可視行数が [`MAX_VISIBLE_ROWS`] を超えたため、走査を打ち切って fail-closed に
    /// 拒否した（部分的な結果を黙って返さない）。
    TooManyVisibleRows { max: usize },
    /// 総走査行数（可視・不可視を問わない）が [`MAX_SCANNED_ROWS`] を超えたため、
    /// 走査を打ち切って fail-closed に拒否した。大量の不可視行を持つテーブルに対する
    /// 計算量 DoS（出力は増えないまま全ページのデコード・ポリシー評価を強制される
    /// 経路）を防ぐ（security.md テナント境界 P0。codex-review 指摘・PR #153）。
    TooManyRowsScanned { max: usize },
    /// [`verify_hits`] に渡された id が、走査対象テーブルの可視行集合に含まれない
    /// （不可視行・捏造 id のいずれも区別せず本 variant に統一する。他テナントの
    /// 存在情報を漏らさないため。security.md P0）。
    HitOutsideVisibleSet,
}

impl std::fmt::Display for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `CatalogError` の `Display`（`TableNotFound` のテーブル名・`RowNotFound` の
            // 行 ID を含む）をそのまま展開しない。認可前の呼び出し・エラーログ経由で
            // 他テナントの存在情報が漏れるのを防ぐため、識別子・バックエンド詳細を含まない
            // 固定文言に丸める（security.md テナント境界 P0）。原因の詳細は本型の外へは
            // 一切公開しない（`Debug`・`Error::source` も同様にサニタイズ済み。内部診断が
            // 必要な場合は本型を経由しない別経路を用意すること）。
            TenantError::Catalog(_) => write!(f, "tenant boundary catalog error"),
            TenantError::TooManyVisibleRows { max } => {
                write!(f, "too many visible rows: limit={max}")
            }
            TenantError::TooManyRowsScanned { max } => {
                write!(f, "too many rows scanned: limit={max}")
            }
            TenantError::HitOutsideVisibleSet => {
                write!(f, "hit id is outside the policy-visible row set")
            }
        }
    }
}

impl std::fmt::Debug for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `#[derive(Debug)]` は `CatalogError`（`TableNotFound` のテーブル名・
        // `RowNotFound` の行 ID 等）をそのまま展開してしまい、`Display` で
        // 隠した情報がパニック出力・`{:?}` ログ経由で再露出する（security.md
        // テナント境界 P0）。variant 名のみを出力し、内部の識別情報は含めない。
        match self {
            TenantError::Catalog(_) => f.write_str("Catalog(<redacted>)"),
            TenantError::TooManyVisibleRows { max } => f
                .debug_struct("TooManyVisibleRows")
                .field("max", max)
                .finish(),
            TenantError::TooManyRowsScanned { max } => f
                .debug_struct("TooManyRowsScanned")
                .field("max", max)
                .finish(),
            TenantError::HitOutsideVisibleSet => f.write_str("HitOutsideVisibleSet"),
        }
    }
}

impl std::error::Error for TenantError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `CatalogError` をそのまま `source()` で返すと、`Display` で固定文言に
        // 丸めた識別情報（テーブル名・行 ID 等）が一般的なエラーチェーン出力
        // （`anyhow` 等の `{:#}` 展開・ログ収集基盤）経由で再露出する
        // （security.md テナント境界 P0）。原因チェーンはここで打ち切り、常に
        // `None` を返す。
        None
    }
}

impl From<CatalogError> for TenantError {
    fn from(e: CatalogError) -> Self {
        TenantError::Catalog(e)
    }
}

/// `table` の全行を上限付きページング（`Storage::scan_table_page`）で走査し、`ctx`
/// （[`PolicyContext::is_visible`]）が可視と判定する行だけを列挙する（TABLE-9・
/// TABLE-11 の参照実装）。
///
/// 可視行数が [`MAX_VISIBLE_ROWS`] を超える場合は部分結果を返さず
/// [`TenantError::TooManyVisibleRows`] で拒否する。総走査行数（可視・不可視を
/// 問わない）が [`MAX_SCANNED_ROWS`] を超える場合も同様に部分結果を返さず
/// [`TenantError::TooManyRowsScanned`] で拒否する（他テナントの不可視行を大量に
/// 格納したテーブルに対する計算量 DoS を防ぐ。security.md テナント境界 P0）。
/// テーブル不存在は [`CatalogError::TableNotFound`] のまま [`TenantError::Catalog`]
/// へ伝播する（存在情報の扱いは呼び出し元の責務）。
pub fn visible_rows(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
) -> Result<Vec<Row>, TenantError> {
    let mut out = Vec::new();
    // カーソルは行ストアの物理キーと同じ `(tenant_id, id)` 形（TABLE-12）。`id` 単独では
    // 再開位置を表現できず、テナントをまたぐ走査で行を取りこぼす。
    let mut after: Option<(String, u64)> = None;
    let mut scanned: usize = 0;
    loop {
        let cursor = after.as_ref().map(|(t, id)| (t.as_str(), *id));
        let (page, next) = storage.scan_table_page(table, cursor, PAGE_LIMIT)?;
        if page.is_empty() && next.is_none() {
            break;
        }
        scanned = scanned.saturating_add(page.len());
        if scanned > MAX_SCANNED_ROWS {
            return Err(TenantError::TooManyRowsScanned {
                max: MAX_SCANNED_ROWS,
            });
        }
        for row in page {
            if ctx.is_visible(&row.tenant_id, row.visibility) {
                if out.len() >= MAX_VISIBLE_ROWS {
                    return Err(TenantError::TooManyVisibleRows {
                        max: MAX_VISIBLE_ROWS,
                    });
                }
                out.push(row);
            }
        }
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(out)
}

/// 検索結果 `hits` が、`table` に対する `ctx` の可視集合へすべて収まって
/// いることを **`(tenant_id, id)` の完全な行キー**で fail-closed に検証する
/// （TABLE-11: 200 試行 × 4 テナント巡回検証の
/// 混入 0 件アサーションを、`EngineCore::search`/`PrefilterIndex::search` の内部実装と
/// 独立した経路で裏付けるためのヘルパ）。
///
/// 1 件でも可視集合外の id があれば、走査を打ち切り即座に
/// [`TenantError::HitOutsideVisibleSet`] を返す。
pub fn verify_hits(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    hits: &[SearchHit],
) -> Result<(), TenantError> {
    let visible = visible_rows(storage, table, ctx)?;
    // 照合キーは行 `id` 単独ではなく完全な行キー `(tenant_id, id)`（対象ビヘイビア:
    // TABLE-12・RLS-9。codex-review P1 指摘・PR #194）。`id` だけの集合で照合すると、
    // 「可視な行（例: 他テナントの `Public` 行）と同じ `id` を持つ不可視行（別テナントの
    // `Private` 行）」由来のヒットを見逃す——`id` は可視集合に存在してしまうため。
    // ヒット側のテナントは `SearchHit::tenant_id`（検索経路が行の帰属として付与した値）
    // を使う。
    let visible_keys: std::collections::HashSet<(&str, u64)> = visible
        .iter()
        .map(|r| (r.tenant_id.as_str(), r.id))
        .collect();
    if hits
        .iter()
        .all(|hit| visible_keys.contains(&(hit.tenant_id.as_str(), hit.id)))
    {
        Ok(())
    } else {
        Err(TenantError::HitOutsideVisibleSet)
    }
}

/// [`insert_row`]・[`update_row`]・[`delete_row`] のエラー型（TASK-95・対象ビヘイビア:
/// RECOVER-4）。`Display`・`Debug`・`std::error::Error::source` のいずれにもテナント ID・
/// 行 id・テーブル名を含めず、他テナントの存在情報を漏らさない（[`TenantError`] と同じ
/// 契約。security.md P0）。
pub enum TenantWriteError {
    /// 呼び出し元が入力した `RowInput::tenant_id` が `ctx` のテナントと不一致
    /// （クライアント自身の入力に起因するため存在情報を含まない）。他テナント名義の
    /// 新規行の書き込み・自テナント行の他テナントへの付け替え試行の両方がここに入る。
    Forbidden,
    /// UPDATE/DELETE 対象行が不存在、または `ctx` が所有しない行（区別しない。
    /// 存在情報を漏らさないため fail-closed に統一する。security.md P0）。
    NotFound,
    /// INSERT 先 id に既存行がある（所有者を問わず同一 variant。上書きによる他テナント
    /// 行の破壊を遮断しつつ、所有テナントの存在情報を漏らさない）。
    IdConflict,
    /// `operation_id` の省略（句の欠落・明示 `NULL` を含む）。台帳あり構成
    /// （`recovery::required_op_id::LedgerMode::Ledgered`、既定）では書き込み系操作に
    /// `operation_id` の指定を必須とする（TASK-92・対象ビヘイビア: RECOVER-1）。
    /// `crate::core::EngineCore::{insert_row, update_row, delete_row}` が
    /// `crate::tenant::*_unchecked` へ委譲する**前**に `self.ledger_mode` でガードを
    /// 適用し、本モジュールの `pub fn insert_row`/`insert_rows`/`insert_typed_row`/
    /// `update_row`/`delete_row` は `operation_id` を必須引数として要求したうえで
    /// `LedgerMode::Ledgered` で内部ガードするため、いずれの経路でも本 variant が
    /// 実際に返る時点で書き込みトランザクションは未開始（ERR-2: `23502`）。
    MissingOperationId,
    /// [`crate::catalog`] 側のエラー（テーブル不存在・行破損・redb バックエンドエラー等）。
    Catalog(CatalogError),
    /// [`crate::storage`] 側のエンコード/デコードエラー（`RowInput` の入力検証失敗等）。
    Storage(StorageError),
}

impl TenantWriteError {
    /// SQLSTATE 風 `wire_code`（coding-rust.md「エラー型は SQLSTATE 風 wire_code の設計に
    /// 従う」）。対象ビヘイビア: RECOVER-4・ERR-2（`docs/spec/04-behavior/error-format.md`
    /// をポインタ参照。写像の具体値・採用理由は spec 側の管理事項であり、本コメントへは
    /// 転記しない。spec-confidentiality.md 参照）。
    pub fn wire_code(&self) -> &'static str {
        match self {
            TenantWriteError::Forbidden => "42501",
            TenantWriteError::NotFound => "P0002",
            TenantWriteError::IdConflict => "23505",
            TenantWriteError::MissingOperationId => "23502",
            TenantWriteError::Catalog(_) | TenantWriteError::Storage(_) => "XX000",
        }
    }
}

impl std::fmt::Display for TenantWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TenantWriteError::Forbidden => {
                write!(f, "tenant write forbidden: not the row owner")
            }
            TenantWriteError::NotFound => write!(f, "tenant write target row not found"),
            TenantWriteError::IdConflict => write!(f, "tenant write id conflict"),
            TenantWriteError::MissingOperationId => write!(f, "missing operation_id"),
            // `CatalogError`/`StorageError` の `Display` をそのまま展開しない（`TenantError`
            // と同じ理由。security.md テナント境界 P0）。
            TenantWriteError::Catalog(_) => write!(f, "tenant write catalog error"),
            TenantWriteError::Storage(_) => write!(f, "tenant write storage error"),
        }
    }
}

impl std::fmt::Debug for TenantWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `#[derive(Debug)]` は内部の `CatalogError`/`StorageError` をそのまま展開し、
        // `Display` で隠した情報がパニック出力・`{:?}` ログ経由で再露出する
        // （security.md テナント境界 P0）。variant 名のみを出力する。
        match self {
            TenantWriteError::Forbidden => f.write_str("Forbidden"),
            TenantWriteError::NotFound => f.write_str("NotFound"),
            TenantWriteError::IdConflict => f.write_str("IdConflict"),
            TenantWriteError::MissingOperationId => f.write_str("MissingOperationId"),
            TenantWriteError::Catalog(_) => f.write_str("Catalog(<redacted>)"),
            TenantWriteError::Storage(_) => f.write_str("Storage(<redacted>)"),
        }
    }
}

impl std::error::Error for TenantWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `TenantError::source` と同じ理由で原因チェーンをここで打ち切る
        // （security.md テナント境界 P0）。
        None
    }
}

impl From<CatalogError> for TenantWriteError {
    fn from(e: CatalogError) -> Self {
        TenantWriteError::Catalog(e)
    }
}

impl From<StorageError> for TenantWriteError {
    fn from(e: StorageError) -> Self {
        TenantWriteError::Storage(e)
    }
}

/// 行ストア（`user_rows/{table_name}`）への一意挿入ヘルパ（TABLE-12・TASK-130・
/// PR #194 の申し送り対応）。[`insert_row_unchecked`]・[`insert_rows_unchecked`]・
/// [`insert_typed_row_unchecked`] が共有する唯一の実装。
///
/// 旧来は `get` で存在確認してから `insert` する 2 回の B-tree 探索だったが、
/// `redb::Table::insert` が上書き前の旧値を `Option<AccessGuard>` として返す性質を
/// 使い、`insert` 一回の走査結果だけで存在判定する（`get` を省略）。返る旧値は
/// 中身を読まず即座に破棄する（存在の有無だけが関心事）。
///
/// # 呼び出し元が守るべき前提（Err 時は commit しない）
///
/// `Some` を返した時点で該当キーには **新しい値がすでに書き込まれている**（redb の
/// `insert` は探索と書き込みを同一パスで行うため、後から取り消す API はない）。
/// この上書きを外部から観測させないのは、本関数の `Err` を受け取った呼び出し元が
/// 属する write トランザクションを **commit せずに drop（abort）する**という契約に
/// よる（`redb::WriteTransaction` の drop 契約。[`insert_row_unchecked`] 等はいずれも
/// 自身が `begin_write` した txn をこの関数の外側でだけ commit するため、この契約を
/// 満たす）。将来の呼び出し元を追加する場合もこの前提を破らないこと。
///
/// キーは呼び出し元がサーバー側導出テナント（`ctx.tenant_id()`）で組み立てる
/// （本関数はキー生成に関与しない）。他テナントの行キーへ触れる経路を持たないため、
/// 他テナント行の有無で分岐する処理は本関数にも存在しない（RLS-9・fail-closed）。
fn insert_unique_row(
    row_table: &mut redb::Table<'_, (&'static str, u64), &'static [u8]>,
    key: (&str, u64),
    encoded: &[u8],
) -> Result<(), TenantWriteError> {
    if row_table
        .insert(key, encoded)
        .map_err(CatalogError::from)?
        .is_some()
    {
        return Err(TenantWriteError::IdConflict);
    }
    Ok(())
}

/// `table` へ新規行を 1 件挿入する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// `row.tenant_id` が `ctx` のテナントと不一致なら
/// [`TenantWriteError::Forbidden`]（他テナント名義での新規行書き込み・テナント
/// 付け替えの試行を遮断。判定は [`PolicyContext::is_owner`] の単一照合パス経由）。
///
/// 重複検出のスコープ（対象ビヘイビア: TABLE-12・RLS-9。codex-review P0 指摘・PR #194）:
/// 行ストアの物理キーは `(tenant_id, id)` で名前空間化されており、既存行の照会は
/// **サーバー側導出テナント（`ctx.tenant_id()`）の名前空間内だけ**を対象とする
/// （クライアント自己申告の `row.tenant_id` はキー構築に用いない）。したがって
/// 同一テナント内の重複のみ [`TenantWriteError::IdConflict`]（`23505`）となり、
/// 他テナントが同じ `id` を保持していても本経路は通常どおり成功する。他テナント行の
/// 有無で分岐する処理を一切持たないため、応答（成否・`wire_code`・文言）からも
/// 実行経路の分岐からも他テナントの存在情報を観測できない（fail-closed）。
///
/// スキーマ取得・次元検証・所有権判定・書き込みを単一の write トランザクション内で
/// 行い、失敗時は commit せずトランザクションを破棄する（`redb::WriteTransaction` の
/// drop 契約により abort。判定と書き込みの間に TOCTOU を作らない。redb は単一
/// ライタで書き込みを直列化する）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`insert_row_unchecked`] へ委譲する（TASK-92・対象ビヘイビア: RECOVER-1・
/// codex-review P1 指摘・PR #217）。本関数はモジュール冒頭ドキュメントの「公開 API」
/// 層であり、`operation_id` を省略できる経路を型で塞ぐ。
pub fn insert_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    LedgerMode::Ledgered.require(Some(operation_id))?;
    insert_row_unchecked(storage, table, ctx, id, row)
}

/// [`insert_row`] のガードなし実体（`pub(crate)`。TASK-92・RECOVER-1）。
/// `operation_id` 必須化ガードを持たないため、クレート外から直接呼べない
/// （`pub(crate)` によりガードを迂回できる経路を閉じる）。呼び出し元は
/// [`insert_row`]（本モジュール内でガード済み）と
/// `crate::core::EngineCore::insert_row`（`self.ledger_mode` でガード済み）の 2 か所。
pub(crate) fn insert_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    // ストレージへ触れる前に、クライアント自己申告の `tenant_id` を ctx と照合する
    // （security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。
    if !ctx.is_owner(row.tenant_id) {
        return Err(TenantWriteError::Forbidden);
    }
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        schema.validate_embedding_dim(row.embedding.len())?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // 物理キーはサーバー側導出テナントで名前空間化する（TABLE-12・RLS-9）。
        let key = (ctx.tenant_id(), id);
        let encoded = encode_row(row)?;
        insert_unique_row(&mut row_table, key, encoded.as_slice())?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

/// `table` へ複数行をまとめて挿入する（[`insert_row`] のバッチ版。TASK-95・
/// 対象ビヘイビア: RECOVER-4, TABLE-12, RLS-9）。
///
/// 認可・重複検出の契約は [`insert_row`] と同一で、バッチ全体を単一の write
/// トランザクションで処理する（1 件でも拒否されれば commit せず全体が未反映になる。
/// `redb::WriteTransaction` の drop 契約）。
///
/// 存在確認は行ごとに `get` → `insert` の 2 回 B-tree 探索する旧実装ではなく、
/// [`insert_unique_row`]（`insert` の戻り値で既存行の有無を判定）を使い 1 回に
/// 削減している（TASK-130・PR #194 の申し送り対応）。
///
/// - `row.tenant_id` が `ctx` と不一致な行が 1 件でもあれば [`TenantWriteError::Forbidden`]
///   （ストレージへ触れる前に全件を検査する）
/// - 物理キーは `(ctx.tenant_id(), id)`（TABLE-12）。既存行との衝突、および
///   **同一バッチ内の id 重複**はいずれも [`TenantWriteError::IdConflict`]。後者を
///   検出しないと、バッチ内の後勝ちで先行行が黙って上書きされ、[`insert_row`] が
///   守っている「既存行を上書きしない」契約をバッチ経由で迂回できてしまう
/// - 他テナントが同じ `id` を保持していても成功する（別キーのため。RLS-9）
///
/// 空バッチはテーブル存在確認のみを行い、世代を進めずに成功する
/// （`catalog.rs::Storage::insert_rows_into_table` と同じ扱い。無変更コミットで
/// 既存インデックスを不要に失効させない）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`insert_rows_unchecked`] へ委譲する（[`insert_row`] と同じ設計。TASK-92・
/// RECOVER-1・codex-review P1 指摘・PR #217。バッチ経路にガード付き公開入口が
/// 存在しなかった点も本対応で塞ぐ）。
pub fn insert_rows(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    rows: &[(u64, RowInput<'_>)],
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    LedgerMode::Ledgered.require(Some(operation_id))?;
    insert_rows_unchecked(storage, table, ctx, rows)
}

/// [`insert_rows`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計。呼び出し元は本モジュール内の [`insert_rows`] のみ）。
pub(crate) fn insert_rows_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    rows: &[(u64, RowInput<'_>)],
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    // ストレージへ触れる前に、クライアント自己申告の `tenant_id` を全件検査する
    // （security.md P0。[`insert_row`] と同じ単一照合パス `PolicyContext::is_owner`）。
    if rows.iter().any(|(_, row)| !ctx.is_owner(row.tenant_id)) {
        return Err(TenantWriteError::Forbidden);
    }
    // バッチ内の id 重複検出（上記ドキュメント参照）。件数は呼び出し元のスライス長で
    // 上限が決まるため、確保はフォールブルにする（無制限 `with_capacity` を使わない。
    // coding-rust.md）。
    let mut seen_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    seen_ids.try_reserve(rows.len()).map_err(|_| {
        TenantWriteError::Storage(StorageError::Codec(
            "failed to reserve batch id set".to_string(),
        ))
    })?;
    for (id, _) in rows {
        if !seen_ids.insert(*id) {
            return Err(TenantWriteError::IdConflict);
        }
    }

    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        if rows.is_empty() {
            drop(write_txn);
            return Ok(());
        }
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        for (id, row) in rows {
            schema.validate_embedding_dim(row.embedding.len())?;
            let key = (ctx.tenant_id(), *id);
            let encoded = encode_row(row)?;
            insert_unique_row(&mut row_table, key, encoded.as_slice())?;
        }
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

/// スキーマ列順の型付き値列から 1 行挿入する（`catalog.rs::Storage::insert_typed_row` の
/// テナント境界付き版。TASK-95・対象ビヘイビア: RECOVER-4, TABLE-12）。
///
/// 行の `tenant_id` は**引数で受け取らず** `ctx`（サーバー側導出テナント。WIRE-2・
/// RLS-6）から導出する（クライアント自己申告のテナントを書き込みへ持ち込む経路を
/// 作らない。security.md P0）。重複検出・物理キーの扱いは [`insert_row`] と同一。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`insert_typed_row_unchecked`] へ委譲する（[`insert_row`] と同じ設計。
/// TASK-92・RECOVER-1・codex-review P1 指摘・PR #217）。`crate::sql::exec::execute_insert`
/// は `sql::allowlist::validate_insert` が既にガード済みであることを前提に
/// [`insert_typed_row_unchecked`] を直接呼ぶため、本関数を経由しない
/// （`sql/exec.rs` のドキュメント参照）。
pub fn insert_typed_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    visibility: crate::storage::Visibility,
    values: &[crate::row_codec::Value],
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    LedgerMode::Ledgered.require(Some(operation_id))?;
    insert_typed_row_unchecked(storage, table, ctx, id, visibility, values)
}

/// [`insert_typed_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と
/// 同じ設計）。呼び出し元は本モジュール内の [`insert_typed_row`] と
/// `crate::sql::exec::execute_insert`（`allowlist::validate_insert` でガード済み）。
pub(crate) fn insert_typed_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    visibility: crate::storage::Visibility,
    values: &[crate::row_codec::Value],
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        let vector_idx = schema
            .columns
            .iter()
            .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)))
            .ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(
                    "table has no VECTOR column".to_string(),
                ))
            })?;
        let embedding = match values.get(vector_idx) {
            Some(crate::row_codec::Value::Vector(v)) => v.clone(),
            _ => {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "VECTOR column value missing or not a Vector".to_string(),
                )))
            }
        };
        schema.validate_embedding_dim(embedding.len())?;
        let metadata = crate::row_codec::encode_scalar_columns(&schema, values)
            .map_err(|e| CatalogError::Invalid(e.to_string()))?;
        let row = RowInput {
            tenant_id: ctx.tenant_id(),
            visibility,
            embedding: &embedding,
            metadata: &metadata,
        };
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        let key = (ctx.tenant_id(), id);
        let encoded = encode_row(&row)?;
        insert_unique_row(&mut row_table, key, encoded.as_slice())?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

/// `table` の既存行を 1 件更新する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// `row.tenant_id` が `ctx` のテナントと不一致なら
/// [`TenantWriteError::Forbidden`]（自テナント行を他テナントへ付け替える試行を含む）。
/// 対象行が不存在、または既存行の所有者が `ctx` と一致しない場合は
/// **区別せず** [`TenantWriteError::NotFound`]（他テナントの存在情報を漏らさない。
/// security.md P0）。
///
/// スキーマ取得・次元検証・既存行の所有権判定・書き込みを単一の write トランザクション
/// 内で行う（[`insert_row`] と同じ TOCTOU 対策）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`update_row_unchecked`] へ委譲する（[`insert_row`] と同じ設計。TASK-92・
/// RECOVER-1・codex-review P1 指摘・PR #217）。
pub fn update_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    LedgerMode::Ledgered.require(Some(operation_id))?;
    update_row_unchecked(storage, table, ctx, id, row)
}

/// [`update_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計）。呼び出し元は本モジュール内の [`update_row`] と
/// `crate::core::EngineCore::update_row`（`self.ledger_mode` でガード済み）。
pub(crate) fn update_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    if !ctx.is_owner(row.tenant_id) {
        return Err(TenantWriteError::Forbidden);
    }
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        schema.validate_embedding_dim(row.embedding.len())?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // 物理キーの名前空間化（TABLE-12）により、他テナントの行はそもそも別キーで
        // 到達不能（構造的な遮断）。既存行の所有者照合は `is_owner` の単一照合パスに
        // 残し、二重防御とする（旧フォーマット行の混在等でヘッダのテナントが
        // キーと食い違う場合も fail-closed 側に倒れる）。
        // `AccessGuard` の借用をこのブロック内に閉じ込め、後続の可変借用（`insert`）と
        // 衝突しないようにする。
        let key = (ctx.tenant_id(), id);
        let owns_existing = match row_table.get(&key).map_err(CatalogError::from)? {
            Some(guard) => {
                let (existing_tenant, _existing_visibility) =
                    decode_row_tenant_and_visibility(guard.value())?;
                ctx.is_owner(existing_tenant)
            }
            None => false,
        };
        if !owns_existing {
            return Err(TenantWriteError::NotFound);
        }
        let encoded = encode_row(row)?;
        row_table
            .insert(key, encoded.as_slice())
            .map_err(CatalogError::from)?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

/// `table` の既存行を 1 件削除する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// 対象行が不存在、または既存行の所有者が `ctx` と一致しない場合は
/// **区別せず** [`TenantWriteError::NotFound`]（[`update_row`] と同じ契約。
/// security.md P0）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`delete_row_unchecked`] へ委譲する（[`insert_row`] と同じ設計。TASK-92・
/// RECOVER-1・codex-review P1 指摘・PR #217）。
pub fn delete_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    LedgerMode::Ledgered.require(Some(operation_id))?;
    delete_row_unchecked(storage, table, ctx, id)
}

/// [`delete_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計）。呼び出し元は本モジュール内の [`delete_row`] と
/// `crate::core::EngineCore::delete_row`（`self.ledger_mode` でガード済み）。
pub(crate) fn delete_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        // 次元検証は不要だが、テーブル不存在の判定・並行 DDL との整合のため
        // `insert_row`/`update_row` と同じ前段を通す。
        require_table_schema_write(&write_txn, table)?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // `update_row` と同じく `(tenant_id, id)` キー（TABLE-12）＋ `is_owner` の二重防御。
        let key = (ctx.tenant_id(), id);
        let owns_existing = match row_table.get(&key).map_err(CatalogError::from)? {
            Some(guard) => {
                let (existing_tenant, _existing_visibility) =
                    decode_row_tenant_and_visibility(guard.value())?;
                ctx.is_owner(existing_tenant)
            }
            None => false,
        };
        if !owns_existing {
            return Err(TenantWriteError::NotFound);
        }
        row_table.remove(&key).map_err(CatalogError::from)?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::storage::{RowInput, Visibility};

    // 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn schema(table: &str) -> TableSchema {
        TableSchema::new(
            table,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        )
    }

    // 対象ビヘイビア: TABLE-9。
    #[test]
    fn visible_rows_includes_other_tenant_public_rows() {
        let path = unique_db_path("visible-public");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");
        storage
            .insert_rows_into_table(
                "docs",
                &[
                    (
                        1,
                        RowInput {
                            tenant_id: "tenant-a",
                            visibility: Visibility::Public,
                            embedding: &[1.0, 0.0],
                            metadata: &[],
                        },
                    ),
                    (
                        2,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Public,
                            embedding: &[0.0, 1.0],
                            metadata: &[],
                        },
                    ),
                    (
                        3,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Private,
                            embedding: &[1.0, 1.0],
                            metadata: &[],
                        },
                    ),
                ],
            )
            .expect("seed rows");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut rows = visible_rows(&storage, "docs", &ctx).expect("visible_rows ok");
        rows.sort_by_key(|r| r.id);
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2],
            "tenant-a ctx must see its own row and the other tenant's Public row, \
             but not the other tenant's Private row"
        );
    }

    // 対象ビヘイビア: TABLE-11。`verify_hits` は可視集合外の id を fail-closed に拒否する。
    #[test]
    fn verify_hits_rejects_id_outside_visible_set() {
        let path = unique_db_path("verify-hits");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");
        storage
            .insert_rows_into_table(
                "docs",
                &[
                    (
                        1,
                        RowInput {
                            tenant_id: "tenant-a",
                            visibility: Visibility::Public,
                            embedding: &[1.0, 0.0],
                            metadata: &[],
                        },
                    ),
                    (
                        2,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Private,
                            embedding: &[0.0, 1.0],
                            metadata: &[],
                        },
                    ),
                ],
            )
            .expect("seed rows");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 照合キーは `(tenant_id, id)`（TABLE-12・RLS-9）。
        let own_hit = SearchHit::new("tenant-a", 1, 1.0);
        let foreign_private_hit = SearchHit::new("tenant-b", 2, 0.5);
        assert!(verify_hits(&storage, "docs", &ctx, std::slice::from_ref(&own_hit)).is_ok());
        assert!(matches!(
            verify_hits(&storage, "docs", &ctx, &[own_hit, foreign_private_hit]),
            Err(TenantError::HitOutsideVisibleSet)
        ));
    }

    // 対象ビヘイビア: RECOVER-4（負方向・生 API の到達範囲確認）。
    // `crate::catalog::Storage::insert_row_into_table` は codex-review P0 指摘
    // （PR #194）を受けて `pub(crate)` 化し、クレート外（`tests/` 配下の結合テスト・
    // wire-server 等）からは到達不能にした。この生 API は本モジュール内では
    // （例: 将来の移行ツール等で）引き続き参照しうるため、クレート内ユニットテストとして
    // 「テナント境界チェックを経由しない書き込みは実際に行を書き換える」ことを確認する。
    // 旧・結合テスト版（`tests/tenant_breach.rs::recover4_checker_detects_unguarded_mutation`）
    // は `pub(crate)` 化に伴いクレート外から呼べなくなったため、このユニットテストへ
    // 移設した。
    #[test]
    fn raw_insert_row_into_table_bypasses_tenant_guard() {
        let path = unique_db_path("raw-insert-bypass");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");

        // ガード付き経路（`insert_row_unchecked`。`operation_id` 必須化ガードは本テストの
        // 対象外なので、ガードを内包する `pub fn insert_row` ではなくガードなし実体を
        // 直接使う）で tenant-b 名義の行を正規に投入する。
        let owner = PolicyContext::new("tenant-b").expect("valid tenant");
        insert_row_unchecked(
            &storage,
            "docs",
            &owner,
            1,
            &RowInput {
                tenant_id: "tenant-b",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: &[],
            },
        )
        .expect("seed tenant-b row via guarded path");

        // ガードを経由しない生の `Storage::insert_row_into_table`（`pub(crate)`）で
        // 同じ id を tenant-a 名義へ上書きできてしまうことを確認する（クレート内から
        // 到達可能である以上、この経路自体は塞がっていないことの記録。クレート外からの
        // 到達不能性が本対応の主眼）。
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[9.0, 9.0],
                    metadata: &[],
                },
            )
            .expect("unguarded write succeeds by construction");

        let after = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("read back row");
        assert_eq!(after.tenant_id, "tenant-a");
    }

    // 対象ビヘイビア: TABLE-12・TASK-130。[`insert_unique_row`] 導入（`get` を省き
    // `insert` の戻り値で衝突判定）により、衝突行より前に処理される行が実際に write
    // txn 内へ書き込まれても、txn が commit されなければ何も永続化されないことを固定
    // する（PR #194 の「既存行と衝突するバッチは IdConflict・既存行は不変」契約の
    // 単体テスト版。バッチ末尾で衝突するケースを対象に、先行行が残らないことを確認）。
    #[test]
    fn insert_rows_with_trailing_conflict_persists_nothing() {
        let path = unique_db_path("trailing-conflict");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");

        let a = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = OperationId::parse("test-op").expect("valid operation_id");

        // id=3 を事前に投入しておき、バッチ末尾でこの id と衝突させる。
        insert_row(
            &storage,
            "docs",
            &a,
            3,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[9.0, 9.0],
                metadata: b"original",
            },
            &op_id,
        )
        .expect("seed id=3");

        let batch = [
            (
                1u64,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: b"one",
                },
            ),
            (
                2u64,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: b"two",
                },
            ),
            (
                3u64,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 1.0],
                    metadata: b"overwrite-attempt",
                },
            ),
        ];
        let err = insert_rows(&storage, "docs", &a, &batch, &op_id)
            .expect_err("trailing id=3 conflicts with the seeded row");
        assert!(matches!(err, TenantWriteError::IdConflict));

        // id=1・2 は「衝突より前に処理された行」だが、txn が commit されていない
        // ため一切書き込まれていない（all-or-nothing）。
        assert!(
            storage.get_row_from_table("docs", "tenant-a", 1).is_err(),
            "id=1 must not have been persisted"
        );
        assert!(
            storage.get_row_from_table("docs", "tenant-a", 2).is_err(),
            "id=2 must not have been persisted"
        );
        // id=3 は元の内容のまま（insert-then-abort でも上書きは永続化されない）。
        let row3 = storage
            .get_row_from_table("docs", "tenant-a", 3)
            .expect("id=3 must still exist with its original content");
        assert_eq!(row3.metadata, b"original".to_vec());
    }
}
