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
use crate::recovery::content_hash;
use crate::recovery::ledger::{self, LedgerRecordError, LedgerWrite};
use crate::recovery::required_op_id::{LedgerMode, OperationId};
use crate::storage::{
    decode_row_tenant_and_visibility, encode_row, Row, RowInput, Storage, StorageError,
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
    /// 台帳（TASK-93）に記録済みの `operation_id` へ、**内容が一致する**書き込みが
    /// 再送された（TASK-101・対象ビヘイビア: RECOVER-10。TASK-94・RECOVER-3 の
    /// 重複拒否契約を包含する）。commit 済み確定の根拠として扱ってよく、`23505`
    /// （`UniqueViolation` と同じ分類。error_format.rs のコメント参照）へ写像する。
    /// 行キー衝突（[`TenantWriteError::IdConflict`]）とは別 variant にすることで、
    /// クライアントが「先行実行が commit 済み」（RECOVER-7 が使う判定）を行キー衝突と
    /// 取り違えない固定文言を返せるようにする。
    DuplicateOperationId,
    /// 台帳に記録済みの `operation_id` へ、**内容が異なる**書き込みが再送された、
    /// または内容一致を証明できない旧フォーマット（v1）エントリへ再送された
    /// （TASK-101・RECOVER-10）。commit 済み確定の根拠にしない fail-closed 判定
    /// （`22023`）。行内容・テナント・他テナントの存在情報は含まない。
    OperationIdContentMismatch,
}

impl TenantWriteError {
    /// SQLSTATE 風 `wire_code`（coding-rust.md「エラー型は SQLSTATE 風 wire_code の設計に
    /// 従う」）。対象ビヘイビア: RECOVER-4・ERR-2（`docs/spec/04-behavior/error-format.md`
    /// をポインタ参照。写像の具体値・採用理由は spec 側の管理事項であり、本コメントへは
    /// 転記しない。spec-confidentiality.md 参照）。TASK-152 で単一真実源化した
    /// [`crate::error_format::ErrorClass`] へ委譲する（既存の返値は 1 つも変えない）。
    pub fn wire_code(&self) -> &'static str {
        crate::error_format::ClassifiedError::wire_code(self)
    }
}

/// TASK-152（ERR-2）: `wire_code` 写像の単一真実源 [`crate::error_format::ErrorClass`]
/// へ委譲する。variant → `ErrorClass` の対応は既存 `wire_code()` の返値と 1:1 で
/// 一致させ、委譲化で応答コードを変えない。
impl crate::error_format::ClassifiedError for TenantWriteError {
    fn error_class(&self) -> crate::error_format::ErrorClass {
        use crate::error_format::ErrorClass;
        match self {
            TenantWriteError::Forbidden => ErrorClass::ForbiddenTenantMismatch,
            TenantWriteError::NotFound => ErrorClass::RowNotFound,
            TenantWriteError::IdConflict => ErrorClass::UniqueViolation,
            TenantWriteError::DuplicateOperationId => ErrorClass::UniqueViolation,
            TenantWriteError::MissingOperationId => ErrorClass::MissingOperationId,
            TenantWriteError::Catalog(_)
            | TenantWriteError::Storage(_)
            | TenantWriteError::LedgerCorrupted(_) => ErrorClass::InternalError,
            TenantWriteError::OperationIdContentMismatch => ErrorClass::OperationIdContentMismatch,
        }
    }

    /// `Display` は既にテナント境界の秘匿契約（テナント ID・行 id・テーブル名を
    /// 含めない。上記 struct doc 参照）を満たしているため、そのまま返す。
    fn client_message(&self) -> String {
        self.to_string()
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
            // 行キー衝突（`IdConflict`）とは別の固定文言にすることで、クライアントが
            // 「`operation_id` の重複拒否＝先行実行が commit 済み」（RECOVER-7 が使う
            // 判定）を行キー衝突と取り違えないようにする（TASK-94・RECOVER-3・
            // TASK-101・RECOVER-10）。
            TenantWriteError::DuplicateOperationId => {
                write!(f, "operation_id already recorded with the same content")
            }
            TenantWriteError::OperationIdContentMismatch => {
                write!(f, "operation_id already recorded with different content")
            }
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
            TenantWriteError::DuplicateOperationId => f.write_str("DuplicateOperationId"),
            TenantWriteError::MissingOperationId => f.write_str("MissingOperationId"),
            TenantWriteError::Catalog(_) => f.write_str("Catalog(<redacted>)"),
            TenantWriteError::Storage(_) => f.write_str("Storage(<redacted>)"),
            TenantWriteError::LedgerCorrupted(_) => f.write_str("LedgerCorrupted(<redacted>)"),
            TenantWriteError::OperationIdContentMismatch => {
                f.write_str("OperationIdContentMismatch")
            }
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

/// `ledger::record_in_txn`（TASK-101・RECOVER-10）の結果を `TenantWriteError` へ写像
/// する。呼び出し元 6 箇所（本ファイル `*_unchecked`）が `?` で自然に変換できるように
/// する。
impl From<LedgerRecordError> for TenantWriteError {
    fn from(e: LedgerRecordError) -> Self {
        match e {
            LedgerRecordError::Corrupted(storage_err) => {
                TenantWriteError::LedgerCorrupted(storage_err)
            }
            LedgerRecordError::Duplicate => TenantWriteError::DuplicateOperationId,
            LedgerRecordError::ContentMismatch => TenantWriteError::OperationIdContentMismatch,
        }
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
        // 台帳照合用ハッシュ（TASK-101・RECOVER-10）はクライアント要求由来の内容
        // （id・行データ）のみから計算する（DB 状態に依存しない決定性の担保。
        // `content_hash` モジュールドキュメント参照）。同一 write トランザクション内で
        // 即座に判定する（TOCTOU なし。redb 単一ライタ直列化により、この
        // get→insert→判定がそのまま「トランザクション内再確認」になる）。`Err` の場合は
        // 行の書き込みへ進まず、この後 `write_txn` が commit されない（呼び出し元の `?`
        // で早期 return → drop）ため台帳追記も破棄され、部分書き込みが残らない
        // （fail-closed。TASK-94・RECOVER-3 の原子性契約を包含する）。
        let content_hash = content_hash::for_insert(id, row)?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // 物理キーはサーバー側導出テナントで名前空間化する（TABLE-12・RLS-9）。
        let key = (ctx.tenant_id(), id);
        let encoded = encode_row(row)?;
        insert_unique_row(&mut row_table, key, encoded.as_slice())?;
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
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
        // バッチ全体で 1 ハッシュ（TASK-101・RECOVER-10。`content_hash` モジュール
        // ドキュメント参照。要求記載順を含めて連結する）。`Err` の場合は行の書き込みへ
        // 進まず、この後 `write_txn` が commit されない（呼び出し元の `?` で早期
        // return → drop）ため台帳追記も破棄され、部分書き込みが残らない（fail-closed。
        // TASK-94・RECOVER-3 の原子性契約を包含する）。
        let content_hash = content_hash::for_insert_batch(rows)?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;
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
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
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
        // 型付き挿入も行形 INSERT と同じ「新規挿入」操作としてハッシュ化する
        // （TASK-101・RECOVER-10。`content_hash::for_typed_insert` ドキュメント参照）。
        // `Err` の場合は行の書き込みへ進まず、`write_txn` が commit されない（早期
        // return → drop）ため台帳追記も破棄される（fail-closed。TASK-94・RECOVER-3
        // の原子性契約を包含する）。
        //
        // ハッシュ入力には `values`（`schema.columns.len()` 幅・位置インデックス
        // 基準の配列。`sql::parser::bind_insert` が構築）をそのまま渡さず、非
        // VECTOR 列を列名付きペアへ変換してから渡す（cursor bugbot 指摘・PR #248。
        // `content_hash::push_named_scalar_columns` ドキュメント参照。位置基準の
        // ままだと `ALTER TABLE ADD COLUMN` を挟んだ再送で配列幅がずれ、内容一致の
        // 再送が `22023` に誤判定される）。
        let named_columns: Vec<(&str, &crate::row_codec::Value)> = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(idx, column)| {
                *idx != vector_idx && !matches!(column.ty, crate::catalog::ColumnType::Vector(_))
            })
            .filter_map(|(idx, column)| values.get(idx).map(|value| (column.name.as_str(), value)))
            .collect();
        let content_hash =
            content_hash::for_typed_insert(id, visibility, &embedding, &named_columns)?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        let key = (ctx.tenant_id(), id);
        let encoded = encode_row(&row)?;
        insert_unique_row(&mut row_table, key, encoded.as_slice())?;
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
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
/// `ledger`（TASK-93・RECOVER-2、TASK-101・RECOVER-10）: 台帳照合・追記を所有権判定
/// （`owns_existing`。`NotFound` 判定）より**前**に行う（TASK-93 時点の元設計から
/// TASK-101 で反転）。commit 済み操作の再送は行状態が既に変化済み（削除済み行の
/// 再更新等）のことがあり、所有権判定を先に行うと `NotFound`（`P0002`）が返って
/// しまい、ハッシュ一致による再送検知（`23505`）に到達できない。台帳照合を先行
/// させることで、再送検知が行状態の変化に左右されなくなる。「失敗した書き込みは
/// 台帳へ残らない」不変条件は、エラー時に write トランザクションが commit されず
/// drop（abort）される既存契約でそのまま保たれる（台帳照合が先でも、後続で
/// `NotFound` を返せば同じ txn 内の台帳挿入も一緒に破棄される）。
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
        let content_hash = content_hash::for_update(id, row)?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

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
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
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
/// `ledger`（TASK-93・RECOVER-2、TASK-94・RECOVER-3、TASK-101・RECOVER-10）:
/// [`update_row_unchecked`] と同じく、台帳照合・追記を所有権判定（`owns_existing`）
/// より**前**に行う。DELETE は「対象行を消す」副作用が 1 回目の commit で完了する
/// ため、同一 `operation_id` の 2 回目以降の再送は対象行が既に不存在
/// （`owns_existing == false`）になっている。所有権判定を先に見て `NotFound` を
/// 返すと、この正当な重複再送がハッシュ一致による再送検知（`DuplicateOperationId`・
/// `23505`）ではなく `NotFound` として観測され、RECOVER-3 の「同一 `operation_id` の
/// 2 回目以降は重複として拒否する」契約を壊す（codex-review P1 指摘・PR #247）。
/// 台帳照合を先に行うことで、「未使用の `operation_id` で対象行が不存在」の通常
/// ケースは `NotFound` のまま維持しつつ（台帳への tentative 追記はこの後の早期
/// `return` で `write_txn` が commit されず破棄されるため、副作用として残らない）、
/// 「使用済みの `operation_id` を対象行削除後に再送」のケースを
/// `DuplicateOperationId`（内容一致）・`OperationIdContentMismatch`（内容不一致）
/// として区別する。
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
        // 削除要求のクライアント由来の内容は id のみ（`content_hash::for_delete`
        // ドキュメント参照）。
        let content_hash = content_hash::for_delete(id);
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

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
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
    Ok(())
}

/// [`replace_typed_rows_by_text_key`] の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceOutcome {
    /// テキスト列 `key_column` が `key_value` に一致した既存行のうち削除した件数。
    pub removed: usize,
    /// 新規挿入した行数（`rows.len()` と一致する）。
    pub inserted: usize,
    /// 新規挿入した行のうち最小の `id`（`rows` が空なら `None`）。
    pub first_id: Option<u64>,
}

/// テキスト列 `key_column` の値が `key_value` に一致するテナント内の既存行を
/// すべて削除し、代わりに `rows` を新規挿入する（TASK-120・対象ビヘイビア:
/// INDEX-1, INDEX-2。ファイル形 `INSERT` の同一パス再送時の置換セマンティクスの
/// 決定を担う。ポインタ: `docs/design/resend-semantics.md`）。
///
/// `crate::incremental::index_file` から、チャンク化・埋め込み計算をすべて終えた
/// 後にのみ呼ばれる（write トランザクションの外で外部 I/O・CPU 計算を終えてから
/// 単一の write トランザクションへ入る設計。coding-rust.md「不安全な設計 / DoS」:
/// 単一ライタの長時間占有を避ける）。
///
/// - 削除・採番・挿入・世代更新をすべて単一の write トランザクション内で行う
///   （途中失敗は `redb::WriteTransaction` の drop 契約により abort。副作用ゼロ）
/// - 走査範囲はテナント名前空間 `(ctx.tenant_id(), ..)` に限定する（TABLE-12・RLS-9。
///   他テナントの同一 `key_value` 行は走査対象にも削除対象にも含まれない）
/// - 新規行の `id` は同一テナント名前空間内の既存最大 `id`（削除対象・対象外を問わず、
///   走査中に観測した全行）+ 1 から連番で採番する（[`insert_row`] の「既存行を
///   上書きしない」契約と衝突しない。`checked_add` でオーバーフローを `Err` に倒す）
/// - `rows` が空かつ削除対象も 0 件なら世代を進めずに成功する（[`insert_rows`] の
///   空バッチと同じ扱い。無変更コミットで既存インデックスを不要に失効させない）
/// - テナント名前空間内の走査は `visible_rows` と同じ上限（[`MAX_SCANNED_ROWS`]・
///   [`MAX_VISIBLE_ROWS`]）を適用し、超過時は副作用ゼロで `Err`。各行は
///   `crate::storage::decode_row_metadata_borrowed` で `metadata`（スカラー列
///   ペイロード）のみを借用取得し、比較に不要な embedding は確保しない
///   （coding-rust.md「不安全な設計 / DoS」対応）
///
/// エラー契約は [`insert_row`]/[`delete_row`] と同一（`TenantWriteError`。他テナントの
/// 存在情報を漏らさない fail-closed）。`key_column` がスキーマに存在しない・
/// テーブルに VECTOR 列がない場合は [`TenantWriteError::Catalog`]（`CatalogError::Invalid`）。
///
/// 可視性: `operation_id` 必須化ガード（TASK-92・RECOVER-1）を自身では適用しない
/// 内部結線用 API のため `pub(crate)` に閉じる（ガードは唯一の到達経路である
/// `core::EngineCore::execute_insert_sql` が `sql::allowlist::validate_insert` 経由で
/// 書き込み前に適用済み）。クレート外へ公開するとガードを迂回する書き込み入口に
/// なる（codex-review P1 指摘・PR #221。security.md P0）。
/// [`replace_typed_rows_by_text_key`] の入力一式（引数の取り違えを型で防ぎ、
/// 引数個数を抑えるためのまとまり。`incremental::index_file` が構築する）。
pub(crate) struct ReplaceByTextKey<'a> {
    pub table: &'a str,
    /// 置換キーにするテキスト列名（ファイル形 `INSERT` では常に `path`）。
    pub key_column: &'a str,
    pub key_value: &'a str,
    pub visibility: crate::storage::Visibility,
    pub rows: &'a [Vec<crate::row_codec::Value>],
    /// 内容照合ハッシュ（TASK-101・RECOVER-10）専用の raw クライアント要求。
    /// `rows`（チャンク化・埋め込み後の派生行データ）とは意図的に分離する
    /// （codex-review P1 指摘・PR #248。`content_hash::for_replace_by_text_key`
    /// ドキュメント参照）。
    pub content_hash_path: &'a str,
    pub content_hash_body: &'a str,
    pub content_hash_template_values: &'a [crate::row_codec::Value],
    /// 台帳への記録指示（TASK-93・RECOVER-2）。行の削除・挿入と同一 write
    /// トランザクション内で適用される。
    pub ledger_write: LedgerWrite<'a>,
}

pub(crate) fn replace_typed_rows_by_text_key(
    storage: &Storage,
    ctx: &PolicyContext,
    req: ReplaceByTextKey<'_>,
) -> Result<ReplaceOutcome, TenantWriteError> {
    let ReplaceByTextKey {
        table,
        key_column,
        key_value,
        visibility,
        rows,
        content_hash_path,
        content_hash_body,
        content_hash_template_values,
        ledger_write,
    } = req;
    validate_identifier(table)?;
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    // `row_table` の借用（`write_txn.open_table(..)`）をこのブロック内に閉じ込め、
    // ブロックを抜けた後に `write_txn` を（成功なら commit、無変更なら drop で
    // abort）自由に扱えるようにする（`insert_rows` の空バッチ早期 return と異なり、
    // 「削除対象 0 件」は行を走査するまで判定できないため、走査後に判定する）。
    let outcome: Result<ReplaceOutcome, TenantWriteError> = (|| {
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
        let key_idx = schema
            .columns
            .iter()
            .position(|c| c.name == key_column)
            .ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(format!(
                    "unknown key column: {key_column}"
                )))
            })?;

        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;

        let tenant = ctx.tenant_id();
        // テナント名前空間内の既存行を走査し、削除対象 id・既存最大 id（削除対象・
        // 対象外を問わない）を同時に収集する。`range` の借用は本ブロックで閉じ、
        // 後続の `remove`/`insert`（`&mut` 借用）と衝突しないようにする
        // （`update_row`/`delete_row` の `AccessGuard` スコープと同じ方針）。
        let mut to_remove: Vec<u64> = Vec::new();
        let mut max_id: Option<u64> = None;
        let mut scanned_count: usize = 0;
        {
            let start = std::ops::Bound::Included((tenant, 0u64));
            let end = std::ops::Bound::Included((tenant, u64::MAX));
            let mut iter = row_table
                .range::<(&str, u64)>((start, end))
                .map_err(CatalogError::from)?;
            for entry in &mut iter {
                let (k, v) = entry.map_err(CatalogError::from)?;
                let (_key_tenant, id) = k.value();
                let raw = v.value();
                scanned_count = scanned_count.saturating_add(1);
                if scanned_count > MAX_SCANNED_ROWS {
                    return Err(TenantWriteError::Storage(StorageError::Codec(format!(
                        "too many rows scanned for replace: max {MAX_SCANNED_ROWS}"
                    ))));
                }
                // embedding は比較に不要なため、metadata（スカラー列ペイロード）のみを
                // 借用で取り出す（`decode_row` は行ごとに `Vec<f32>` を確保するため、
                // テナント全行走査のホットパスでは使わない。`storage.rs`
                // `decode_row_metadata_borrowed` モジュールドキュメント参照。
                // coding-rust.md「不安全な設計 / DoS」対応）。
                let metadata = crate::storage::decode_row_metadata_borrowed(raw)
                    .map_err(TenantWriteError::Storage)?;
                max_id = Some(max_id.map_or(id, |m: u64| m.max(id)));
                let scanned = crate::row_codec::scan_scalar_columns(&schema, metadata)
                    .map_err(|e| TenantWriteError::Storage(StorageError::Codec(e.to_string())))?;
                if scanned.get(key_idx).copied().flatten() == Some(key_value) {
                    to_remove.push(id);
                    if to_remove.len() > MAX_VISIBLE_ROWS {
                        return Err(TenantWriteError::Storage(StorageError::Codec(format!(
                            "too many matching rows for replace: max {MAX_VISIBLE_ROWS}"
                        ))));
                    }
                }
            }
        }

        let removed = to_remove.len();
        if removed == 0 && rows.is_empty() {
            // 変更ゼロ。`insert_rows` の空バッチと同じく世代を進めずに成功する
            // （呼び出し元が commit せず drop することを、戻り値の 0/0/None から判断する）。
            return Ok(ReplaceOutcome {
                removed: 0,
                inserted: 0,
                first_id: None,
            });
        }

        // 台帳記録は行の削除・挿入と同一の write トランザクション内で行う
        // （TASK-93・RECOVER-2。`insert_typed_row_unchecked` と同型。失敗すれば
        // トランザクションごと abort し、行変更も台帳も残さない）。ハッシュ入力は
        // 要求由来フィールド（`key_column`・`key_value`・`visibility`・
        // `content_hash_path`・`content_hash_body`・`content_hash_template_values`）
        // のみ（TASK-101・RECOVER-10。削除対象集合・採番 id 等の DB 状態由来の値に加え、
        // チャンク化・埋め込み後の派生行データ（`rows`）も含めない。
        // `content_hash::for_replace_by_text_key` ドキュメント参照）。同一内容の再送は
        // `23505`、内容不一致は `22023` へ写像される（呼び出し元の共通 `TenantWriteError`
        // 契約に従う。行形 `INSERT` 経路と同じ扱い。TASK-94・RECOVER-3 の重複拒否契約を
        // 包含する）。
        //
        // `content_hash_template_values`（`schema.columns.len()` 幅・位置インデックス
        // 基準の配列。`sql::parser::bind_file_insert` が構築）をそのまま渡さず、
        // 列名付きペアへ変換してから渡す（cursor bugbot 指摘・PR #248。
        // `content_hash::push_named_scalar_columns` ドキュメント参照。`insert_typed_row_unchecked`
        // と同じ理由: 位置基準のままだと `ALTER TABLE ADD COLUMN` を挟んだ再送で
        // 配列幅がずれ、内容一致の再送が `22023` に誤判定される）。
        let named_template_columns: Vec<(&str, &crate::row_codec::Value)> = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(idx, column)| {
                *idx != vector_idx && !matches!(column.ty, crate::catalog::ColumnType::Vector(_))
            })
            .filter_map(|(idx, column)| {
                content_hash_template_values
                    .get(idx)
                    .map(|value| (column.name.as_str(), value))
            })
            .collect();
        let content_hash = content_hash::for_replace_by_text_key(
            key_column,
            key_value,
            visibility,
            content_hash_path,
            content_hash_body,
            &named_template_columns,
        )?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

        for id in &to_remove {
            row_table
                .remove(&(tenant, *id))
                .map_err(CatalogError::from)?;
        }

        let mut next_id = max_id.map_or(Ok(0u64), |m| {
            m.checked_add(1).ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(
                    "id namespace exhausted".to_string(),
                ))
            })
        })?;
        let mut first_id: Option<u64> = None;
        let mut inserted = 0usize;
        for values in rows {
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
            let id = next_id;
            let key = (ctx.tenant_id(), id);
            // 上の採番規則により既存行との衝突は起こらないはずだが、`insert_row` と
            // 同じ防御（TOCTOU 対策の単一 write トランザクション内チェック）を残す。
            if row_table.get(&key).map_err(CatalogError::from)?.is_some() {
                return Err(TenantWriteError::IdConflict);
            }
            let encoded = encode_row(&row)?;
            row_table
                .insert(key, encoded.as_slice())
                .map_err(CatalogError::from)?;
            if first_id.is_none() {
                first_id = Some(id);
            }
            inserted += 1;
            next_id = id.checked_add(1).ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(
                    "id namespace exhausted".to_string(),
                ))
            })?;
        }

        Ok(ReplaceOutcome {
            removed,
            inserted,
            first_id,
        })
    })();
    let outcome = outcome?;
    if outcome.removed == 0 && outcome.inserted == 0 {
        drop(write_txn);
        return Ok(outcome);
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
    Ok(outcome)
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

/// `table` の最終 commit 済み `operation_id` を照会する（TASK-98、対象ビヘイビア:
/// RECOVER-7。契約の詳細は spec 参照）。`crate::core::EngineCore::last_operation_id`
/// からの薄い委譲先。[`operation_recorded`] と同じ理由で `pub(crate)` に限定する:
/// `ledger_mode` の `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）判定は
/// `EngineCore::last_operation_id` 側が担い、本関数はモード非依存の生の照会結果
/// （[`ledger::LastOperationRaw`]。詳細は `recovery::ledger` モジュールドキュメント
/// 参照。codex-review P1 指摘対応）のみを返す。この区別をクレート外から迂回できない
/// よう `pub(crate)` に留める。
///
/// 照会範囲は呼び出し元テナント（`ctx.tenant_id()`）の名前空間に閉じる（TABLE-12・
/// RLS-9 と同型。security.md P0）。
pub(crate) fn last_operation(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
) -> Result<ledger::LastOperationRaw, TenantWriteError> {
    validate_identifier(table)?;
    let read_txn = storage.db().begin_read().map_err(CatalogError::from)?;
    ledger::last_operation_in_read_txn(&read_txn, ctx.tenant_id(), table)
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

    // --- replace_typed_rows_by_text_key（TASK-120・対象ビヘイビア: INDEX-1, INDEX-2） --

    fn file_schema(table: &str) -> TableSchema {
        TableSchema::new(
            table,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    fn row_values(embedding: [f32; 2], path: &str, body: &str) -> Vec<crate::row_codec::Value> {
        vec![
            crate::row_codec::Value::Vector(embedding.to_vec()),
            crate::row_codec::Value::Text(path.to_string()),
            crate::row_codec::Value::Text(body.to_string()),
        ]
    }

    #[test]
    fn replace_same_path_replaces_rows_and_leaves_other_paths_untouched() {
        let path = unique_db_path("replace-same-path");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "other.txt",
                visibility: Visibility::Private,
                rows: &[row_values([9.0, 9.0], "other.txt", "unrelated")],
                content_hash_path: "other.txt",
                content_hash_body: "unrelated",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("seed other path");

        let first = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "note.txt",
                visibility: Visibility::Private,
                rows: &[row_values([1.0, 0.0], "note.txt", "v1 chunk a")],
                content_hash_path: "note.txt",
                content_hash_body: "v1 body",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("first replace should succeed");
        assert_eq!(first.removed, 0);
        assert_eq!(first.inserted, 1);
        assert!(first.first_id.is_some());

        let second = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "note.txt",
                visibility: Visibility::Private,
                rows: &[
                    row_values([2.0, 0.0], "note.txt", "v2 chunk a"),
                    row_values([2.0, 1.0], "note.txt", "v2 chunk b"),
                ],
                content_hash_path: "note.txt",
                content_hash_body: "v2 body",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("second replace should succeed");
        assert_eq!(second.removed, 1);
        assert_eq!(second.inserted, 2);
        // 採番は既存最大 id + 1 から連番であり、他パス・旧チャンクの id と衝突しない。
        assert!(second.first_id.unwrap() > first.first_id.unwrap());

        // 他パスは無変更。
        let visible_ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let rows = visible_rows(&storage, "docs", &visible_ctx).expect("visible rows");
        let bodies: Vec<&str> = rows
            .iter()
            .map(|r| {
                let scanned =
                    crate::row_codec::scan_scalar_columns(&file_schema("docs"), &r.metadata)
                        .expect("scan scalar columns");
                scanned.get(2).copied().flatten().unwrap_or("")
            })
            .collect();
        assert_eq!(rows.len(), 3);
        assert!(bodies.contains(&"unrelated"));
        assert!(bodies.contains(&"v2 chunk a"));
        assert!(bodies.contains(&"v2 chunk b"));
        assert!(!bodies.contains(&"v1 chunk a"));
    }

    #[test]
    fn replace_does_not_touch_other_tenants_same_path_rows() {
        let path = unique_db_path("replace-tenant-isolation");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

        replace_typed_rows_by_text_key(
            &storage,
            &ctx_b,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "shared.txt",
                visibility: Visibility::Private,
                rows: &[row_values([1.0, 1.0], "shared.txt", "tenant-b content")],
                content_hash_path: "shared.txt",
                content_hash_body: "tenant-b content",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("tenant-b seed should succeed");

        replace_typed_rows_by_text_key(
            &storage,
            &ctx_a,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "shared.txt",
                visibility: Visibility::Private,
                rows: &[row_values([2.0, 2.0], "shared.txt", "tenant-a content")],
                content_hash_path: "shared.txt",
                content_hash_body: "tenant-a content",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("tenant-a replace should succeed");

        let visible_ctx_b =
            PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let rows_b = visible_rows(&storage, "docs", &visible_ctx_b).expect("visible rows");
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_b[0].tenant_id, "tenant-b");
    }

    #[test]
    fn replace_empty_rows_and_no_match_does_not_bump_generation() {
        let path = unique_db_path("replace-empty-noop");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let before = storage.current_generation().expect("read generation");
        let outcome = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "absent.txt",
                visibility: Visibility::Private,
                rows: &[],
                content_hash_path: "absent.txt",
                content_hash_body: "",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("no-op replace should succeed");
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.inserted, 0);
        assert_eq!(outcome.first_id, None);
        let after = storage.current_generation().expect("read generation");
        assert_eq!(before, after);
    }

    #[test]
    fn replace_rejects_table_without_vector_column() {
        let path = unique_db_path("replace-no-vector-column");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("path", ColumnType::Text, false)],
            ))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let err = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "a.txt",
                visibility: Visibility::Private,
                rows: &[vec![crate::row_codec::Value::Text("a.txt".to_string())]],
                content_hash_path: "a.txt",
                content_hash_body: "body",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TenantWriteError::Catalog(CatalogError::Invalid(_))
        ));
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
        // TASK-101（RECOVER-10）: 台帳照合は operation_id 単位でハッシュを持つため、
        // 同一 operation_id を別内容の書き込みへ使い回すと本テストの意図（行 id 衝突の
        // 検証）より先に `OperationIdContentMismatch` を検出してしまう。行 id 衝突を
        // 単独で検証するため、シード投入とバッチ投入で別々の operation_id を使う。
        let seed_op_id = OperationId::parse("test-op-seed").expect("valid operation_id");
        let batch_op_id = OperationId::parse("test-op-batch").expect("valid operation_id");

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
            &seed_op_id,
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
        let err = insert_rows(&storage, "docs", &a, &batch, &batch_op_id)
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

    // codex-review P1 再指摘（PR #266）「新設する場合は書き込み経路での更新漏れが
    // ないことをテストで担保」対応: `bump_table_generation_in_txn` を呼ぶすべての
    // テナント境界付き書き込み API（`insert_row`・`insert_rows`・`insert_typed_row`・
    // `update_row`・`delete_row`・`replace_typed_rows_by_text_key`）が対象テーブル
    // （`docs`）の世代を実際に進めること、かつ無関係な別テーブル（`sibling`）・
    // 同一テーブルへの他テナント（`tenant-b`）の書き込みには影響を与えないことを
    // 固定する（`catalog.rs` 側の DDL・生の書き込み API は
    // `catalog_write_apis_bump_only_the_written_tables_generation` で別途カバーする。
    // `docs` への他テナント書き込みは意図的に「無関係」扱いしない設計判断＝
    // `core.rs` `Statement::Select` アームの `USING PLAN` 世代照合コメント参照）。
    #[test]
    fn write_apis_bump_only_the_written_tables_generation() {
        let path = unique_db_path("table-generation-bump-coverage-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema("docs"))
            .expect("create table docs");
        storage
            .create_table(&schema("sibling"))
            .expect("create table sibling");

        let read_gen = |name: &str| -> u64 {
            let read_txn = storage.db().begin_read().expect("begin read");
            crate::catalog::table_generation_in_txn(&read_txn, name).expect("read table generation")
        };

        let a = PolicyContext::new("tenant-a").expect("valid tenant");
        let sibling_gen = read_gen("sibling");
        let mut prev = read_gen("docs");

        let op = |suffix: &str| OperationId::parse(suffix).expect("valid operation_id");

        insert_row(
            &storage,
            "docs",
            &a,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: b"one",
            },
            &op("bump-insert-row"),
        )
        .expect("insert_row");
        let next = read_gen("docs");
        assert!(next > prev, "insert_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        insert_rows(
            &storage,
            "docs",
            &a,
            &[(
                2,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: b"two",
                },
            )],
            &op("bump-insert-rows"),
        )
        .expect("insert_rows (non-empty)");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "insert_rows (non-empty) must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        // 空バッチは commit 自体を行わない既存契約（`insert_rows_unchecked` の
        // ドキュメントコメント参照）のとおり、世代を進めない。
        insert_rows(&storage, "docs", &a, &[], &op("bump-insert-rows-empty"))
            .expect("insert_rows (empty)");
        assert_eq!(
            read_gen("docs"),
            prev,
            "insert_rows with an empty batch must not bump the generation"
        );
        assert_eq!(read_gen("sibling"), sibling_gen);

        insert_typed_row(
            &storage,
            "docs",
            &a,
            3,
            Visibility::Public,
            &[crate::row_codec::Value::Vector(vec![0.2, 0.3])],
            &op("bump-insert-typed-row"),
        )
        .expect("insert_typed_row");
        let next = read_gen("docs");
        assert!(next > prev, "insert_typed_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        update_row(
            &storage,
            "docs",
            &a,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[9.0, 9.0],
                metadata: b"one-updated",
            },
            &op("bump-update-row"),
        )
        .expect("update_row");
        let next = read_gen("docs");
        assert!(next > prev, "update_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        delete_row(&storage, "docs", &a, 2, &op("bump-delete-row")).expect("delete_row");
        let next = read_gen("docs");
        assert!(next > prev, "delete_row must bump docs' generation");
        assert_eq!(read_gen("sibling"), sibling_gen);

        let file_docs_gen_before_replace = read_gen("docs");
        replace_typed_rows_by_text_key(
            &storage,
            &a,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "nonexistent-key",
                visibility: Visibility::Private,
                rows: &[],
                content_hash_path: "irrelevant",
                content_hash_body: "",
                content_hash_template_values: &[],
                ledger_write: LedgerWrite::Disabled,
            },
        )
        // `schema("docs")` は `path` 列を持たないため、`key_column` 探索が
        // 見つからない列として `Err(Invalid)` を返す。これは意図的（`docs` は
        // 埋め込み専用スキーマのため）で、本テストの関心は「世代を進めないこと」
        // のみなので `Err` を許容し、世代不変のみ確認する。
        .ok();
        assert_eq!(
            read_gen("docs"),
            file_docs_gen_before_replace,
            "a rejected replace_typed_rows_by_text_key call must not bump the generation"
        );

        // `replace_typed_rows_by_text_key` の実変更経路は
        // `path`/`body` 列を持つ別テーブル（`file_schema` 相当）で確認する
        // （`replace_same_path_replaces_rows_and_leaves_other_paths_untouched` と
        // 同型のスキーマ）。
        let file_table = "docs_file";
        storage
            .create_table(&file_schema(file_table))
            .expect("create file-shaped table");
        let file_gen_before = read_gen(file_table);
        replace_typed_rows_by_text_key(
            &storage,
            &a,
            ReplaceByTextKey {
                table: file_table,
                key_column: "path",
                key_value: "note.txt",
                visibility: Visibility::Private,
                rows: &[row_values([1.0, 0.0], "note.txt", "v1")],
                content_hash_path: "note.txt",
                content_hash_body: "v1",
                content_hash_template_values: &[],
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("replace_typed_rows_by_text_key (inserting)");
        let file_gen_after = read_gen(file_table);
        assert!(
            file_gen_after > file_gen_before,
            "replace_typed_rows_by_text_key must bump the target table's generation when it \
             inserts/removes rows"
        );
        // 対象外のテーブル（`docs`・`sibling`）はいずれも無変化。
        assert_eq!(read_gen("docs"), file_docs_gen_before_replace);
        assert_eq!(read_gen("sibling"), sibling_gen);

        // 同一テーブル（`docs`）への他テナント（`tenant-b`）の書き込みは
        // 「無関係」ではなく引き続き `docs` の世代へ影響する（上記コメント参照。
        // `user_rows/{table}` は複数テナントの行を同居させる単一の物理テーブル
        // であり、辞書スナップショットは `tenant::visible_rows` を経由するため
        // 他テナントの可視行の増減が要求元テナントの辞書内容にも影響しうる）。
        let b = PolicyContext::new("tenant-b").expect("valid tenant");
        let docs_gen_before_other_tenant = read_gen("docs");
        insert_row(
            &storage,
            "docs",
            &b,
            100,
            &RowInput {
                tenant_id: "tenant-b",
                visibility: Visibility::Public,
                embedding: &[5.0, 5.0],
                metadata: b"other-tenant",
            },
            &op("bump-other-tenant-insert-row"),
        )
        .expect("insert_row (other tenant, same table)");
        assert!(
            read_gen("docs") > docs_gen_before_other_tenant,
            "a write to the same table by a different tenant must still bump the table's \
             generation (same-table writes are not treated as unrelated)"
        );
        assert_eq!(read_gen("sibling"), sibling_gen);
    }
}
