//! `redb` ベースの永続化層（TASK-140/TASK-141、対象ビヘイビア: PERSIST-1, PERSIST-2,
//! PERSIST-3, PERSIST-4。ポインタ: `docs/spec/05-tasks.md` TASK-140・TASK-141・
//! `docs/spec/04-behavior/persistence.md`）。
//!
//! 責務境界: ベクトル行（id・テナント ID・可視性ラベル・埋め込み・メタデータ）の永続化 API を
//! 提供する。後続の検索カーネル・カタログ層（TASK-124〜、TASK-85〜）から呼び出される想定で、
//! 本モジュールは `tenant_id`・`visibility` をスキーマとして同居保持するが、
//! ポリシー評価（可視性判定・RLS 事前フィルタ）そのものは行わない（評価は TASK-133 以降の
//! 呼び出し元の責務）。呼び出し元がメタデータ列にどのようなバイト列を格納するかは別途決める。
//!
//! 分離レベル（PERSIST-4）: `redb` の契約をそのまま宣言する。書き込みトランザクションは
//! `redb::Database::begin_write` が排他ロックを取ることで直列化され、読み取りトランザクション
//! （`begin_read`）は開始時点のスナップショットを見る（進行中の未コミット書き込みは見えない）。
//! 本モジュールは独自のロック層を追加しない。

use std::fmt;
use std::path::Path;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

/// 電源断シミュレーション用 `redb::StorageBackend` の共通モデル（TASK-145）。
/// 本クレートのユニットテスト（本モジュール下部の `tests::power_loss`）と
/// `crates/engine/tests/power_loss.rs` 統合テストの双方から `#[path]` 経由で
/// 同一ソースを取り込む。テストコードのみで使うため通常ビルドには含めない。
#[cfg(test)]
mod power_loss_model;

/// 行データを格納するテーブル。キーは行 ID（`u64`）、値は [`encode_row`] でエンコードした
/// バイト列。テーブル名は `docs/spec` 側の成果物指定に依存しないローカルな識別子。
///
/// `pub(crate)`: `txn.rs`（TASK-88・TABLE-3）が `Storage` の公開 API を経由せず、
/// 同一テーブルに対する読み取りスナップショット・書き込みトランザクションハンドルを
/// 直接構築するために参照する。
pub(crate) const ROWS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("rows");

/// バッチ台帳テーブル（TASK-90、対象ビヘイビア: TABLE-10。ポインタ:
/// `docs/spec/05-tasks.md` TASK-90・`docs/spec/04-behavior/data-model.md` TABLE-10）。
/// キーはバッチ通番（`batch_seq`、0 起点で連番）、値はそのバッチで [`ROWS_TABLE`] へ
/// 新規挿入した行数。[`ROWS_TABLE`] と同一の `redb::WriteTransaction` 内でのみ書き込む
/// （`txn.rs` の [`crate::txn::BatchWriteTxn::log_batch`]）ことで、2 テーブル横断の
/// トランザクション原子性（同時にコミットされる／同時に破棄される）を製品コード経路で
/// 成立させる。TASK-93（`operation_id` 台帳。ポインタ: `docs/spec/05-tasks.md` TASK-93）と
/// 同型のパターンであり、本テーブル自体はテスト専用の使い捨てではない。
///
/// **契約の適用範囲（重要）**: 「台帳の row_count 合計 == [`ROWS_TABLE`] の行総数」という
/// 不変条件は、[`crate::txn::BatchWriteTxn`] だけを使って [`ROWS_TABLE`] へ書き込んだ場合に
/// のみ [`crate::txn::BatchWriteTxn`] の公開 API（`DuplicateBatchSeq`・`EmptyBatch`・
/// `UnloggedRows`・上書き非カウントの各チェック）によって保証される。[`Storage::put`]・
/// [`Storage::put_batch`]・[`crate::txn::WriteTxn::put`]（バッチ台帳を経由しない別経路）は
/// 台帳を一切更新せず [`ROWS_TABLE`] に直接書き込めるため、これらと
/// [`crate::txn::BatchWriteTxn`] を同一 DB・同一テーブルに対して混在させると、上記の
/// 不変条件は成立しなくなる。これは型システムでは検出できない呼び出し元の責務であり、
/// 本モジュールは意図的にそれを強制しない（PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf
/// 対応。「台帳合計 == 行総数」を保証する範囲を、実際に型で保証できる範囲まで明文化して
/// 限定した）。TABLE-10 の不変条件が必要な呼び出し元は、対象テーブルへの書き込みを
/// [`crate::txn::BatchWriteTxn`] に一本化すること。
pub(crate) const BATCH_LOG_TABLE: TableDefinition<u64, u64> = TableDefinition::new("batch_log");

/// ストレージ全体の書き込み世代カウンタ（TASK-133 P1・対象ビヘイビア: RLS-1〜4）。
/// 単一の固定キー [`GENERATION_KEY`] に対する値（u64）を [`bump_generation_and_commit`]
/// だけが更新する。`crate::rls::PrefilterIndex` が構築後の失効検出に使う。
pub(crate) const GENERATION_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("storage_generation");

/// [`GENERATION_TABLE`] の唯一のキー（ストレージ全体で 1 カウンタ）。
const GENERATION_KEY: &str = "generation";

/// 行エンコーディングの先頭バイト。v2（TASK-141）で RLS フィールド（`tenant_id`・
/// `visibility`）を同居させるレイアウトへ拡張した。v1 の行は RLS フィールドを持たず、
/// 暗黙のデフォルトテナント・可視性で読み出すのは fail-open（P0 違反）になるため、
/// デコード側は v1 を含む未知バージョンを一律 fail-closed に拒否する
/// （マイグレーションは提供しない。実装着手直後で永続データの互換性保証は不要）。
const ROW_FORMAT_VERSION: u8 = 2;

/// `tenant_id` の UTF-8 バイト長上限。デコード時・エンコード時ともこの値を超える
/// 長さフィールドを `Vec`/`String` へのアロケーションに使う前に拒否する
/// （.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
///
/// `pub(crate)`: `arena.rs`（TASK-87、対象ビヘイビア: TABLE-8）が、コールドスタート・
/// アリーナ構築時に `tenant_id` 文字列群の総メモリ量を見積もる上限値として参照する。
pub(crate) const MAX_TENANT_ID_LEN: u16 = 256;

/// 埋め込み次元数の上限。デコード時にこの値を超える `dim` を確認した場合、
/// `Vec::with_capacity` へ渡す前に拒否する（未検証の長さフィールドを無制限アロケーションに
/// 使わない。.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
pub(crate) const MAX_EMBEDDING_DIM: u32 = 65_536;

/// メタデータ列のバイト長上限。埋め込みと同様、デコード前に上限検証する。
const MAX_METADATA_LEN: u32 = 4 * 1024 * 1024;

/// [`Storage::scan_page`] が 1 回の呼び出しで返す行数の上限。
/// [`Storage::scan`] は行数に上限がなく、最大サイズの行（[`MAX_EMBEDDING_DIM`]・
/// [`MAX_METADATA_LEN`] 相当）が大量にある場合に一度に巨大なメモリ確保が発生し得る
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」）。後続タスクで検索カーネル等が
/// 永続化層に依存する際は、この上限付きページングを使う前提とする。
///
/// `pub(crate)`: `catalog.rs`（TASK-146・EXT-1, EXT-2）のテーブルスコープ
/// `scan_table_page` が、ここと同じ行数上限を適用するために再利用する。
pub(crate) const MAX_SCAN_PAGE_LIMIT: u32 = 10_000;

/// [`Storage::scan`] が一度に確保してよい行数の上限（無制限 `Vec` 確保を避けるための
/// 契約。security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。超過時は
/// [`StorageError::ScanLimitExceeded`] で fail-closed に拒否し、呼び出し元へは
/// 上限付きページング API [`Storage::scan_page`] の利用を促す。
const MAX_SCAN_TOTAL_ROWS: usize = 100_000;

/// [`Storage::scan`] が確保してよいデコード対象バイト量（エンコード済みバイト列長で近似）の
/// 上限。[`MAX_SCAN_TOTAL_ROWS`] だけでは最大サイズの行が並んだ場合に巨大確保になり得るため、
/// 行数とバイト量の両方で上限を課す（[`MAX_SCAN_PAGE_BYTES`] と同様の考え方）。
const MAX_SCAN_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// [`Storage::scan_page`] が 1 ページで確保するデコード後バイト量（embedding + metadata）の
/// 上限。行数の上限（[`MAX_SCAN_PAGE_LIMIT`]）だけでは、最大サイズ（[`MAX_EMBEDDING_DIM`]・
/// [`MAX_METADATA_LEN`] 相当）の行が並んだ場合に依然として数十 GB 規模の確保になり得るため、
/// 行数とバイト量の両方で上限を課す。
///
/// `pub(crate)`: [`MAX_SCAN_PAGE_LIMIT`] と同じ理由で `catalog.rs` の
/// `scan_table_page` が再利用する。
pub(crate) const MAX_SCAN_PAGE_BYTES: usize = 16 * 1024 * 1024;

/// [`Storage::scan_batch_log`] が一度に確保してよいエントリ数の上限（[`MAX_SCAN_TOTAL_ROWS`]
/// と同様、無制限 `Vec` 確保を避けるための契約。security.md「不安全な設計｜無制限リソース
/// 確保（DoS）」対応）。バッチ台帳の 1 エントリは固定 16 バイト（`u64` キー + `u64` 値）の
/// ため、この上限だけで確保量が頭打ちになる（[`MAX_SCAN_TOTAL_BYTES`] 相当のバイト上限は
/// 不要）。超過時は [`StorageError::ScanLimitExceeded`] で fail-closed に拒否する
/// （PR #193 codex レビュー PRRT_kwDOUAKASM6cCITT 対応: [`Storage::scan`] 用と variant を
/// 分けない。理由は [`StorageError::ScanLimitExceeded`] のドキュメンテーションコメント参照）。
/// 最大 `batch_seq` だけが必要な採番再開経路はこの上限に依存しない
/// [`Storage::batch_log_max_seq`] を使う。
const MAX_BATCH_LOG_ROWS: usize = 1_000_000;

/// 永続化層の公開エラー型。`redb` の複数のエラー型（`DatabaseError` 等）はすべて
/// `redb::Error` へ変換可能なため、それを内部に保持して一本化する。
/// ライブラリコードとして panic せず、すべての失敗を `Result` で返す
/// （coding-rust.md: engine では `Result` を返し panic させない）。
#[derive(Debug)]
pub enum StorageError {
    /// `redb` 側で発生したエラー（I/O・破損検出・トランザクション競合等）。
    Backend(redb::Error),
    /// 行データのエンコード/デコードで検出した不正値。fail-closed に拒否する
    /// （欠落・不正値を黙殺フォールバックしない）。
    Codec(String),
    /// 指定した行 ID が存在しない。
    NotFound(u64),
    /// [`Storage::scan`] の対象行数・バイト量が上限（[`MAX_SCAN_TOTAL_ROWS`]・
    /// [`MAX_SCAN_TOTAL_BYTES`]）を超過、または [`Storage::scan_batch_log`] のエントリ数が
    /// 上限（[`MAX_BATCH_LOG_ROWS`]）を超過したため fail-closed に拒否した（対象ビヘイビア:
    /// PERSIST-4・TABLE-10）。
    ///
    /// この 2 経路は同一 variant を共有する（Issue #131 で `Storage::scan_batch_log` 側にも
    /// 行テーブル用の代替 API 案内（`scan_page`）が誤って出る問題が見つかったが、専用
    /// variant を新設する案（PR #193 codex レビュー PRRT_kwDOUAKASM6cCITT）・
    /// `#[non_exhaustive]` を付与する案（同 PRRT_kwDOUAKASM6cB6is）はいずれも公開 enum・
    /// 既存の網羅的 match への破壊的変更にあたるとして差し戻された）。
    ///
    /// **[`fmt::Display`] の互換性契約**: 本 variant の `Display` 文字列は既存利用者が
    /// ログ・診断で参照しうる観測可能な契約の一部とみなし、variant 追加・`non_exhaustive`
    /// 化と同様に告知なく削除・改変しない。ただし文字列の**中身**が経路別の代替 API 名
    /// （`scan_page`）を含んでいると `scan_batch_log` 経由でも誤案内になるため（PR #193
    /// codex レビュー PRRT_kwDOUAKASM6cC0IQ 対応）、両経路で共通して成立する中立な文言
    /// （どの API 名にも触れない）に固定する。経路別の正確な代替手段案内（`scan` →
    /// [`Storage::scan_page`]・テーブルスコープ → `catalog.rs` の `scan_table_page`）は、
    /// どの関数を呼んだかを把握している呼び出し元（= 内部コンテキスト）が本 variant を
    /// 変換する際に付加する（`catalog.rs::convert_storage_error` 参照）。
    ScanLimitExceeded,
    /// [`crate::txn::BatchWriteTxn::log_batch`] に既存の `batch_seq` を渡した。`redb` の
    /// `insert` は無条件上書きのため、検出せず通すとバッチ台帳の不変条件
    /// （`batch_seq` ごとに 1 エントリ）が壊れる。呼び出し元の採番バグを fail-closed
    /// に拒否する（security.md「不安全な設計」対応。他テナント情報は含まない）。
    DuplicateBatchSeq(u64),
    /// [`crate::txn::BatchWriteTxn`] の内部カウンタ（直近の `log_batch` 以降に `put` した
    /// 行数）が `u64` を溢れた。実運用では到達し得ない防御的分岐だが、undefined な
    /// ラップアラウンドで台帳の行数契約を壊さないよう checked 演算で明示的に検出する
    /// （coding-rust.md「整数演算は checked_* / saturating_* を使う」対応）。
    PendingRowCountOverflow,
    /// [`crate::txn::BatchWriteTxn`] で新規挿入した行を、直近の `log_batch` 以降まだ台帳へ
    /// 記録しないまま [`crate::txn::BatchWriteTxn::commit`] しようとした。台帳に記録され
    /// ない行が commit で確定すると「台帳の row_count 合計 == 行総数」という TABLE-10 の
    /// 不変条件を [`crate::txn::BatchWriteTxn`] の公開 API だけで壊せるため fail-closed に
    /// 拒否する（security.md「不安全な設計」対応。他テナント情報は含まない）。
    UnloggedRows(u64),
    /// [`crate::txn::BatchWriteTxn::log_batch`] を、直近の呼び出し以降 1 件も
    /// [`crate::txn::BatchWriteTxn::put`]（新規挿入）していない状態で呼んだ。クラッシュ
    /// 検証ツールの検証オラクル（台帳の各エントリ値 == 実際にコミットされたバッチ
    /// サイズ）と食い違うゼロ件エントリを台帳へ残さないよう fail-closed に拒否する。
    EmptyBatch,
    /// [`GENERATION_TABLE`] のカウンタが `u64` を溢れた（到達し得ない防御的分岐。
    /// coding-rust.md「整数演算は checked_*/saturating_* を使う」対応）。
    GenerationCounterOverflow,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Backend(e) => write!(f, "storage backend error: {e}"),
            StorageError::Codec(msg) => write!(f, "row codec error: {msg}"),
            StorageError::NotFound(id) => write!(f, "row not found: id={id}"),
            // `scan`・`scan_batch_log` の 2 経路で共有する中立文言（本 variant の
            // ドキュメンテーションコメント「Display の互換性契約」参照。PR #193 codex
            // レビュー PRRT_kwDOUAKASM6cC0IQ 対応）。経路別の正確な代替手段案内（`scan_page`
            // 等の API 名）は、経路を把握している呼び出し元（`catalog.rs::convert_storage_error`
            // 等）が組み立てる。
            StorageError::ScanLimitExceeded => {
                write!(
                    f,
                    "scan limit exceeded: result too large for a single unpaginated read"
                )
            }
            StorageError::DuplicateBatchSeq(seq) => {
                write!(f, "duplicate batch seq: seq={seq}")
            }
            StorageError::PendingRowCountOverflow => {
                write!(f, "pending row count overflow: split into smaller batches")
            }
            StorageError::UnloggedRows(count) => {
                write!(f, "unlogged rows before commit: count={count}")
            }
            StorageError::EmptyBatch => write!(f, "empty batch: no rows put since last log_batch"),
            StorageError::GenerationCounterOverflow => {
                write!(f, "storage generation counter overflow")
            }
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Backend(e) => Some(e),
            StorageError::Codec(_)
            | StorageError::NotFound(_)
            | StorageError::ScanLimitExceeded
            | StorageError::DuplicateBatchSeq(_)
            | StorageError::PendingRowCountOverflow
            | StorageError::UnloggedRows(_)
            | StorageError::EmptyBatch
            | StorageError::GenerationCounterOverflow => None,
        }
    }
}

// `redb` の各操作（begin_write・open_table・commit 等）はそれぞれ異なるエラー型を返すが、
// すべて `redb::Error` へ変換可能なので、ここで一括して `StorageError` へ橋渡しする。
//
// 設計メモ: この blanket impl（`E: Into<redb::Error>` を満たす任意の型から変換可能）は、
// coherence 制約により `StorageError` へ他の `From` 実装を個別に追加することを妨げる
// （`redb::Error` に変換可能な型は将来的にも本 impl に一元化される）。`StorageError` は
// engine ⇔ wire-server のエラー契約（`wire_code`）の土台となる型のため、`redb` 以外の
// エラー源を追加する際はこの制約を踏まえて設計を見直すこと。
impl<E> From<E> for StorageError
where
    E: Into<redb::Error>,
{
    fn from(e: E) -> Self {
        StorageError::Backend(e.into())
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// 書き込みコミット直前に [`GENERATION_TABLE`] を +1 してからコミットする
/// （TASK-133 P1 対応）。本クレート内の書き込みコミット（`Storage::put`/`put_batch`・
/// `crate::catalog` の DDL/DML・`crate::txn::WriteTxn`/`BatchWriteTxn`）は
/// `write_txn.commit()` を直接呼ばずすべて本関数を経由する。将来の書き込み API 追加も
/// 本関数を呼ぶだけで世代カウントの経路網羅が保たれる。
///
/// テーブル単位ではなくストレージ全体で 1 カウンタにした: `Storage::put`/`put_batch`
/// は対象テーブル名を持たない旧 API のため、テーブル単位にすると経路によって
/// カウンタ更新対象を誤る余地が生まれる（誤りは fail-open に直結する）。単一カウンタは
/// 無関係なテーブルへの書き込みでも既存インデックスを失効させる（過剰拒否）が、
/// fail-closed 方向のため許容する。
pub(crate) fn bump_generation_and_commit(write_txn: redb::WriteTransaction) -> Result<()> {
    {
        let mut gen_table = write_txn.open_table(GENERATION_TABLE)?;
        let current = gen_table
            .get(GENERATION_KEY)?
            .map(|v| v.value())
            .unwrap_or(0);
        let next = current
            .checked_add(1)
            .ok_or(StorageError::GenerationCounterOverflow)?;
        gen_table.insert(GENERATION_KEY, next)?;
    }
    write_txn.commit()?;
    Ok(())
}

/// RLS 相当のテナント境界判定に使う可視性ラベル（対象ビヘイビア: PERSIST-3）。
///
/// 本モジュールはこの値をスキーマとして同居保持するのみで、ポリシー評価（どの値なら
/// 呼び出し元へ返してよいか）は行わない（評価は TASK-133 以降の呼び出し元の責務）。
/// 永続化表現は 1 バイトの固定コードで、デコード時に未知のバイト値を検出した場合は
/// 既知の値へ黙殺フォールバックせず `StorageError::Codec` で拒否する（fail-closed。
/// 「未知値 → Public 扱い」は情報漏えいに直結するため行わない）。
///
/// 詳細は `docs/spec/04-behavior/data-model.md`（TASK-141・PERSIST-3、ポインタ表記）
/// を参照。値の追加・変更は `ROW_FORMAT_VERSION` 更新を伴う破壊的変更として扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// テナント内で広く共有される可視性ラベル。
    Public,
    /// テナント内でも限定共有される可視性ラベル。
    Private,
}

impl Visibility {
    const PUBLIC_BYTE: u8 = 0x01;
    const PRIVATE_BYTE: u8 = 0x02;

    // `pub(crate)`: `row_codec.rs`（TASK-86）が独立フォーマットの行コーデックで
    // 同じ `Visibility` 型を再利用し、未知バイトの `Err` 拒否をそのまま活かすための
    // 最小限の公開範囲拡張（既存の v2 行フォーマット・`Storage` 公開 API 自体は変更しない）。
    pub(crate) fn to_byte(self) -> u8 {
        match self {
            Visibility::Public => Self::PUBLIC_BYTE,
            Visibility::Private => Self::PRIVATE_BYTE,
        }
    }

    pub(crate) fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            Self::PUBLIC_BYTE => Ok(Visibility::Public),
            Self::PRIVATE_BYTE => Ok(Visibility::Private),
            other => Err(StorageError::Codec(format!(
                "unknown visibility byte: {other}"
            ))),
        }
    }
}

/// 永続化予定の行データ（呼び出し側が構築する入力形）。
pub struct RowInput<'a> {
    /// RLS 相当のテナント境界判定に使う不透明な識別子（対象ビヘイビア: PERSIST-3）。
    /// 空文字列はテナント境界判定を曖昧にするため、エンコード時に拒否する（fail-closed）。
    pub tenant_id: &'a str,
    /// RLS 相当の可視性ラベル（対象ビヘイビア: PERSIST-3）。ポリシー評価はこの型の
    /// 呼び出し元（TASK-133 以降）の責務であり、本モジュールは値をそのまま保持する。
    pub visibility: Visibility,
    pub embedding: &'a [f32],
    /// 呼び出し元が定義する不透明なメタデータバイト列（本モジュールは中身を解釈しない）。
    pub metadata: &'a [u8],
}

/// 読み出した行データ（呼び出し側へ返す出力形）。
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u64,
    /// RLS 相当のテナント境界判定に使う不透明な識別子（対象ビヘイビア: PERSIST-3）。
    pub tenant_id: String,
    /// RLS 相当の可視性ラベル（対象ビヘイビア: PERSIST-3）。[`RowInput::visibility`] 参照。
    pub visibility: Visibility,
    pub embedding: Vec<f32>,
    pub metadata: Vec<u8>,
}

/// `redb::Database` を保持する永続化層のハンドル。
///
/// wire-server の接続ハンドラや検索カーネルからは直接ではなく、このハンドルを介して
/// 行データへアクセスする想定（呼び出し元は `Storage` を通じてのみ永続化状態を触る）。
pub struct Storage {
    db: redb::Database,
}

impl Storage {
    /// 指定パスの `redb` データベースを開く。ファイルが存在しなければ新規作成する
    /// （`redb::Database::create` の契約）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = redb::Database::create(path)?;
        Ok(Self { db })
    }

    /// 内部 `redb::Database` ハンドルへの `pub(crate)` アクセサ。
    ///
    /// `txn.rs`（TASK-88・TABLE-3）が宣言済み分離レベルのトランザクション API
    /// （[`Storage::begin_read`](crate::txn) 相当）を実装する際に、`Storage` の外へ
    /// `redb::Database` 型そのものをリークさせずに到達するための最小限の穴。
    /// 公開 API・挙動は変更しない。
    pub(crate) fn db(&self) -> &redb::Database {
        &self.db
    }

    /// 単一行を書き込み、コミットする（対象ビヘイビア: PERSIST-1）。
    pub fn put(&self, id: u64, row: &RowInput<'_>) -> Result<()> {
        let encoded = encode_row(row)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROWS_TABLE)?;
            table.insert(id, encoded.as_slice())?;
        }
        bump_generation_and_commit(write_txn)
    }

    /// 複数行を単一トランザクションで書き込む（対象ビヘイビア: PERSIST-2）。
    /// 空スライスの場合はトランザクションを開かず即座に成功を返す。
    pub fn put_batch(&self, rows: &[(u64, RowInput<'_>)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROWS_TABLE)?;
            for (id, row) in rows {
                let encoded = encode_row(row)?;
                table.insert(*id, encoded.as_slice())?;
            }
        }
        bump_generation_and_commit(write_txn)
    }

    /// 現在の書き込み世代を返す（TASK-133 P1 対応。読み取り専用。1 回の `read_txn` を
    /// 開いて閉じるだけの単純な値取得で、スナップショットは保持しない）。
    /// `crate::rls::PrefilterIndex::build`/`search` が失効検出の前後比較に使う
    /// （安全性は呼び出し元が前後の値を比較することで担保する。本メソッド自体は
    /// 世代値以外の一貫性を何も保証しない）。
    pub(crate) fn current_generation(&self) -> Result<u64> {
        let read_txn = self.db.begin_read()?;
        match read_txn.open_table(GENERATION_TABLE) {
            Ok(t) => Ok(t.get(GENERATION_KEY)?.map(|v| v.value()).unwrap_or(0)),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// 行 ID を指定して 1 行取得する（スナップショット読み取り）。
    pub fn get(&self, id: u64) -> Result<Row> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            // テーブル未作成（1 行も書き込んでいない）は「存在しない」として扱う。
            Err(redb::TableError::TableDoesNotExist(_)) => return Err(StorageError::NotFound(id)),
            Err(e) => return Err(e.into()),
        };
        let guard = table.get(id)?.ok_or(StorageError::NotFound(id))?;
        decode_row(id, guard.value())
    }

    /// 全行をスナップショット読み取りで走査する（対象ビヘイビア: PERSIST-4）。
    ///
    /// 総行数が [`MAX_SCAN_TOTAL_ROWS`]、総バイト量（エンコード済みバイト列長で近似）が
    /// [`MAX_SCAN_TOTAL_BYTES`] のいずれかを超える場合は、部分的な結果を黙って切り詰めず
    /// [`StorageError::ScanLimitExceeded`] で fail-closed に拒否する
    /// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    /// 大規模データベースを扱う呼び出し元（検索カーネル等）は、この上限を前提にせず
    /// 上限付きページング API [`Storage::scan_page`] を使うこと。
    pub fn scan(&self) -> Result<Vec<Row>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        let mut bytes_used: usize = 0;
        for entry in table.iter()? {
            let (k, v) = entry?;
            let raw = v.value();
            check_scan_limits(out.len(), bytes_used, raw.len())?;
            bytes_used = bytes_used.saturating_add(raw.len());
            out.push(decode_row(k.value(), raw)?);
        }
        Ok(out)
    }

    /// 行 ID 昇順で最大 `limit` 件を走査する上限付きページング API（[`scan`](Self::scan) の
    /// 無制限確保を避けるための代替。対象ビヘイビア: PERSIST-4）。
    ///
    /// `after` に前回呼び出しで返した [`Row::id`] の最大値（カーソル）を渡すと、
    /// その ID より大きい行から再開する。初回は `None` を渡す。
    /// 戻り値の第 2 要素は次回呼び出しに渡すべきカーソル（続きがなければ `None`）。
    ///
    /// `limit` は [`MAX_SCAN_PAGE_LIMIT`] で切り詰める（呼び出し元が誤って大きな値を
    /// 渡しても一度の確保が上限を超えないようにする）。`limit == 0` は空ページを返す。
    /// さらに、行数が上限未満でも、ページ内のデコード対象バイト量（embedding + metadata
    /// 相当。エンコード済みバイト列長で近似）が [`MAX_SCAN_PAGE_BYTES`] を超える場合は
    /// その時点でページを打ち切る（最大サイズの行が並んだ場合の巨大確保を避けるため。
    /// 1 行のみで超過する場合はその 1 行を含めて返し、無限ループを避ける）。
    pub fn scan_page(&self, after: Option<u64>, limit: u32) -> Result<(Vec<Row>, Option<u64>)> {
        let limit = limit.min(MAX_SCAN_PAGE_LIMIT) as usize;
        if limit == 0 {
            return Ok((Vec::new(), None));
        }

        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), None)),
            Err(e) => return Err(e.into()),
        };

        // カーソルの直後（`after + 1`）から走査を開始する。`after` が `u64::MAX` の場合は
        // 「これ以上続きがない」ことを意味するため、範囲を作らず空ページを返す。
        let start = match after {
            Some(cursor) => match cursor.checked_add(1) {
                Some(next) => next,
                None => return Ok((Vec::new(), None)),
            },
            None => 0,
        };

        let mut out = Vec::new();
        let mut bytes_used: usize = 0;
        // 行数上限・バイト上限のいずれかで打ち切った場合のみ「続きがあるかもしれない」。
        // イテレータが自然に尽きた（テーブル末尾に到達した）場合は打ち切りではない。
        let mut capped = false;
        for entry in table.range(start..)? {
            if out.len() == limit {
                capped = true;
                break;
            }
            let (k, v) = entry?;
            let id = k.value();
            let raw = v.value();
            if !out.is_empty() && bytes_used.saturating_add(raw.len()) > MAX_SCAN_PAGE_BYTES {
                capped = true;
                break;
            }
            out.push(decode_row(id, raw)?);
            bytes_used = bytes_used.saturating_add(raw.len());
        }

        let cursor_for_next = if capped {
            out.last().map(|r| r.id)
        } else {
            None
        };
        Ok((out, cursor_for_next))
    }

    /// [`BATCH_LOG_TABLE`] の全エントリを `batch_seq` 昇順で読み出す（対象ビヘイビア:
    /// TABLE-10）。再起動後の検証専用の読み取り（採番再開には
    /// [`Storage::batch_log_max_seq`] を使う。全件走査を要する検証オラクル
    /// （`crash_tool_cross_table.rs` の `verify_inner` 等）向け）。
    ///
    /// エントリ数が [`MAX_BATCH_LOG_ROWS`] を超える場合は、[`Storage::scan`] と同様に
    /// 部分的な結果を黙って切り詰めず [`StorageError::ScanLimitExceeded`] で
    /// fail-closed に拒否する（security.md「不安全な設計｜無制限リソース確保（DoS）」
    /// 対応。バッチ台帳はコミットごとに増え続けるため、大きな DB では無制限確保が
    /// メモリ枯渇につながり得る）。台帳にはページング API が無いため、この経路の呼び出し元
    /// は `ScanLimitExceeded` の `Display` が持つ中立文言（どの API 名にも触れない）を
    /// そのまま使う（`scan` 用の `scan_page` 案内を誤って流用しない。
    /// [`StorageError::ScanLimitExceeded`] のドキュメンテーションコメント参照）。
    pub fn scan_batch_log(&self) -> Result<Vec<(u64, u64)>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(BATCH_LOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            check_batch_log_limit(out.len())?;
            let (k, v) = entry?;
            out.push((k.value(), v.value()));
        }
        Ok(out)
    }

    /// [`BATCH_LOG_TABLE`] の最大 `batch_seq` を返す（対象ビヘイビア: TABLE-10）。
    /// 台帳テーブル未作成・空の場合は `Ok(None)`。
    ///
    /// [`Storage::scan_batch_log`] と異なり全エントリを `Vec` へ確保せず、redb の
    /// B-tree 実装が持つ最終キー取得（`ReadableTable::last`）だけを使う（O(log n)・
    /// アロケーションなし）ため、[`MAX_BATCH_LOG_ROWS`] に依存しない。採番再開経路
    /// （`crash_tool_cross_table.rs` の `find_resume_state`）が呼ぶ想定で、台帳件数が
    /// 上限を超えた DB でも再開できるようにするための専用 API（Issue #132）。
    pub fn batch_log_max_seq(&self) -> Result<Option<u64>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(BATCH_LOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // `AccessGuard` は read txn（延いては `table`）を借用したままなので、
        // `u64` へコピーしてから返す（`table`/`read_txn` のスコープを抜けた後に
        // 借用が残る E0597 を避ける）。
        let max_seq = table.last()?.map(|(k, _v)| k.value());
        Ok(max_seq)
    }
}

/// [`Storage::scan`] の行数・バイト量上限判定を切り出した純粋関数（テスト容易性のため）。
/// `rows`・`bytes_used` は走査済み件数・バイト量、`next_len` はこれから加える 1 行の
/// エンコード済みバイト長。超過時は [`StorageError::ScanLimitExceeded`] を返す。
/// 判定条件（`>=` / `>` / `saturating_add`）は既存の `scan` 実装から変更しない。
fn check_scan_limits(rows: usize, bytes_used: usize, next_len: usize) -> Result<()> {
    if rows >= MAX_SCAN_TOTAL_ROWS || bytes_used.saturating_add(next_len) > MAX_SCAN_TOTAL_BYTES {
        return Err(StorageError::ScanLimitExceeded);
    }
    Ok(())
}

/// [`Storage::scan_batch_log`] のエントリ数上限判定を切り出した純粋関数（テスト容易性の
/// ため）。`entries` は走査済みエントリ数。超過時は [`StorageError::ScanLimitExceeded`]
/// を返す（`scan` 用と同一 variant。理由は同 variant のドキュメンテーションコメント参照）。
/// 判定条件は既存の `scan_batch_log` 実装から変更しない。
fn check_batch_log_limit(entries: usize) -> Result<()> {
    if entries >= MAX_BATCH_LOG_ROWS {
        return Err(StorageError::ScanLimitExceeded);
    }
    Ok(())
}

/// 行を固定レイアウトでエンコードする（serde 系依存を増やさない方針。dependency-policy.md）。
///
/// レイアウト（v2、対象ビヘイビア: PERSIST-3）:
/// `[version: u8=2][tenant_len: u16 le][tenant bytes][visibility: u8]`
/// `[dim: u32 le][embedding: dim * f32 le][metadata_len: u32 le][metadata bytes]`
///
/// RLS フィールド（`tenant_id`・`visibility`）を先頭側に置くのは、後続の RLS 事前フィルタ
/// （TASK-133）が embedding をデコードせずテナント判定できる余地を残すため。
/// バージョンバイトと非構造化のメタデータバイト列により、TASK-146（次元固定カタログ）等の
/// 後続スキーマ拡張が非互換変更なしに行えるようにしている。
///
/// `pub(crate)`: `txn.rs`（TASK-88）の書き込みトランザクションハンドルが、`Storage::put`
/// と同一のエンコーディングで行を書き込むために再利用する。
pub(crate) fn encode_row(row: &RowInput<'_>) -> Result<Vec<u8>> {
    if row.tenant_id.is_empty() {
        return Err(StorageError::Codec(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_bytes = row.tenant_id.as_bytes();
    let tenant_len = u16::try_from(tenant_bytes.len()).map_err(|_| {
        StorageError::Codec(format!("tenant_id too long: {} bytes", tenant_bytes.len()))
    })?;
    if tenant_len > MAX_TENANT_ID_LEN {
        return Err(StorageError::Codec(format!(
            "tenant_id length {tenant_len} exceeds limit {MAX_TENANT_ID_LEN}"
        )));
    }

    let dim = u32::try_from(row.embedding.len()).map_err(|_| {
        StorageError::Codec(format!("embedding dim too large: {}", row.embedding.len()))
    })?;
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    let metadata_len = u32::try_from(row.metadata.len())
        .map_err(|_| StorageError::Codec(format!("metadata too large: {}", row.metadata.len())))?;
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }

    let mut buf = Vec::with_capacity(
        1 + 2 + tenant_bytes.len() + 1 + 4 + row.embedding.len() * 4 + 4 + row.metadata.len(),
    );
    buf.push(ROW_FORMAT_VERSION);
    buf.extend_from_slice(&tenant_len.to_le_bytes());
    buf.extend_from_slice(tenant_bytes);
    buf.push(row.visibility.to_byte());
    buf.extend_from_slice(&dim.to_le_bytes());
    for v in row.embedding {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(&metadata_len.to_le_bytes());
    buf.extend_from_slice(row.metadata);
    Ok(buf)
}

/// 行バイト列の先頭（バージョン・`tenant_id`・`visibility`）だけをデコードする
/// [`decode_row`] の共有前段。embedding・metadata のバイト列へは触れないため、
/// それらが破損していても本関数の成否には影響しない。
///
/// 呼び出し文脈: [`decode_row`]（フル デコード）と [`decode_row_tenant_and_visibility`]
/// （ヘッダのみ）の両方がこの関数を呼ぶ。ロジックを 1 箇所に集約することで、
/// 検証条件（`tenant_len` 上限・UTF-8・空文字列拒否等）が両者で食い違わないようにする。
/// 成功時は `(tenant_id, visibility, visibility バイトの直後のオフセット)` を返す。
fn decode_row_header(buf: &[u8]) -> Result<(String, Visibility, usize)> {
    let version = *buf
        .first()
        .ok_or_else(|| StorageError::Codec("row buffer is empty".to_string()))?;
    if version != ROW_FORMAT_VERSION {
        return Err(StorageError::Codec(format!(
            "unsupported row format version: {version}"
        )));
    }
    let mut offset = 1usize;

    let tenant_len_field_end = offset
        .checked_add(2)
        .ok_or_else(|| StorageError::Codec("offset overflow at tenant_len field".to_string()))?;
    let tenant_len_bytes = buf.get(offset..tenant_len_field_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at tenant_len field".to_string())
    })?;
    let tenant_len_arr: [u8; 2] = tenant_len_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("tenant_len field is not 2 bytes".to_string()))?;
    let tenant_len = u16::from_le_bytes(tenant_len_arr);
    if tenant_len > MAX_TENANT_ID_LEN {
        return Err(StorageError::Codec(format!(
            "tenant_id length {tenant_len} exceeds limit {MAX_TENANT_ID_LEN}"
        )));
    }
    offset = offset
        .checked_add(2)
        .ok_or_else(|| StorageError::Codec("offset overflow after tenant_len field".to_string()))?;

    let tenant_end = offset
        .checked_add(tenant_len as usize)
        .ok_or_else(|| StorageError::Codec("offset overflow after tenant_id field".to_string()))?;
    let tenant_bytes = buf.get(offset..tenant_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at tenant_id field".to_string())
    })?;
    if tenant_bytes.is_empty() {
        return Err(StorageError::Codec(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_id = std::str::from_utf8(tenant_bytes)
        .map_err(|_| StorageError::Codec("tenant_id is not valid UTF-8".to_string()))?
        .to_string();
    offset = tenant_end;

    let visibility_byte = *buf.get(offset).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at visibility field".to_string())
    })?;
    let visibility = Visibility::from_byte(visibility_byte)?;
    offset = offset
        .checked_add(1)
        .ok_or_else(|| StorageError::Codec("offset overflow after visibility field".to_string()))?;

    Ok((tenant_id, visibility, offset))
}

/// 行バイト列から `tenant_id`・`visibility` だけを取り出す（[`decode_row_header`] の
/// 薄いラッパー）。embedding・metadata のデコードは行わない。
///
/// 呼び出し文脈: `arena.rs::VectorArena::build_filtered` が、可視性判定（呼び出し元の
/// `predicate`）に必要な最小限の情報だけを、embedding のデコード（次元検証を含む）を
/// 経ずに安全に取得するために使う。これにより、不可視行（他テナント行）の embedding
/// 部分が破損していても、その破損を検出する前に `predicate` で弾いてスキップできる
/// （他テナントのデータ破損状態が対象テナントの検索可用性へ干渉しない設計にする。
/// codex P0 対応・Issue #137）。
///
/// ヘッダ部自体（バージョン・`tenant_len`・`tenant_id`・`visibility`）が破損していて
/// 可視性を判定できない場合は、`decode_row_header` と同じ理由で `Err` を返す
/// （呼び出し元はこの行を「不可視だからスキップ」とは判断できないため fail-closed。
/// `arena.rs` 側のドキュメント参照）。
pub(crate) fn decode_row_tenant_and_visibility(buf: &[u8]) -> Result<(String, Visibility)> {
    let (tenant_id, visibility, _offset_after_header) = decode_row_header(buf)?;
    Ok((tenant_id, visibility))
}

/// [`encode_row`] の逆変換。欠落・不正値はすべて `Err` で拒否する（fail-closed。
/// 黙殺フォールバックで既知の型・デフォルト値へ落とさない）。添字アクセス `[]` ではなく
/// `get()` を使い、境界外アクセスを未定義動作にしない。
///
/// `pub(crate)`: `txn.rs`（TASK-88）の読み取りスナップショットハンドルが、`Storage::get`
/// と同一のデコード・fail-closed 契約で行を読み出すために再利用する。
pub(crate) fn decode_row(id: u64, buf: &[u8]) -> Result<Row> {
    let (tenant_id, visibility, mut offset) = decode_row_header(buf)?;

    let dim_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at dim field".to_string()))?;
    let dim_bytes = buf
        .get(offset..dim_field_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at dim field".to_string()))?;
    let dim_arr: [u8; 4] = dim_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("dim field is not 4 bytes".to_string()))?;
    let dim = u32::from_le_bytes(dim_arr);
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    offset = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow after dim field".to_string()))?;

    let embedding_bytes_len = (dim as usize)
        .checked_mul(4)
        .ok_or_else(|| StorageError::Codec("embedding byte length overflow".to_string()))?;
    let embedding_end = offset
        .checked_add(embedding_bytes_len)
        .ok_or_else(|| StorageError::Codec("offset overflow after embedding field".to_string()))?;
    let embedding_bytes = buf.get(offset..embedding_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at embedding field".to_string())
    })?;
    // 上限検証済みの dim に基づくため、無制限確保にはならない。
    let mut embedding = Vec::with_capacity(dim as usize);
    for chunk in embedding_bytes.chunks_exact(4) {
        let arr: [u8; 4] = chunk
            .try_into()
            .map_err(|_| StorageError::Codec("embedding chunk is not 4 bytes".to_string()))?;
        embedding.push(f32::from_le_bytes(arr));
    }
    offset = embedding_end;

    let metadata_len_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at metadata_len field".to_string()))?;
    let metadata_len_bytes = buf.get(offset..metadata_len_field_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at metadata_len field".to_string())
    })?;
    let metadata_len_arr: [u8; 4] = metadata_len_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("metadata_len field is not 4 bytes".to_string()))?;
    let metadata_len = u32::from_le_bytes(metadata_len_arr);
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }
    offset = offset.checked_add(4).ok_or_else(|| {
        StorageError::Codec("offset overflow after metadata_len field".to_string())
    })?;

    let metadata_end = offset
        .checked_add(metadata_len as usize)
        .ok_or_else(|| StorageError::Codec("offset overflow after metadata field".to_string()))?;
    let metadata_bytes = buf
        .get(offset..metadata_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at metadata field".to_string()))?;
    if metadata_end != buf.len() {
        return Err(StorageError::Codec(
            "row buffer has trailing bytes beyond declared metadata length".to_string(),
        ));
    }

    Ok(Row {
        id,
        tenant_id,
        visibility,
        embedding,
        metadata: metadata_bytes.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(embedding: &[f32], metadata: &[u8]) -> Vec<u8> {
        sample_row_with_rls("tenant-a", Visibility::Public, embedding, metadata)
    }

    fn sample_row_with_rls(
        tenant_id: &str,
        visibility: Visibility,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Vec<u8> {
        encode_row(&RowInput {
            tenant_id,
            visibility,
            embedding,
            metadata,
        })
        .unwrap()
    }

    #[test]
    fn row_roundtrips_through_encode_decode() {
        let embedding = vec![0.5_f32, -1.0, 2.25];
        let metadata = b"opaque".to_vec();
        let buf = sample_row_with_rls("tenant-a", Visibility::Private, &embedding, &metadata);
        let row = decode_row(7, &buf).unwrap();
        assert_eq!(row.id, 7);
        assert_eq!(row.tenant_id, "tenant-a");
        assert_eq!(row.visibility, Visibility::Private);
        assert_eq!(row.embedding, embedding);
        assert_eq!(row.metadata, metadata);
    }

    #[test]
    fn row_roundtrips_with_empty_embedding_and_metadata() {
        let buf = sample_row(&[], &[]);
        let row = decode_row(1, &buf).unwrap();
        assert!(row.embedding.is_empty());
        assert!(row.metadata.is_empty());
    }

    #[test]
    fn decode_row_rejects_version1_row_without_rls_fields() {
        // v1（RLS フィールドを持たない）行を模したバッファ。version=1 の行を暗黙の
        // デフォルトテナントとして読み出すのは fail-open（P0 違反）であり、拒否しなければ
        // ならない。
        let mut v1_buf = Vec::new();
        v1_buf.push(1u8); // ROW_FORMAT_VERSION（旧 v1）
        v1_buf.extend_from_slice(&0u32.to_le_bytes()); // dim=0
        v1_buf.extend_from_slice(&0u32.to_le_bytes()); // metadata_len=0
        let err = decode_row(1, &v1_buf).expect_err("v1 row must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_unknown_visibility_byte() {
        let mut buf = sample_row(&[1.0], b"m");
        // レイアウト: [version][tenant_len(2)][tenant bytes]["tenant-a" は 8 バイト][visibility]
        let visibility_offset = 1 + 2 + "tenant-a".len();
        buf[visibility_offset] = 0xFF;
        let err = decode_row(1, &buf).expect_err("unknown visibility byte must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn encode_row_rejects_empty_tenant_id() {
        let result = encode_row(&RowInput {
            tenant_id: "",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &[],
        });
        assert!(matches!(result, Err(StorageError::Codec(_))));
    }

    #[test]
    fn encode_row_rejects_oversized_tenant_id() {
        let huge_tenant = "t".repeat((MAX_TENANT_ID_LEN as usize) + 1);
        let result = encode_row(&RowInput {
            tenant_id: &huge_tenant,
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &[],
        });
        assert!(matches!(result, Err(StorageError::Codec(_))));
    }

    #[test]
    fn decode_row_rejects_non_utf8_tenant_bytes() {
        let mut buf = sample_row(&[1.0], b"m");
        // "tenant-a" の先頭バイトを不正な UTF-8 継続バイトへ書き換える。
        let tenant_start = 1 + 2;
        buf[tenant_start] = 0xFF;
        let err = decode_row(1, &buf).expect_err("non-UTF-8 tenant_id must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_tenant_len_exceeding_limit() {
        let mut buf = sample_row(&[1.0], b"m");
        let oversized = MAX_TENANT_ID_LEN + 1;
        buf[1..3].copy_from_slice(&oversized.to_le_bytes());
        let err = decode_row(1, &buf).expect_err("oversized tenant_len must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_zero_length_tenant_id() {
        // encode_row 経由では空 tenant_id を作れないため（エンコード時に拒否される）、
        // decode_row 側の空 tenant_id 拒否パス（RLS が黙って無効化される fail-open 経路）を
        // 単体で検証するために、tenant_len=0 の一貫したバッファを直接組み立てる。
        let mut buf = Vec::new();
        buf.push(ROW_FORMAT_VERSION);
        buf.extend_from_slice(&0u16.to_le_bytes()); // tenant_len = 0
        buf.push(Visibility::Public.to_byte());
        buf.extend_from_slice(&0u32.to_le_bytes()); // dim = 0
        buf.extend_from_slice(&0u32.to_le_bytes()); // metadata_len = 0
        let err = decode_row(1, &buf).expect_err("zero-length tenant_id must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_truncated_buffer() {
        let buf = sample_row(&[1.0, 2.0], b"meta");
        for cut in 1..buf.len() {
            assert!(
                decode_row(1, &buf[..cut]).is_err(),
                "truncated buffer of length {cut} should not decode successfully"
            );
        }
    }

    #[test]
    fn decode_row_rejects_unknown_version() {
        let mut buf = sample_row(&[1.0], b"m");
        buf[0] = 0xFF;
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn decode_row_rejects_trailing_garbage() {
        let mut buf = sample_row(&[1.0], b"m");
        buf.push(0);
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn decode_row_rejects_oversized_dim_without_allocating() {
        // dim をアロケーション上限より大きい値に書き換えたバッファ。
        // decode_row は Vec::with_capacity を呼ぶ前に dim の上限検証で拒否するべき。
        let mut buf = sample_row(&[1.0], b"m");
        let oversized = MAX_EMBEDDING_DIM + 1;
        // レイアウト: [version(1)][tenant_len(2)]["tenant-a"(8)][visibility(1)][dim(4)]。
        let dim_offset = 1 + 2 + "tenant-a".len() + 1;
        buf[dim_offset..dim_offset + 4].copy_from_slice(&oversized.to_le_bytes());
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn encode_row_rejects_oversized_metadata() {
        let huge = vec![0u8; (MAX_METADATA_LEN as usize) + 1];
        let result = encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &huge,
        });
        assert!(result.is_err());
    }

    /// テストごとに一意な DB ファイルパスを払い出す（persistence.rs の同名ヘルパーと同じ方針）。
    fn unique_db_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vector-db-engine-storage-{label}-{}-{seq}.redb",
            std::process::id()
        ));
        path
    }

    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // TASK-133 P1 対応: 書き込みコミットのたびに世代カウンタが単調増加し、無関係な
    // 読み取り操作では増加しないことを確認する。
    #[test]
    fn current_generation_increments_only_on_write_commits() {
        let path = unique_db_path("generation");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        assert_eq!(storage.current_generation().expect("gen 0"), 0);

        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0],
                    metadata: &[],
                },
            )
            .expect("put row 1");
        assert_eq!(storage.current_generation().expect("gen after put"), 1);

        storage
            .put_batch(&[(
                2,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[2.0],
                    metadata: &[],
                },
            )])
            .expect("put_batch row 2");
        assert_eq!(
            storage.current_generation().expect("gen after put_batch"),
            2
        );

        // 読み取りのみの操作は世代を進めない。
        let _ = storage.get(1).expect("get row 1");
        let _ = storage.scan().expect("scan");
        assert_eq!(
            storage.current_generation().expect("gen after reads"),
            2,
            "read-only operations must not bump the generation counter"
        );
    }

    #[test]
    fn scan_page_paginates_in_id_order_and_reports_continuation_cursor() {
        let path = unique_db_path("scan-page");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let embeddings: Vec<[f32; 1]> = (0..25u64).map(|i| [i as f32]).collect();
        let rows: Vec<(u64, RowInput<'_>)> = (0..25u64)
            .map(|i| {
                (
                    i,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &embeddings[i as usize],
                        metadata: b"m",
                    },
                )
            })
            .collect();
        storage.put_batch(&rows).expect("seed rows");

        let (page1, cursor1) = storage.scan_page(None, 10).expect("first page");
        assert_eq!(page1.len(), 10);
        assert_eq!(page1.first().map(|r| r.id), Some(0));
        assert_eq!(page1.last().map(|r| r.id), Some(9));
        assert_eq!(cursor1, Some(9));

        let (page2, cursor2) = storage.scan_page(cursor1, 10).expect("second page");
        assert_eq!(page2.len(), 10);
        assert_eq!(page2.first().map(|r| r.id), Some(10));
        assert_eq!(page2.last().map(|r| r.id), Some(19));
        assert_eq!(cursor2, Some(19));

        let (page3, cursor3) = storage
            .scan_page(cursor2, 10)
            .expect("third (partial) page");
        assert_eq!(page3.len(), 5);
        assert_eq!(page3.first().map(|r| r.id), Some(20));
        assert_eq!(page3.last().map(|r| r.id), Some(24));
        // 末尾未満の件数しか返らなかったので、続きがないことを示す `None` を返す。
        assert_eq!(cursor3, None);
    }

    #[test]
    fn scan_page_clamps_limit_to_max_page_limit() {
        let path = unique_db_path("scan-page-clamp");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0],
                    metadata: b"m",
                },
            )
            .expect("seed row");

        // limit に MAX_SCAN_PAGE_LIMIT を超える値を渡しても panic せず、切り詰めて処理される。
        let (page, _cursor) = storage
            .scan_page(None, MAX_SCAN_PAGE_LIMIT + 1_000)
            .expect("scan with oversized limit request");
        assert_eq!(page.len(), 1);
    }

    #[test]
    fn scan_page_caps_page_by_byte_budget_even_under_row_limit() {
        let path = unique_db_path("scan-page-bytes");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // 最大メタデータ長（MAX_METADATA_LEN = 4 MiB）の行を 5 件用意する。行数上限
        // （limit=100）には遠く及ばないが、エンコード済みバイト量の合計はすぐに
        // MAX_SCAN_PAGE_BYTES（16 MiB）を超えるため、バイト上限で打ち切られるはずである。
        let big_metadata = vec![0u8; MAX_METADATA_LEN as usize];
        let rows: Vec<(u64, RowInput<'_>)> = (0..5u64)
            .map(|i| {
                (
                    i,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[],
                        metadata: &big_metadata,
                    },
                )
            })
            .collect();
        storage.put_batch(&rows).expect("seed large rows");

        let (page1, cursor1) = storage
            .scan_page(None, 100)
            .expect("first page capped by byte budget");
        assert!(
            page1.len() < 5,
            "byte budget must cap the page before the row limit (100) is reached, got {} rows",
            page1.len()
        );
        assert!(
            !page1.is_empty(),
            "at least one row must be returned even under a tight byte budget"
        );
        assert!(
            cursor1.is_some(),
            "a capped page must report a continuation cursor"
        );

        // 続きのページを取得し、最終的に全 5 行を過不足なく読み切れること。
        let mut all_ids: Vec<u64> = page1.iter().map(|r| r.id).collect();
        let mut cursor = cursor1;
        loop {
            let (page, next_cursor) = storage
                .scan_page(cursor, 100)
                .expect("subsequent page after byte-budget cap");
            all_ids.extend(page.iter().map(|r| r.id));
            if next_cursor.is_none() {
                break;
            }
            cursor = next_cursor;
        }
        all_ids.sort_unstable();
        assert_eq!(all_ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn scan_page_with_zero_limit_returns_empty_page() {
        let path = unique_db_path("scan-page-zero");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let (page, cursor) = storage.scan_page(None, 0).expect("scan with zero limit");
        assert!(page.is_empty());
        assert_eq!(cursor, None);
    }

    // Issue #131 / PR #193 codex レビュー PRRT_kwDOUAKASM6cC0IQ 対応: `Display` 文字列は
    // `scan` / `scan_batch_log` の 2 経路で単一 variant を共有するため、どちらの経路でも
    // 誤案内にならない中立な文言（経路別 API 名を含まない）に固定する。経路別の正確な
    // 代替手段案内は呼び出し元（`catalog.rs::convert_storage_error` 等）が別途生成する。

    #[test]
    fn scan_limit_error_message_is_neutral_across_scan_and_batch_log_routes() {
        let message = StorageError::ScanLimitExceeded.to_string();
        assert_eq!(
            message,
            "scan limit exceeded: result too large for a single unpaginated read"
        );
        // scan 専用 API 名（scan_page 等）を含まないことを固定する。含めると
        // scan_batch_log 経由の呼び出し元へ存在しない台帳用ページング手段を誤案内する
        // （PR #193 codex レビュー PRRT_kwDOUAKASM6cC0IQ）。
        assert!(!message.contains("scan_page"));
    }

    #[test]
    fn check_scan_limits_rejects_row_and_byte_overrun() {
        // 行数境界: MAX_SCAN_TOTAL_ROWS 件目を追加しようとすると拒否される。
        assert!(matches!(
            check_scan_limits(MAX_SCAN_TOTAL_ROWS, 0, 1),
            Err(StorageError::ScanLimitExceeded)
        ));
        assert!(check_scan_limits(MAX_SCAN_TOTAL_ROWS - 1, 0, 1).is_ok());

        // バイト境界: 追加後に MAX_SCAN_TOTAL_BYTES を超えると拒否される。
        assert!(matches!(
            check_scan_limits(0, MAX_SCAN_TOTAL_BYTES, 1),
            Err(StorageError::ScanLimitExceeded)
        ));
        assert!(check_scan_limits(0, MAX_SCAN_TOTAL_BYTES, 0).is_ok());
    }

    #[test]
    fn check_batch_log_limit_rejects_overrun() {
        assert!(matches!(
            check_batch_log_limit(MAX_BATCH_LOG_ROWS),
            Err(StorageError::ScanLimitExceeded)
        ));
        assert!(check_batch_log_limit(MAX_BATCH_LOG_ROWS - 1).is_ok());
    }

    #[test]
    fn scan_limit_errors_have_no_source() {
        use std::error::Error;
        assert!(StorageError::ScanLimitExceeded.source().is_none());
    }

    // 対象ビヘイビア: TABLE-10。台帳テーブル未作成（DB 作成直後で一度も log_batch
    // していない）状態では `None` を返す（fail-closed だが「未作成 = 0 件」を過不足なく
    // 表現する契約。scan_batch_log の空 Vec 返却と同じ扱い）。
    #[test]
    fn batch_log_max_seq_returns_none_when_table_does_not_exist() {
        let path = unique_db_path("batch-log-max-seq-no-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        assert_eq!(
            storage.batch_log_max_seq().expect("batch_log_max_seq"),
            None
        );
    }

    // 対象ビヘイビア: TABLE-10。挿入順ではなくキー最大値を返すこと（redb の B-tree
    // 末尾キー取得であり、挿入順に依存する実装への退行を検知する）。
    #[test]
    fn batch_log_max_seq_returns_max_key_not_insertion_order() {
        let path = unique_db_path("batch-log-max-seq-max-key");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // `Storage` の private フィールド（`db`）は同一モジュール内なので直接触れる。
        // 公開 API は log_batch 経由（BatchWriteTxn）のみだが、ここでは
        // batch_log_max_seq 単体の「キー最大値」契約だけを最小構成で検証したいため、
        // 台帳テーブルへ非連続キーを直接書き込む。
        {
            let write_txn = storage.db.begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(BATCH_LOG_TABLE)
                    .expect("open batch log table");
                table.insert(3u64, 10u64).expect("insert seq 3");
                table.insert(0u64, 10u64).expect("insert seq 0");
                table.insert(7u64, 10u64).expect("insert seq 7");
            }
            write_txn.commit().expect("commit");
        }

        assert_eq!(
            storage.batch_log_max_seq().expect("batch_log_max_seq"),
            Some(7)
        );
    }

    // 対象ビヘイビア: TABLE-10。境界値 `u64::MAX` を含む場合もそのまま返すこと
    // （呼び出し元の `checked_add` による fail-closed 拒否は呼び出し元の責務であり、
    // 本 API 自身は値の解釈をしない）。
    #[test]
    fn batch_log_max_seq_returns_u64_max_boundary() {
        let path = unique_db_path("batch-log-max-seq-u64-max");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        {
            let write_txn = storage.db.begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(BATCH_LOG_TABLE)
                    .expect("open batch log table");
                table.insert(1u64, 10u64).expect("insert seq 1");
                table.insert(u64::MAX, 10u64).expect("insert seq u64::MAX");
            }
            write_txn.commit().expect("commit");
        }

        assert_eq!(
            storage.batch_log_max_seq().expect("batch_log_max_seq"),
            Some(u64::MAX)
        );
    }

    /// 電源断シミュレーションによるクラッシュ耐性の再検証（TASK-145、ポインタ:
    /// `docs/spec/05-tasks.md`。関連ビヘイビア: PERSIST-3、ポインタ:
    /// `docs/spec/04-behavior/persistence.md`）。検証対象の契約内容は上記ポインタ先を
    /// 参照。他シナリオは
    /// `crates/engine/tests/power_loss.rs`（raw `redb::Database` を直接操作する統合テスト）
    /// を参照。
    ///
    /// 本サブモジュールを `crate` 内の `#[cfg(test)]` に閉じているのは、本番の
    /// `Storage::put`/`Storage::get`（＝実際の `encode_row`/`decode_row`）経由で検証
    /// しつつ、`Storage` の公開 API へバックエンド差し替え用のコンストラクタを一切
    /// 増やさないため。`Storage { db }` の private フィールドへは同一クレート内の
    /// 子モジュールから直接アクセスできるので、feature 限定の `pub fn` を用意せずに
    /// テスト専用の `redb::Database`（`StorageBackend` 差し替え済み）を渡せる。
    mod power_loss {
        use super::*;

        // 電源断シミュレーション用 `StorageBackend` の基本実装（`BackendState`/
        // `PowerLossBackend`）は `crates/engine/tests/power_loss.rs` と共有する
        // ため `power_loss_model`（`crate::storage::power_loss_model`）へ分離済み。
        // 詳細・分離理由は同モジュールの doc を参照。
        use crate::storage::power_loss_model::PowerLossBackend;
        use redb::StorageBackend;

        /// 新規（空）の [`PowerLossBackend`] 上に `redb::Database` を開く。
        fn open_fresh() -> (PowerLossBackend, redb::Database) {
            let backend = PowerLossBackend::new();
            let db = redb::Builder::new()
                .create_with_backend(backend.clone())
                .expect("open fresh database on PowerLossBackend");
            (backend, db)
        }

        /// 指定バイト列を初期像として `redb::Database` を開き直す（「電源断後の再起動」に
        /// 相当）。破損像で開けない場合は `Err` をそのまま返す。
        fn reopen_from_image(
            image: Vec<u8>,
        ) -> std::result::Result<redb::Database, redb::DatabaseError> {
            redb::Builder::new().create_with_backend(PowerLossBackend::from_bytes(image))
        }

        // シナリオ 4（対応する契約: PERSIST-3。ポインタ: `docs/spec/04-behavior/persistence.md`）。
        // `PowerLossBackend` で差し替えた `redb::Database` を、本モジュール（`storage` の
        // 子孫）の特権で `Storage { db }` へ直接渡し（公開 API のバイパス用コンストラクタは
        // 追加しない）、書き込み（`Storage::put`）・読み出し（`Storage::get`）ともに本番の
        // `encode_row`/`decode_row` を経由させる。電源断前後どちらも `Storage::get` で
        // 読み出して同じ内容が復元されることを確認する。
        #[test]
        fn power_loss_scenario4_rls_fields_survive_crash_after_commit() {
            let (backend, raw_db) = open_fresh();
            let storage = Storage { db: raw_db };

            // embedding/metadata は空スライスにしない。空だと encoder/decoder が
            // これらのフィールドを常に欠落させる退行があっても電源断前後の比較が
            // 偶然一致してしまい、「全フィールド保持」を検証したことにならない。
            let row1_embedding = [0.5_f32, -1.0, 2.25];
            let row1_metadata = b"row1-metadata".to_vec();
            let row2_embedding = [3.0_f32, 0.0, -4.5, 1.25];
            let row2_metadata = b"row2-metadata".to_vec();

            storage
                .put(
                    1,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &row1_embedding,
                        metadata: &row1_metadata,
                    },
                )
                .expect("put row 1 via production Storage API");
            storage
                .put(
                    2,
                    &RowInput {
                        tenant_id: "tenant-b",
                        visibility: Visibility::Private,
                        embedding: &row2_embedding,
                        metadata: &row2_metadata,
                    },
                )
                .expect("put row 2 via production Storage API");

            // `Storage::put` の commit が実際に `sync_data()` まで完了していることを
            // 確認してから `durable_snapshot()` を電源断像として使う。ここを確認しないと、
            // 仮に `PowerLossBackend::write` が sync を待たず durable へ書き戻す実装（本来は
            // 契約違反）へ退行しても本テストが検出できなくなる。
            assert_eq!(
                backend.pending_write_count(),
                0,
                "Storage::put の commit 完了後は log が sync 済みで空になっているはず \
                 （0 件でなければ、まだ sync されていない書き込みを電源断像に含めてしまう）"
            );

            let row1_before = storage
                .get(1)
                .expect("decode row 1 via production Storage API before crash");
            let row2_before = storage
                .get(2)
                .expect("decode row 2 via production Storage API before crash");

            let crash_image = backend.durable_snapshot();
            drop(storage);

            let recovered_raw_db =
                reopen_from_image(crash_image).expect("reopen after crash must succeed");
            let recovered_storage = Storage {
                db: recovered_raw_db,
            };

            let row1_after = recovered_storage
                .get(1)
                .expect("decode row 1 via production Storage API after crash");
            assert_eq!(
                row1_after, row1_before,
                "row 1 は電源断前後で production decode_row の結果が完全一致していなければならない"
            );
            assert_eq!(row1_after.tenant_id, "tenant-a");
            assert_eq!(row1_after.visibility, Visibility::Public);
            assert_eq!(
                row1_after.embedding, row1_embedding,
                "row 1 の embedding フィールドが電源断後も欠落・変質なく保持されているはず"
            );
            assert_eq!(
                row1_after.metadata, row1_metadata,
                "row 1 の metadata フィールドが電源断後も欠落・変質なく保持されているはず"
            );

            let row2_after = recovered_storage
                .get(2)
                .expect("decode row 2 via production Storage API after crash");
            assert_eq!(
                row2_after, row2_before,
                "row 2 は電源断前後で production decode_row の結果が完全一致していなければならない"
            );
            assert_eq!(row2_after.tenant_id, "tenant-b");
            assert_eq!(row2_after.visibility, Visibility::Private);
            assert_eq!(
                row2_after.embedding, row2_embedding,
                "row 2 の embedding フィールドが電源断後も欠落・変質なく保持されているはず"
            );
            assert_eq!(
                row2_after.metadata, row2_metadata,
                "row 2 の metadata フィールドが電源断後も欠落・変質なく保持されているはず"
            );
        }

        // 否定コントロール: `sync_data()` を呼ばない限り `write()` の内容は
        // `durable_snapshot()`（＝電源断で必ず残る像）へ反映されないことを直接確認する。
        // これがないと、`PowerLossBackend::write` が sync を待たず即座に `durable` を
        // 書き換える実装（commit/sync 契約違反）へ退行しても、シナリオ 4 は偶然
        // 成功し続けてしまう（恒真化）。`StorageBackend` トレイトのメソッドを
        // `redb::Database` を介さず直接呼び、モデルの sync ゲーティングだけを検証する。
        #[test]
        fn power_loss_backend_durable_image_excludes_writes_until_sync_data() {
            let backend = PowerLossBackend::new();

            backend
                .write(0, b"uncommitted-write")
                .expect("write to backend");
            assert_eq!(
                backend.pending_write_count(),
                1,
                "write() は log に記録されているはず（0 件だと本テストは何も検証していない）"
            );
            assert!(
                backend.durable_snapshot().is_empty(),
                "sync_data() を呼ぶ前は、write() の内容が durable_snapshot()（電源断で \
                 必ず残る像）へ反映されていてはならない"
            );

            backend.sync_data().expect("sync_data");
            assert_eq!(
                backend.pending_write_count(),
                0,
                "sync_data() 後は log が空になっているはず"
            );
            assert_eq!(
                backend.durable_snapshot(),
                b"uncommitted-write",
                "sync_data() を呼んだ後は、write() の内容が durable_snapshot() へ \
                 反映されていなければならない"
            );
        }

        // 退行検出用: `write()` が現在の EOF を越える offset へ書き込んだ場合に
        // `len()` が追従して伸長することを直接確認する。ここを確認しないと、
        // `write()` が `log` へ積むだけで `state.len` を更新しない実装（EOF 越え
        // write を無視する契約違反）へ退行しても検出できない。`len()` が古いままだと、
        // `sync_data()` 後の `durable_snapshot()` の実長が `len()` の申告値を超え、
        // `redb` から見た「ファイル長」と実データが矛盾する。
        #[test]
        fn power_loss_backend_write_past_eof_extends_len() {
            let backend = PowerLossBackend::from_bytes(b"short".to_vec());
            assert_eq!(backend.len().expect("len before write"), 5);

            // 既存の 5 バイトより後ろ（offset 10）へ書き込み、EOF を越えて伸長させる。
            backend.write(10, b"tail").expect("write past current EOF");
            assert_eq!(
                backend.len().expect("len after EOF-crossing write"),
                14,
                "write() が EOF を越えた分だけ len() も伸長しているはず"
            );

            backend.sync_data().expect("sync_data");
            let durable = backend.durable_snapshot();
            assert_eq!(
                durable.len(),
                backend.len().expect("len after sync_data") as usize,
                "sync_data() 後は durable_snapshot() の実長が len() の申告値と \
                 一致していなければならない（不一致は EOF 越え write で len が \
                 追従しない退行の兆候）"
            );
        }
    }
}
