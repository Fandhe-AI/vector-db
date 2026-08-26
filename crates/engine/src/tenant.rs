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

use redb::{ReadableDatabase, ReadableTable};

use crate::catalog::{
    map_row_table_error, require_table_schema_write, user_rows_table_def, user_rows_table_name,
    validate_identifier, CatalogError,
};
use crate::kernel::SearchHit;
use crate::policy::PolicyContext;
use crate::recovery::ledger::{self, LedgerWrite};
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
    /// `operation_id` 台帳（`crate::recovery::ledger`）テーブルの読み書きで検出した
    /// 内部エラー（未知フォーマットバージョンの混入・redb バックエンド障害）。
    /// `Storage(StorageError::Codec)` と型を分ける（Cursor Bugbot 指摘・PR #226）:
    /// 台帳の破損はクライアントが送った行データとは無関係のサーバー内部事象であり、
    /// `sql::exec::execute_insert` の呼び出し元マッピングが `StorageError::Codec` を
    /// 「行データ不正（`22000`）」として丸めてしまうと、台帳破損を「送った行が不正」
    /// という誤ったクライアント向けエラーへ変換してしまう。台帳エラーは常に
    /// `wire_code` `XX000`（内部事象）に固定し、クライアントへ再試行を促す誤情報を
    /// 出さない（fail-closed）。
    LedgerCorrupted(StorageError),
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
            TenantWriteError::LedgerCorrupted(_) => "XX000",
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
            TenantWriteError::LedgerCorrupted(_) => write!(f, "tenant write ledger error"),
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
            TenantWriteError::LedgerCorrupted(_) => f.write_str("LedgerCorrupted(<redacted>)"),
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
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    insert_row_unchecked(storage, table, ctx, id, row, ledger_write)
}

/// [`insert_row`] のガードなし実体（`pub(crate)`。TASK-92・RECOVER-1）。
/// `operation_id` 必須化ガードを持たないため、クレート外から直接呼べない
/// （`pub(crate)` によりガードを迂回できる経路を閉じる）。呼び出し元は
/// [`insert_row`]（本モジュール内でガード済み）と
/// `crate::core::EngineCore::insert_row`（`self.ledger_mode` でガード済み）の 2 か所。
///
/// `ledger`（TASK-93・対象ビヘイビア: RECOVER-2）: 行書き込みと**同一の write
/// トランザクション**内で台帳へ追記する（順序: スキーマ取得 → 台帳追記 → 行書き込み →
/// commit）。台帳を先に触っておくことで、行側の `IdConflict` によりトランザクションが
/// drop された場合に台帳も一緒に破棄される（原子性）ことを結合テストで直接検証できる
/// （`recovery::ledger` モジュールドキュメント参照）。
pub(crate) fn insert_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    ledger_write: LedgerWrite<'_>,
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
        ledger::record_in_txn(&write_txn, ctx.tenant_id(), table, ledger_write)
            .map_err(TenantWriteError::LedgerCorrupted)?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // 物理キーはサーバー側導出テナントで名前空間化する（TABLE-12・RLS-9）。
        let key = (ctx.tenant_id(), id);
        if row_table.get(&key).map_err(CatalogError::from)?.is_some() {
            return Err(TenantWriteError::IdConflict);
        }
        let encoded = encode_row(row)?;
        row_table
            .insert(key, encoded.as_slice())
            .map_err(CatalogError::from)?;
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
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    insert_rows_unchecked(storage, table, ctx, rows, ledger_write)
}

/// [`insert_rows`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計。呼び出し元は本モジュール内の [`insert_rows`] のみ）。
///
/// `ledger`（TASK-93・RECOVER-2）: 空バッチは台帳も書かず世代も進めない現行方針を
/// 維持する（[`insert_row_unchecked`] のドキュメント参照。順序はスキーマ取得 →
/// 空バッチ早期 return → 台帳追記 → 行書き込み → commit）。
pub(crate) fn insert_rows_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    rows: &[(u64, RowInput<'_>)],
    ledger_write: LedgerWrite<'_>,
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
        ledger::record_in_txn(&write_txn, ctx.tenant_id(), table, ledger_write)
            .map_err(TenantWriteError::LedgerCorrupted)?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        for (id, row) in rows {
            schema.validate_embedding_dim(row.embedding.len())?;
            let key = (ctx.tenant_id(), *id);
            if row_table.get(&key).map_err(CatalogError::from)?.is_some() {
                return Err(TenantWriteError::IdConflict);
            }
            let encoded = encode_row(row)?;
            row_table
                .insert(key, encoded.as_slice())
                .map_err(CatalogError::from)?;
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
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    insert_typed_row_unchecked(storage, table, ctx, id, visibility, values, ledger_write)
}

/// [`insert_typed_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と
/// 同じ設計）。呼び出し元は本モジュール内の [`insert_typed_row`] と
/// `crate::sql::exec::execute_insert`（`allowlist::validate_insert` でガード済み。
/// `LedgerMode::resolve` の結果をそのまま渡す。TASK-93・RECOVER-2）。
pub(crate) fn insert_typed_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    visibility: crate::storage::Visibility,
    values: &[crate::row_codec::Value],
    ledger_write: LedgerWrite<'_>,
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
        ledger::record_in_txn(&write_txn, ctx.tenant_id(), table, ledger_write)
            .map_err(TenantWriteError::LedgerCorrupted)?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        let key = (ctx.tenant_id(), id);
        if row_table.get(&key).map_err(CatalogError::from)?.is_some() {
            return Err(TenantWriteError::IdConflict);
        }
        let encoded = encode_row(&row)?;
        row_table
            .insert(key, encoded.as_slice())
            .map_err(CatalogError::from)?;
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
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    update_row_unchecked(storage, table, ctx, id, row, ledger_write)
}

/// [`update_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計）。呼び出し元は本モジュール内の [`update_row`] と
/// `crate::core::EngineCore::update_row`（`self.ledger_mode` でガード済み）。
///
/// `ledger`（TASK-93・RECOVER-2）: 対象行が不存在（`NotFound`）の場合は台帳へ触れない
/// （後続 §T2 相当の結合テストが「未記録」であることを検証する）。所有権判定
/// （`owns_existing`）の**後**に台帳追記する順序を取る。
pub(crate) fn update_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    ledger_write: LedgerWrite<'_>,
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
        ledger::record_in_txn(&write_txn, ctx.tenant_id(), table, ledger_write)
            .map_err(TenantWriteError::LedgerCorrupted)?;
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
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    delete_row_unchecked(storage, table, ctx, id, ledger_write)
}

/// [`delete_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計）。呼び出し元は本モジュール内の [`delete_row`] と
/// `crate::core::EngineCore::delete_row`（`self.ledger_mode` でガード済み）。
///
/// `ledger`（TASK-93・RECOVER-2）: [`update_row_unchecked`] と同じく、対象行が
/// 不存在（`NotFound`）の場合は台帳へ触れない。所有権判定（`owns_existing`）の
/// **後**に台帳追記する。
pub(crate) fn delete_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    ledger_write: LedgerWrite<'_>,
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
        ledger::record_in_txn(&write_txn, ctx.tenant_id(), table, ledger_write)
            .map_err(TenantWriteError::LedgerCorrupted)?;
        row_table.remove(&key).map_err(CatalogError::from)?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

/// `table` に `op_id` が台帳記録済みかを照会する（TASK-93、対象ビヘイビア: RECOVER-2）。
/// `crate::core::EngineCore::operation_recorded` からの薄い委譲先。`pub(crate)` に
/// 限定する（codex-review P1 指摘・PR #226）: `EngineCore::operation_recorded` は
/// `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）で台帳へ一切触れず
/// `LedgerLookup::NoLedger` を返す契約だが、本関数は `Storage` を直接受け取り
/// `ledger_mode` の状態を知らないため、DB に過去（`Ledgered` 構成時）の記録が
/// 残っていればそれをそのまま観測してしまう。これを公開したままだと
/// `EngineCore` の「台帳を持たない構成では照会しない」という fail-closed な
/// 区別を呼び出し元が迂回できてしまう。`EngineCore::operation_recorded` 経由の
/// 委譲（`LedgerLookup` 判定込み）に一本化し、モード非依存の生の照会結果を
/// クレート外へ公開しない。
///
/// 照会範囲は呼び出し元テナント（`ctx.tenant_id()`）の名前空間に閉じる。他テナントの
/// `operation_id` 存在を成否・文言・経路差で観測できる経路にはならない（TABLE-12・
/// RLS-9 と同型。security.md P0）。
pub(crate) fn operation_recorded(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    op_id: &OperationId,
) -> Result<bool, TenantWriteError> {
    validate_identifier(table)?;
    let read_txn = storage.db().begin_read().map_err(CatalogError::from)?;
    ledger::contains_in_read_txn(&read_txn, ctx.tenant_id(), table, op_id)
        .map_err(TenantWriteError::LedgerCorrupted)
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
            // 本テストの主眼は台帳ではなくテナント境界の到達範囲確認のため、台帳は
            // 使わない（`LedgerWrite::Disabled`）。
            LedgerWrite::Disabled,
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
}
