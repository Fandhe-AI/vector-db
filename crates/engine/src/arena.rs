//! コールドスタート・ベクトルアリーナ（TASK-87、対象ビヘイビア: TABLE-8。
//! ポインタ: `docs/spec/05-tasks.md` TASK-87・`docs/spec/04-behavior/data-model.md`
//! TABLE-8）。
//!
//! 責務境界: `storage.rs` はクエリの都度 `redb` から行を読み直してデコードする経路
//! （[`crate::storage::Storage::get`]・[`crate::storage::Storage::scan`] 等）しか
//! 提供しない。本モジュールは、単一の読み取りスナップショットから一度だけ全行を
//! 連続 `Vec<f32>` バッファへデコードし、以降の参照はそのバッファ上のスライスで
//! 完結させる「コールドスタート時の一括ロード」経路を追加する。検索カーネル本体
//! （スコアリング・top-k・SIMD/GPU 経路）は後続タスクの管轄であり、本モジュールは
//! 「一度デコードした連続バッファの提供」までを責務境界とする。
//!
//! テーブル帰属: `catalog.rs` のテーブルスコープ行 API（TASK-146、対象ビヘイビア:
//! EXT-1, EXT-2）が、テーブルごとに独立した動的 redb テーブル
//! （[`crate::catalog::user_rows_table_name`] が指す `user_rows/{table_name}`）へ行を
//! 分離して永続化する。[`VectorArena::build`] はこの対象テーブル専用のテーブルだけを
//! 走査するため、他テーブルの行が次元一致だけで混入することはない
//! （かつてのスコープ外事項だった「行への永続的なテーブル識別子」の欠如は解消済み）。
//! カタログ・行テーブルの両方を単一の `redb::ReadTransaction` 上で扱うことで、
//! スキーマ取得と行走査の間に他テーブルの並行書き込みが挟まる TOCTOU も避ける。
//!
//! [`VectorArena::build`] は `pub` な公開エントリポイントであり、後続の検索カーネル
//! （TASK-133 以降）がコールドスタート時に呼び出す想定の API である。検索カーネル本体は
//! 本モジュールのスコープ外だが、`VectorArena` 自体はクレート外の呼び出し元
//! （`crates/engine/tests/arena.rs` 等の統合テストを含む）から構築・参照できる。
//!
//! RLS との関係: `tenant_id`・`visibility` はデータとして同居保持し、
//! [`VectorArena::build_filtered`] は呼び出し元が渡す述語（`predicate`）の実行結果に
//! 従って行を格納するかどうかを分岐するだけで、可視性判定ロジックそのもの
//! （テナント一致判定・許可ラベル評価）は持たない。判定ロジックは
//! `core.rs::EngineCore::search` が [`crate::policy::PolicyContext::is_visible`]
//! （単一照合パス）から構築した述語として渡す（CORE-2 の判定ロジック集約を維持する。
//! codex P2・Issue #137 対応で構築時フィルタ経路を追加。詳細は
//! [`VectorArena::build_filtered`] のドキュメント参照）。

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::catalog::{self, CatalogError};
use crate::storage::{decode_row, Storage, StorageError, Visibility};

/// アリーナが保持してよい行数の上限（アロケーション前の事前検証に使う。
/// security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_ARENA_ROWS: usize = 1_000_000;

/// アリーナが確保してよい総バイト量の上限。`vectors: Vec<f32>` だけでなく、
/// 同時に確保する `ids`・`tenant_ids`・`visibilities` の見積もりバイト量も合算した
/// 総量として扱う（[`check_capacity`] 参照。codex レビュー指摘対応: 一部バッファのみを
/// 上限対象にすると、他バッファの確保が上限検証をすり抜けて OOM を招き得るため）。
/// `MAX_ARENA_ROWS` だけでは次元数が大きい場合に依然として巨大確保になり得るため、
/// 行数とバイト量の両方で上限を課す（`storage.rs` の `MAX_SCAN_TOTAL_BYTES` と同方針）。
const MAX_ARENA_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// アリーナ構築層の公開エラー型。`storage.rs` の設計メモ（`StorageError` への
/// blanket `From<E: Into<redb::Error>>` impl）が存在するため coherence 制約上
/// 同種の blanket impl はこの型へ追加できない。`redb` へは必ず `StorageError` 経由で
/// 到達し、本型は `StorageError` からの明示的な `From` のみを持つ
/// （`catalog.rs` の `CatalogError` と同方針）。
#[derive(Debug)]
pub enum ArenaError {
    /// 永続化層側で発生したエラー（`redb` バックエンドエラー・行デコード失敗等）。
    Storage(StorageError),
    /// カタログ層側で発生したエラー（対象テーブル不存在・識別子不正等）。
    Catalog(CatalogError),
    /// `expected_dim` が不正（`0` または [`crate::storage::MAX_EMBEDDING_DIM`] 超過）。
    /// 対象テーブルが `VECTOR` 列を持たない場合もこの variant を返す。
    InvalidDim,
    /// アリーナ構築対象の行数・総バイト量がアロケーション前の上限
    /// （[`MAX_ARENA_ROWS`]・[`MAX_ARENA_TOTAL_BYTES`]）を超過した。fail-closed に拒否する。
    CapacityExceeded,
    /// `expected_dim` と一致しない次元の行を検出した。黙殺スキップせず拒否する
    /// （部分的なアリーナを返さない。fail-open を避けるための判断）。
    DimMismatch { id: u64, expected: u32, found: u32 },
    /// `check_capacity` によるアロケーション前の上限検証を通過した後、実際の
    /// `Vec::try_reserve` がメモリ不足で失敗した。`Vec::with_capacity`・`Vec::push`
    /// の内部確保（失敗時に abort する）ではなく `try_reserve` を使うことで、
    /// OOM を allocator の abort ではなく `Err` として呼び出し元へ伝える
    /// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。メッセージは
    /// プログラム出力文字列のため英語）。
    AllocationFailed(String),
}

impl std::fmt::Display for ArenaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArenaError::Storage(e) => write!(f, "arena storage error: {e}"),
            ArenaError::Catalog(e) => write!(f, "arena catalog error: {e}"),
            ArenaError::InvalidDim => write!(f, "invalid expected_dim for arena build"),
            ArenaError::CapacityExceeded => write!(f, "arena capacity exceeded"),
            ArenaError::DimMismatch {
                id,
                expected,
                found,
            } => write!(
                f,
                "embedding dim mismatch at row id={id}: expected={expected} found={found}"
            ),
            ArenaError::AllocationFailed(msg) => write!(f, "arena allocation failed: {msg}"),
        }
    }
}

impl std::error::Error for ArenaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArenaError::Storage(e) => Some(e),
            ArenaError::Catalog(e) => Some(e),
            ArenaError::InvalidDim
            | ArenaError::CapacityExceeded
            | ArenaError::DimMismatch { .. }
            | ArenaError::AllocationFailed(_) => None,
        }
    }
}

impl From<StorageError> for ArenaError {
    fn from(e: StorageError) -> Self {
        ArenaError::Storage(e)
    }
}

impl From<CatalogError> for ArenaError {
    fn from(e: CatalogError) -> Self {
        ArenaError::Catalog(e)
    }
}

pub type Result<T> = std::result::Result<T, ArenaError>;

/// [`VectorArena::build_filtered`] のアロケーション前上限検証（行数・総バイト量の両方、
/// `checked_mul`/`checked_add` によるオーバーフロー安全な演算）。成功時は、その時点まで
/// 確保すべき `vectors: Vec<f32>` の要素数（`row_count * dim`）を返す（呼び出し元は
/// 現在のところ値そのものは使わず、検証だけに使う）。
///
/// `build_filtered` は本関数を、`predicate` を通過した（＝実際にアリーナへ格納する）
/// 可視行数を `row_count` として、可視行を 1 件追加するたびに呼ぶ（codex 指摘・
/// Issue #137 対応: テーブル全行数を上限判定に使うと、対象テナントの可視行が少なくても
/// 他テナントの不可視行を含む総数次第で検索が失敗し、テナント間で可用性が干渉するため。
/// [`VectorArena::build_filtered`] のドキュメント参照）。
///
/// `max_bytes` は `vectors` だけでなく、同時に確保する `ids: Vec<u64>`・
/// `tenant_ids: Vec<String>`・`visibilities: Vec<Visibility>` の見積もりバイト量も
/// 合算した総量として扱う（codex レビュー指摘対応: `vectors` のみを上限対象にすると、
/// 最大 1M 行分の他バッファの確保が上限検証をすり抜け、`Vec::with_capacity` の
/// 確保失敗時 abort（OOM abort）を招き得るため）。`tenant_ids` の 1 要素あたりは
/// `String` 構造体自体のスタック分に加え、ヒープ上の文字列バイト列を最悪ケース
/// （[`crate::storage::MAX_TENANT_ID_LEN`]）で見積もる。
///
/// 上限値（`max_rows`・`max_bytes`）を引数として受け取る形に切り出しているのは、
/// `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES` が private 定数であり、境界値検証
/// （ちょうど上限・上限+1 等）を本ファイル内の `#[cfg(test)]` モジュールから
/// 直接パラメータ化して再現するため。
fn check_capacity(row_count: usize, dim: u32, max_rows: usize, max_bytes: usize) -> Result<usize> {
    if row_count > max_rows {
        return Err(ArenaError::CapacityExceeded);
    }
    let total_floats = row_count
        .checked_mul(dim as usize)
        .ok_or(ArenaError::CapacityExceeded)?;
    let vectors_bytes = total_floats
        .checked_mul(4)
        .ok_or(ArenaError::CapacityExceeded)?;

    let per_row_aux_bytes = per_row_aux_bytes().ok_or(ArenaError::CapacityExceeded)?;
    let aux_bytes = row_count
        .checked_mul(per_row_aux_bytes)
        .ok_or(ArenaError::CapacityExceeded)?;

    let total_bytes = vectors_bytes
        .checked_add(aux_bytes)
        .ok_or(ArenaError::CapacityExceeded)?;
    if total_bytes > max_bytes {
        return Err(ArenaError::CapacityExceeded);
    }
    Ok(total_floats)
}

/// `ids: Vec<u64>`・`tenant_ids: Vec<String>`・`visibilities: Vec<Visibility>` の
/// 1 行あたりの見積もりバイト数（[`check_capacity`] 専用のヘルパー、境界値テストからも
/// 同じ値を参照できるよう関数として切り出す）。`tenant_ids` の 1 要素あたりは `String`
/// 構造体自体のスタック分に加え、ヒープ上の文字列バイト列を最悪ケース
/// （[`crate::storage::MAX_TENANT_ID_LEN`]）で見積もる。
fn per_row_aux_bytes() -> Option<usize> {
    std::mem::size_of::<u64>()
        .checked_add(std::mem::size_of::<String>())
        .and_then(|v| v.checked_add(crate::storage::MAX_TENANT_ID_LEN as usize))
        .and_then(|v| v.checked_add(std::mem::size_of::<Visibility>()))
}

/// `Vec::try_reserve` の失敗を [`ArenaError::AllocationFailed`] へ変換する共通ヘルパー
/// （[`VectorArena::build_filtered`] 専用）。`Vec::with_capacity`・`Vec::push` の内部確保
/// （失敗時に abort する）ではなく本関数経由で予約することで、`check_capacity` の
/// 上限検証を通過した後にホスト側のメモリが実際に不足した場合でも、プロセスを
/// OOM abort させず `Err` として呼び出し元へ伝える（security.md「不安全な設計｜
/// 無制限リソース確保（DoS）」対応）。`what` はエラーメッセージに含める対象バッファ名
/// （英語。プログラム出力文字列）。
///
/// `try_reserve_exact`（要求量ちょうどだけ確保する厳密版）ではなく `try_reserve`
/// （amortized 成長。実装が将来の追加分を見越して多めに確保しうる）を使う
/// （codex 指摘・Issue #137 対応: `build_filtered` は可視行 1 件ごとに本関数を呼ぶため、
/// 呼び出しのたびに要求量ちょうどだけ確保する方式だと、行を追加するたびに再確保が
/// 発生し ―― 最悪 O(可視行数) 回の再確保・コピーになりうる。amortized 成長により
/// 通常の `Vec::push` と同等の償却計算量を維持しつつ、確保失敗を `Err` として扱える）。
fn try_reserve<T>(buf: &mut Vec<T>, additional: usize, what: &str) -> Result<()> {
    buf.try_reserve(additional)
        .map_err(|e| ArenaError::AllocationFailed(format!("failed to reserve {what}: {e}")))
}

/// コールドスタート時に一括デコードした連続ベクトルバッファ（対象ビヘイビア: TABLE-8）。
///
/// [`VectorArena::build`] が単一の読み取りスナップショットから構築する。構築後に
/// `Storage` へ加わった変更（他ライタによる書き込みを含む）は反映されない
/// （`redb::ReadTransaction` のスナップショット契約をそのまま引き継ぐ）。
#[derive(Debug)]
pub struct VectorArena {
    /// 構築時に `build` へ渡されたテーブル名。`vectors`/`ids` の全行はこのテーブル専用の
    /// 行テーブル（[`crate::catalog::user_rows_table_name`] が指す
    /// `user_rows/{table_name}`）のみを走査した結果であり、他テーブルの行は保存先が
    /// 分離されているため混入しない（モジュールドキュメントの「テーブル帰属」参照）。
    table_name: String,
    dim: u32,
    /// 行 ID 昇順・row-major の連続バッファ。長さは常に `ids.len() * dim` と一致する。
    vectors: Vec<f32>,
    /// `vectors` の第 i 行（`vectors[i*dim..(i+1)*dim]`）に対応する行 ID。
    ids: Vec<u64>,
    /// 後続の RLS 事前フィルタ（TASK-89/133 系）が redb 再読なしでテナント判定できる
    /// よう同居保持する（ポリシー評価自体は行わない。モジュールドキュメント参照）。
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
}

impl VectorArena {
    /// `storage` の現時点のスナップショットから、カタログ上のテーブル `table_name`
    /// を対象としたアリーナを構築する公開エントリポイント（対象ビヘイビア: TABLE-8）。
    ///
    /// 呼び出し文脈: 後続の検索カーネル（TASK-133 以降）が、コールドスタート時
    /// （プロセス起動直後・対象テーブルの初回クエリ時等）に本関数を呼び、以降のクエリは
    /// 返された [`VectorArena`] 上のスライスを参照する想定の公開 API である。検索カーネル
    /// 本体（スコアリング・top-k 選定）は本モジュールのスコープ外で、本関数は
    /// 「一度デコードした連続バッファを返す」までを責務とする。
    ///
    /// `expected_dim` はカタログ（[`Storage::get_table_schema`] →
    /// [`crate::catalog::TableSchema::vector_dim`]）から取得し、呼び出し元からは受け取らない
    /// （呼び出し元が渡す次元値をテーブル識別の代用にすると、同一次元の別テーブルが
    /// 混入しても検出できないため）。
    ///
    /// 走査対象は `table_name` に対応する行テーブル
    /// （[`crate::catalog::user_rows_table_name`] が指す `user_rows/{table_name}`。
    /// TASK-146・EXT-1/EXT-2）のみで、他テーブルの行は保存先そのものが分離されている
    /// ため次元一致だけで混入することはない（対象ビヘイビア: TABLE-8）。
    ///
    /// 単一の `storage.db().begin_read()` 上でカタログのスキーマ取得・対象テーブルの行走査を
    /// 行う（[`crate::storage::Storage::scan_page`]・[`crate::storage::Storage::scan_table_page`]
    /// は呼び出しごとに別トランザクションを開くためページ間のスナップショット一貫性がなく、
    /// アリーナ構築には使えない）。アロケーション前に行数・総バイト量の上限を検証してから
    /// 確保する（無制限 `Vec::with_capacity` 禁止。.claude/rules/coding-rust.md）。
    ///
    /// 対象テーブルがカタログに存在しない場合・`VECTOR` 列を持たない場合は
    /// `Err`（テーブル未作成の空アリーナという特別扱いはしない。カタログに登録されて
    /// いて 1 行も書き込まれていない場合のみ空アリーナとして成功する）。次元不一致の
    /// 行を検出した場合はスキップせず `Err(ArenaError::DimMismatch)` を返す
    /// （部分的なアリーナを返さない fail-closed な判断。通常この分岐へは到達しない。
    /// `insert_row_into_table`/`insert_rows_into_table` が挿入時に次元検証済みのため、
    /// 事後にスキーマ・行データが手で書き換えられた場合の防御として残す）。
    pub fn build(storage: &Storage, table_name: &str) -> Result<Self> {
        Self::build_filtered(storage, table_name, |_, _| true)
    }

    /// [`Self::build`] の構築時フィルタ付き版（codex P2・Issue #137 対応）。
    /// `predicate(tenant_id, visibility)` が `false` を返す行は、decode 直後に
    /// 破棄してアリーナへ格納しない（`vectors`/`ids`/`tenant_ids`/`visibilities` の
    /// いずれにも追加しない）。
    ///
    /// 呼び出し文脈: `core.rs::EngineCore::search` が
    /// `|tenant, visibility| ctx.is_visible(tenant, visibility)` をそのまま渡すことで、
    /// `PolicyContext` の下で不可視な行（他テナント行を含む）をそもそもアリーナへ確保
    /// しない「構築時フィルタ」を実現する。以前は [`Self::build`] が対象テーブル全行の
    /// アリーナを構築してから、`core.rs` 側が可視行だけの別バッファへ改めて確保・
    /// 全コピーしていたため、1 検索あたりのピークメモリが最大で 2 倍（全行アリーナ ＋
    /// 可視行コピー）になっていた。`predicate` を構築ループへ渡す設計にすることで、
    /// 可視縮約ビューを単一確保・コピーなしで得られるようにする。
    ///
    /// アロケーション前の上限検証（[`check_capacity`]）は「`predicate` を通過した行
    /// （＝実際にアリーナへ格納する行）」に対して、行を追加するたびに逐次行う
    /// （codex 指摘・Issue #137 対応: 以前はテーブル全行数（`table.len()`）を上限判定に
    /// 使っていたため、対象テナントの可視行が少なくても、他テナントの不可視行を含む
    /// テーブル全体の行数・バイト量が [`MAX_ARENA_ROWS`]・[`MAX_ARENA_TOTAL_BYTES`] を
    /// 超えると検索そのものが `CapacityExceeded` で失敗し、他テナントのデータ量が
    /// 対象テナントの検索可用性へ干渉していた。可視行基準に変えることで、この干渉を
    /// なくす）。事前の全行分一括予約もしない。可視行 1 件を追加するたびに
    /// `Vec::try_reserve`（amortized 成長。`Vec::with_capacity`/`push` の内部確保のように
    /// 失敗時 abort しない）で少しずつ確保する。
    ///
    /// 次元不一致検証（[`ArenaError::DimMismatch`]）は `predicate` による可視性判定より
    /// 前に行う。`predicate` が `false` を返す（＝呼び出し元から不可視）行であっても
    /// データ破損の検出自体はスキップしない（[`Self::build`]・従来の全件検証と同じ
    /// fail-closed な挙動を維持する。不可視行の破損を黙って読み飛ばすと、破損データの
    /// 存在に誰も気付けなくなるため）。
    pub fn build_filtered<F>(storage: &Storage, table_name: &str, predicate: F) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
    {
        Self::build_filtered_with_limits(
            storage,
            table_name,
            predicate,
            MAX_ARENA_ROWS,
            MAX_ARENA_TOTAL_BYTES,
        )
    }

    /// [`Self::build_filtered`] の上限値パラメータ化版。実装は本関数に集約し、
    /// [`Self::build_filtered`] は本番用の定数（[`MAX_ARENA_ROWS`]・
    /// [`MAX_ARENA_TOTAL_BYTES`]）で呼び出すだけの薄いラッパーにする。
    ///
    /// `max_rows`・`max_bytes` を引数化しているのは、[`check_capacity`] と同じ理由
    /// （上記 [`check_capacity`] のドキュメント参照）で、境界値検証を
    /// 本ファイル内の `#[cfg(test)]` モジュールから小さい上限値で再現するため
    /// （本番の 1,000,000 行・1 GiB 相当のデータセットをテストごとに用意するのは
    /// 非現実的）。「他テナントの不可視行が大量にあっても対象テナントの可視行が
    /// 上限内なら検索が失敗しないこと」を検証するテスト
    /// （`build_filtered_capacity_check_is_based_on_visible_rows_not_total_table_rows`）
    /// が、小さい `max_rows` を指定してこの構造を直接検証する。
    fn build_filtered_with_limits<F>(
        storage: &Storage,
        table_name: &str,
        mut predicate: F,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
    {
        // スキーマ取得・対象テーブルの行走査を単一の `read_txn`（同一スナップショット）上で
        // 行う。別トランザクションに分かれていると、スキーマ取得後・走査前に対象テーブルへの
        // 並行書き込みが挟まってもスナップショットの一貫性が保証できない。
        let read_txn = storage.db().begin_read().map_err(StorageError::from)?;
        let schema = catalog::get_table_schema_in_txn(&read_txn, table_name)?;
        let expected_dim = schema.vector_dim().ok_or(ArenaError::InvalidDim)?;
        if expected_dim == 0 || expected_dim > crate::storage::MAX_EMBEDDING_DIM {
            return Err(ArenaError::InvalidDim);
        }

        // 対象テーブル専用の行テーブル（`user_rows/{table_name}`）だけを開く。
        // `catalog::user_rows_table_name` は識別子検証を行わないが、直前の
        // `get_table_schema_in_txn` がカタログ照会の前段で `validate_identifier` を
        // 通しているため、ここで改めて検証する必要はない。
        let row_table_name = catalog::user_rows_table_name(table_name);
        let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
        let table = match read_txn.open_table(row_table_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(VectorArena {
                    table_name: table_name.to_string(),
                    dim: expected_dim,
                    vectors: Vec::new(),
                    ids: Vec::new(),
                    tenant_ids: Vec::new(),
                    visibilities: Vec::new(),
                });
            }
            Err(e) => return Err(StorageError::from(e).into()),
        };

        // `table.len()` は redb 側の集計値（テーブル全行数。不可視行を含む）で、
        // アロケーション上限検証には使わない（上記ドキュメント参照）。走査自体には
        // 使わないため取得しない。
        let dim = expected_dim as usize;
        let mut vectors: Vec<f32> = Vec::new();
        let mut ids: Vec<u64> = Vec::new();
        let mut tenant_ids: Vec<String> = Vec::new();
        let mut visibilities: Vec<Visibility> = Vec::new();
        // `predicate` を通過した（＝実際に格納する）行数。上限検証はこの値に対して行う。
        let mut visible_row_count: usize = 0;

        for entry in table.iter().map_err(StorageError::from)? {
            let (k, v) = entry.map_err(StorageError::from)?;
            let id = k.value();
            let row = decode_row(id, v.value()).map_err(ArenaError::from)?;
            let found_dim =
                u32::try_from(row.embedding.len()).map_err(|_| ArenaError::DimMismatch {
                    id,
                    expected: expected_dim,
                    found: u32::MAX,
                })?;
            if found_dim != expected_dim {
                return Err(ArenaError::DimMismatch {
                    id,
                    expected: expected_dim,
                    found: found_dim,
                });
            }
            // 構築時フィルタ（codex P2 対応）: `predicate` が `false` を返す行（呼び出し元
            // から不可視）は、デコード直後にここで破棄しアリーナへ確保しない。次元検証
            // （上記）は既に完了しているため、不可視行の破損検出を見逃すことはない。
            if !predicate(&row.tenant_id, row.visibility) {
                continue;
            }

            // アロケーション前の上限検証（.claude/rules/security.md「不安全な設計｜
            // 無制限リソース確保（DoS）」対応）を、行を追加する直前に可視行数基準で行う
            // （codex 指摘対応。上記ドキュメント参照）。`check_capacity` は
            // checked_mul/checked_add でオーバーフロー安全に判定する既存ヘルパーを
            // そのまま再利用する（`visible_row_count` を都度渡すだけで、行数・バイト量の
            // 両方が可視行基準で検証される）。
            visible_row_count = visible_row_count
                .checked_add(1)
                .ok_or(ArenaError::CapacityExceeded)?;
            check_capacity(visible_row_count, expected_dim, max_rows, max_bytes)?;

            // 検証を通過した後にのみ確保する。事前の全行分一括予約はせず、可視行 1 件
            // ごとに `try_reserve`（amortized 成長。`Vec::with_capacity`/`push` の内部
            // 確保のように失敗時 abort しない）で確保し、メモリ不足を
            // `Err(ArenaError::AllocationFailed)` として呼び出し元へ返す
            // （codex レビュー指摘対応: `check_capacity` のアロケーション前上限検証を
            // 通過していても、実際のホストメモリが不足していれば確保は失敗し得るため、
            // OOM abort ではなく fail-closed な `Err` にする）。
            try_reserve(&mut vectors, dim, "vectors")?;
            try_reserve(&mut ids, 1, "ids")?;
            try_reserve(&mut tenant_ids, 1, "tenant_ids")?;
            try_reserve(&mut visibilities, 1, "visibilities")?;

            vectors.extend_from_slice(&row.embedding);
            ids.push(id);
            tenant_ids.push(row.tenant_id);
            visibilities.push(row.visibility);
        }

        Ok(VectorArena {
            table_name: table_name.to_string(),
            dim: expected_dim,
            vectors,
            ids,
            tenant_ids,
            visibilities,
        })
    }

    /// 構築時に `build` へ渡されたテーブル名（全行がこのテーブルに帰属することを
    /// 保証する。上記フィールドのドキュメント参照）。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// 埋め込みの次元数。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 保持している行数。
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// 行を 1 件も保持していないか。
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// 行 ID の一覧（構築時のスキャン順＝行 ID 昇順）。
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// row-major の連続ベクトルバッファ全体（長さ = `len() * dim()`）。
    pub fn vectors(&self) -> &[f32] {
        &self.vectors
    }

    /// `index` 番目の行の埋め込みスライスを返す。範囲外は `None`
    /// （`.claude/rules/coding-rust.md`: 添字アクセス `[]` を production コードで使わない）。
    pub fn vector(&self, index: usize) -> Option<&[f32]> {
        let dim = self.dim as usize;
        let start = index.checked_mul(dim)?;
        let end = start.checked_add(dim)?;
        self.vectors.get(start..end)
    }

    /// `index` 番目の行のテナント識別子。範囲外は `None`。
    pub fn tenant_id(&self, index: usize) -> Option<&str> {
        self.tenant_ids.get(index).map(String::as_str)
    }

    /// `index` 番目の行の可視性ラベル。範囲外は `None`。
    pub fn visibility(&self, index: usize) -> Option<Visibility> {
        self.visibilities.get(index).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RowInput;

    #[test]
    fn capacity_check_accepts_at_row_limit() {
        assert_eq!(
            check_capacity(100, 4, 100, usize::MAX).expect("within row limit"),
            400
        );
    }

    #[test]
    fn capacity_check_rejects_over_row_limit() {
        assert!(matches!(
            check_capacity(101, 4, 100, usize::MAX),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn capacity_check_accepts_at_byte_limit() {
        // row_count * dim * 4（vectors）+ row_count * per_row_aux_bytes（ids・tenant_ids・
        // visibilities の見積もり）== max_bytes ちょうど。
        let row_count = 10usize;
        let dim = 4u32;
        let aux = per_row_aux_bytes().expect("aux bytes computation must not overflow in test");
        let max_bytes = row_count * (dim as usize) * 4 + row_count * aux;
        assert_eq!(
            check_capacity(row_count, dim, usize::MAX, max_bytes).expect("within byte limit"),
            40
        );
    }

    #[test]
    fn capacity_check_rejects_over_byte_limit() {
        let row_count = 10usize;
        let dim = 4u32;
        let aux = per_row_aux_bytes().expect("aux bytes computation must not overflow in test");
        let max_bytes = row_count * (dim as usize) * 4 + row_count * aux;
        assert!(matches!(
            check_capacity(row_count, dim, usize::MAX, max_bytes - 1),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。`vectors`（f32 埋め込みバッファ）の
    // バイト量だけでは上限に収まっていても、`ids`・`tenant_ids`・`visibilities` の
    // 見積もりバイト量を合算すると上限を超える場合に拒否すること
    // （codex 指摘: 一部バッファのみを上限検証の対象にすると、他バッファの確保が
    // 検証をすり抜けて OOM abort を招き得るため）。
    #[test]
    fn capacity_check_rejects_when_aux_buffers_push_total_over_limit() {
        let row_count = 10usize;
        let dim = 4u32;
        let vectors_bytes = row_count * (dim as usize) * 4;
        // vectors だけなら上限内だが、aux バッファ込みでは超過する上限値。
        let max_bytes = vectors_bytes;
        assert!(matches!(
            check_capacity(row_count, dim, usize::MAX, max_bytes),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn capacity_check_does_not_overflow_on_huge_dim() {
        // usize::MAX 近傍の dim を渡しても checked_mul が Err に落ちるだけで panic しない。
        let result = check_capacity(usize::MAX / 2, u32::MAX, usize::MAX, usize::MAX);
        assert!(matches!(result, Err(ArenaError::CapacityExceeded)));
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。`check_capacity` の上限検証を素通りする
    // ほど巨大な予約要求（`isize::MAX` バイト超）に対して、`try_reserve` が
    // `Vec::with_capacity`/`Vec::push` のように abort せず `Err(ArenaError::AllocationFailed)`
    // を返すことを検証する。`isize::MAX` 超のレイアウトは Rust のアロケーション API 契約上
    // 実メモリを確保しようとする前に即座に拒否されるため、CI 環境で実際に大量のメモリを
    // 消費せず決定的に再現できる。
    #[test]
    fn try_reserve_converts_oversized_request_to_allocation_failed_without_aborting() {
        let mut buf: Vec<u8> = Vec::new();
        let oversized = (isize::MAX as usize).saturating_add(1);
        let result = try_reserve(&mut buf, oversized, "test buffer");
        assert!(matches!(result, Err(ArenaError::AllocationFailed(_))));
    }

    /// `tests/arena.rs`（統合テスト）と同方針の一意 DB パス払い出しヘルパー。
    /// `VectorArena::build`・`catalog::get_table_schema_in_txn` が `pub(crate)` のため、
    /// 統合テストからは呼べずこのモジュール内 unit test でのみ検証できる。
    fn unique_db_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vector-db-engine-arena-unit-{label}-{}-{seq}.redb",
            std::process::id()
        ));
        path
    }

    // スキーマ取得と行走査の TOCTOU への回帰テスト（対象ビヘイビア: TABLE-8）。
    // 対象テーブル `a` についてスキーマを取得した read_txn を保持したまま、*その後*に
    // 別テーブル `b` を作成・行を書き込んでも、同一 read_txn 上で見える対象テーブル `a`
    // の行テーブル内容は書き込み前のスナップショットのまま（`b` への並行書き込みが
    // `a` の走査結果へ影響しないこと）を検証する。
    #[test]
    fn get_table_schema_in_txn_observes_a_single_snapshot_across_concurrent_writes() {
        use crate::catalog::{self, ColumnDef, ColumnType, TableSchema};
        use crate::storage::{RowInput, Visibility};
        // `table.len()`（本テスト専用の検証用途）にのみ必要なトレイト。`build_filtered`
        // はもうテーブル全行数を使わないため、モジュール先頭の `use` からは外した
        // （codex 指摘・Issue #137 対応。上記 `check_capacity` のドキュメント参照）。
        use redb::ReadableTableMetadata;

        let path = unique_db_path("toctou");
        let storage = Storage::open(&path).expect("open storage");

        let schema_a = TableSchema::new(
            "a",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        );
        storage.create_table(&schema_a).expect("create table a");
        storage
            .insert_row_into_table(
                "a",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[0.0, 1.0, 2.0, 3.0],
                    metadata: &[],
                },
            )
            .expect("insert row into a");

        // スキーマ取得・行テーブルオープン用のスナップショットを先に確立する。
        let read_txn = storage.db().begin_read().expect("begin_read");
        let schema = catalog::get_table_schema_in_txn(&read_txn, "a").expect("get schema for a");
        assert_eq!(schema.name, "a");

        // read_txn 確立後に別テーブル・同次元の行を並行挿入する（TOCTOU 再現）。
        let schema_b = TableSchema::new(
            "b",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        );
        storage.create_table(&schema_b).expect("create table b");
        storage
            .insert_row_into_table(
                "b",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[9.0, 9.0, 9.0, 9.0],
                    metadata: &[],
                },
            )
            .expect("insert row into b");

        // 同一 read_txn 上では、後発の書き込みは一切見えない。対象テーブル `a` の
        // 行テーブルは read_txn 確立時点のスナップショットのまま 1 行だけを保持する。
        let row_table_name = catalog::user_rows_table_name("a");
        let row_table_def: redb::TableDefinition<u64, &[u8]> =
            redb::TableDefinition::new(&row_table_name);
        let table = read_txn
            .open_table(row_table_def)
            .expect("open row table for a");
        assert_eq!(table.len().expect("table len"), 1);

        drop(read_txn);
        let _ = std::fs::remove_file(&path);
    }

    // 以下は旧 `tests/arena.rs`・`tests/arena_perf.rs`（統合テスト）からの移設分。
    // `VectorArena::build` が `pub(crate)`（テーブルスコープの内部 API に依存する）
    // ため、統合テストからは呼べず、クレート内の `#[cfg(test)]` モジュールへ
    // 移設したまま保持している。

    struct CleanupGuard(std::path::PathBuf);

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    const TENANT_ID: &str = "tenant-a";

    /// 外部クレート非依存の決定的擬似乱数生成器（xorshift32）。テストデータ生成にのみ使う。
    struct Xorshift32(u32);

    impl Xorshift32 {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }

        fn next_f32(&mut self) -> f32 {
            (self.next_u32() as f64 / u32::MAX as f64) as f32
        }
    }

    fn make_embedding(rng: &mut Xorshift32, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| rng.next_f32()).collect()
    }

    /// `table_name` の schema を組み立てる（`embedding` 列 1 本のみを持つ最小構成。
    /// `multi_dim_tables.rs` と同方針）。
    fn schema_for(table_name: &str, dim: u32) -> crate::catalog::TableSchema {
        use crate::catalog::{ColumnDef, ColumnType};
        crate::catalog::TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    // 対象ビヘイビア: TABLE-8。複数行を投入して build した結果が、行数・次元・各行の
    // 内容とも Storage::get の読み直し結果と一致し、連続バッファの長さが len * dim と
    // 一致すること（コールドスタート・アリーナの基本契約）を検証する。
    #[test]
    fn build_produces_contiguous_arena_matching_storage_rows() {
        let path = unique_db_path("basic");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let dim: usize = 8;
        storage
            .create_table(&schema_for("docs", dim as u32))
            .expect("create_table");

        let mut rng = Xorshift32(0x1234_5678);
        let embeddings: Vec<Vec<f32>> = (0..10).map(|_| make_embedding(&mut rng, dim)).collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, emb)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility: if i % 2 == 0 {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                        embedding: emb,
                        metadata: b"m",
                    },
                )
            })
            .collect();
        storage
            .insert_rows_into_table("docs", &rows)
            .expect("seed rows");

        let arena = VectorArena::build(&storage, "docs").expect("build arena");
        assert_eq!(arena.table_name(), "docs");
        assert_eq!(arena.dim(), dim as u32);
        assert_eq!(arena.len(), 10);
        assert!(!arena.is_empty());
        assert_eq!(arena.vectors().len(), 10 * dim);
        assert_eq!(arena.ids(), &(0u64..10).collect::<Vec<_>>()[..]);

        for i in 0..10usize {
            let expected_row = storage
                .get_row_from_table("docs", i as u64)
                .expect("read row back via storage");
            assert_eq!(arena.vector(i), Some(expected_row.embedding.as_slice()));
            assert_eq!(arena.tenant_id(i), Some(expected_row.tenant_id.as_str()));
            assert_eq!(arena.visibility(i), Some(expected_row.visibility));
        }

        // 範囲外は panic せず None を返す。
        assert_eq!(arena.vector(10), None);
        assert_eq!(arena.tenant_id(10), None);
        assert_eq!(arena.visibility(10), None);
    }

    // 対象ビヘイビア: TABLE-8。カタログに登録済みだが 1 行も書き込んでいないテーブル
    // （`user_rows/{table_name}` 未作成）は空アリーナとして成功すること。
    #[test]
    fn build_on_empty_table_returns_empty_arena() {
        let path = unique_db_path("empty");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 16))
            .expect("create_table");

        let arena = VectorArena::build(&storage, "docs").expect("build arena on empty table");
        assert_eq!(arena.dim(), 16);
        assert_eq!(arena.len(), 0);
        assert!(arena.is_empty());
        assert!(arena.vectors().is_empty());
        assert!(arena.ids().is_empty());
    }

    // 対象ビヘイビア: TABLE-8。次元不一致の行が 1 行でも混在していれば、部分的な
    // アリーナを返さず Err(DimMismatch) で fail-closed に拒否すること
    // （黙殺スキップは検索結果の欠落＝fail-open に相当するため禁止）。
    #[test]
    fn build_rejects_dimension_mismatch_without_partial_result() {
        let path = unique_db_path("dim-mismatch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"m",
                },
            )
            .expect("seed matching-dim row");
        // `insert_row_into_table` は挿入時点で次元検証するため、次元不一致行を通常経路で
        // 作れない。`build` 側の防御（事後にスキーマ・行データが書き換えられた場合）を
        // 検証するため、ここでは検証を経由しない生の write トランザクションで直接
        // 対象テーブルの行テーブルへ次元不一致の行を書き込む。
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let row_table_name = catalog::user_rows_table_name("docs");
                let row_table_def: redb::TableDefinition<u64, &[u8]> =
                    redb::TableDefinition::new(&row_table_name);
                let mut row_table = write_txn.open_table(row_table_def).expect("open row table");
                let encoded = crate::storage::encode_row(&RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                })
                .expect("encode mismatched-dim row");
                row_table
                    .insert(1u64, encoded.as_slice())
                    .expect("insert mismatched-dim row bypassing dim validation");
            }
            write_txn.commit().expect("commit mismatched-dim row");
        }

        let err = VectorArena::build(&storage, "docs").expect_err("dim mismatch must be rejected");
        match err {
            ArenaError::DimMismatch {
                id,
                expected,
                found,
            } => {
                assert_eq!(id, 1);
                assert_eq!(expected, 4);
                assert_eq!(found, 2);
            }
            other => panic!("expected DimMismatch, got {other:?}"),
        }
    }

    // 対象ビヘイビア: TABLE-8。対象テーブルがカタログに存在しない場合、および
    // `VECTOR` 列を持たない場合は `Err(InvalidDim)` で拒否すること。
    #[test]
    fn build_rejects_missing_table_and_table_without_vector_column() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};

        let path = unique_db_path("invalid-dim");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // カタログに未登録のテーブル名。
        assert!(VectorArena::build(&storage, "not_registered").is_err());

        // VECTOR 列を持たないテーブル。
        let text_only = TableSchema::new(
            "notes",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        storage.create_table(&text_only).expect("create_table");
        assert!(matches!(
            VectorArena::build(&storage, "notes"),
            Err(ArenaError::InvalidDim)
        ));
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。複数テーブルが共存する状態で、
    // 対象テーブル以外に書き込まれた行（次元が一致する行を含む）が混入しないこと、
    // かつ対象テーブルの行はすべて取得できることを検証する。行はテーブルごとに
    // 分離された動的 redb テーブル（`user_rows/{table_name}`）へ永続化されるため、
    // 次元一致のみを根拠とした混入は起こり得ない。
    #[test]
    fn build_scopes_arena_to_the_requested_table_only() {
        let path = unique_db_path("multi-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        storage
            .create_table(&schema_for("docs_a", 4))
            .expect("create_table docs_a");
        storage
            .create_table(&schema_for("docs_b", 4))
            .expect("create_table docs_b");

        storage
            .insert_row_into_table(
                "docs_a",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"table=docs_a",
                },
            )
            .expect("seed docs_a row");
        storage
            .insert_row_into_table(
                "docs_b",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[9.0, 9.0, 9.0, 9.0],
                    metadata: b"table=docs_b",
                },
            )
            .expect("seed docs_b row");
        // docs_b にはさらに、docs_a に存在しない ID の行も追加しておく（行数だけで
        // 混入の有無を誤判定しないようにするため）。
        storage
            .insert_row_into_table(
                "docs_b",
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[8.0, 8.0, 8.0, 8.0],
                    metadata: b"table=docs_b",
                },
            )
            .expect("seed second docs_b row");

        let arena_a = VectorArena::build(&storage, "docs_a").expect("build arena for docs_a");
        assert_eq!(arena_a.len(), 1);
        assert_eq!(arena_a.ids(), &[0u64]);
        assert_eq!(arena_a.vector(0), Some([1.0, 2.0, 3.0, 4.0].as_slice()));

        let arena_b = VectorArena::build(&storage, "docs_b").expect("build arena for docs_b");
        assert_eq!(arena_b.len(), 2);
        assert_eq!(arena_b.ids(), &[0u64, 1u64]);
        assert_eq!(arena_b.vector(0), Some([9.0, 9.0, 9.0, 9.0].as_slice()));
        assert_eq!(arena_b.vector(1), Some([8.0, 8.0, 8.0, 8.0].as_slice()));
    }

    // 対象ビヘイビア: TABLE-8（codex P2 対応・Issue #137）。`build_filtered` の
    // `predicate` が `false` を返す行は、そもそもアリーナへ格納されない（`ids`・
    // `vectors`・`tenant_ids`・`visibilities` のいずれにも現れない）ことを検証する。
    // `build`（`predicate` 常に `true`）と同じデータセットに対して行うことで、
    // 構築時フィルタが「後から除外する」のではなく「最初から格納しない」ことを
    // 行数・内容の両面で確認する。
    #[test]
    fn build_filtered_excludes_rows_failing_the_predicate_at_construction_time() {
        let path = unique_db_path("filtered-excludes");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: b"m",
                },
            )
            .expect("seed tenant-a row");
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: &[2.0, 0.0],
                    metadata: b"m",
                },
            )
            .expect("seed tenant-b row");
        storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[3.0, 0.0],
                    metadata: b"m",
                },
            )
            .expect("seed tenant-a private row");

        // tenant-a・Public の行だけを可視とする述語（`PolicyContext::is_visible` の
        // 既定挙動と同じ形の判定を模す）。
        let arena = VectorArena::build_filtered(&storage, "docs", |tenant, visibility| {
            tenant == "tenant-a" && visibility == Visibility::Public
        })
        .expect("build_filtered ok");

        assert_eq!(
            arena.len(),
            1,
            "only the tenant-a/Public row must be stored"
        );
        assert_eq!(arena.ids(), &[0u64]);
        assert_eq!(arena.vector(0), Some([1.0, 0.0].as_slice()));
        assert_eq!(
            arena.vectors().len(),
            2,
            "no capacity for excluded rows' vectors is retained in len()"
        );
        assert_eq!(arena.tenant_id(0), Some("tenant-a"));

        // 除外された行（id=1, id=2）はどのインデックスにも現れない。
        for idx in 0..arena.len() {
            assert_ne!(arena.tenant_id(idx), Some("tenant-b"));
        }
    }

    // 対象ビヘイビア: TABLE-8（codex P2 対応・Issue #137）。`predicate` が `false` を
    // 返す（＝不可視な）行であっても、次元不一致のデータ破損検出はスキップしない
    // ことを検証する（`build`・従来の全件検証と同じ fail-closed な挙動。不可視行の
    // 破損を黙って読み飛ばすと誰も気付けなくなるため）。
    #[test]
    fn build_filtered_still_detects_dimension_mismatch_in_rows_excluded_by_the_predicate() {
        let path = unique_db_path("filtered-dim-mismatch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        // 述語で除外される想定のテナント（tenant-b）に、次元不一致の破損行を仕込む
        // （`insert_row_into_table` は挿入時点で次元検証するため、
        // `build_rejects_dimension_mismatch_without_partial_result` と同手法で
        // 検証を経由しない生の write トランザクションで直接書き込む）。
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let row_table_name = catalog::user_rows_table_name("docs");
                let row_table_def: redb::TableDefinition<u64, &[u8]> =
                    redb::TableDefinition::new(&row_table_name);
                let mut row_table = write_txn.open_table(row_table_def).expect("open row table");
                let encoded = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                })
                .expect("encode mismatched-dim row");
                row_table
                    .insert(0u64, encoded.as_slice())
                    .expect("insert mismatched-dim row bypassing dim validation");
            }
            write_txn.commit().expect("commit mismatched-dim row");
        }

        // 述語は tenant-b を常に不可視として除外するが、次元不一致の検出はこの述語より
        // 前に行われるため、不可視行であっても Err(DimMismatch) にならなければならない。
        let err = VectorArena::build_filtered(&storage, "docs", |tenant, _| tenant != "tenant-b")
            .expect_err("dim mismatch in an invisible row must still be rejected");
        assert!(
            matches!(
                err,
                ArenaError::DimMismatch {
                    id: 0,
                    expected: 4,
                    found: 2
                }
            ),
            "expected DimMismatch for the excluded row, got: {err:?}"
        );
    }

    // 対象ビヘイビア: TABLE-8（codex 指摘対応・Issue #137）。他テナントの不可視行が
    // 大量にあっても、対象テナントの可視行数が上限内であれば検索
    // （`VectorArena::build_filtered`）が `CapacityExceeded` にならないことを検証する。
    // 本番の上限定数（`MAX_ARENA_ROWS` = 1,000,000・`MAX_ARENA_TOTAL_BYTES` = 1 GiB）で
    // 同じ状況を再現するのは非現実的（テストごとに 100 万行超を用意する必要がある）
    // ため、`check_capacity` と同じ理由でパラメータ化された
    // `build_filtered_with_limits` へ小さい `max_rows` を渡して同じ構造を検証する。
    //
    // `max_rows = 3` に対し、テーブル全体は 10 行（tenant-b の不可視行 8 件 + tenant-a の
    // 可視行 2 件）で、テーブル全行数（10）は上限（3）を超えている。以前の実装
    // （テーブル全行数を上限判定に使う）であればこの時点で `CapacityExceeded` になるが、
    // 可視行数（2）は上限（3）以下のため、修正後は成功し可視行だけが返る。
    #[test]
    fn build_filtered_capacity_check_is_based_on_visible_rows_not_total_table_rows() {
        let path = unique_db_path("capacity-visible-rows-only");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        // tenant-b（不可視想定）の行を、可視行数の上限（3）を上回る本数だけ挿入する。
        for i in 0..8u64 {
            storage
                .insert_row_into_table(
                    "docs",
                    i,
                    &RowInput {
                        tenant_id: "tenant-b",
                        visibility: Visibility::Public,
                        embedding: &[9.0, 9.0],
                        metadata: b"m",
                    },
                )
                .expect("seed tenant-b row");
        }
        // tenant-a（可視想定）の行は上限（3）以下の 2 件のみ。
        for i in 8..10u64 {
            storage
                .insert_row_into_table(
                    "docs",
                    i,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[1.0, 1.0],
                        metadata: b"m",
                    },
                )
                .expect("seed tenant-a row");
        }

        let max_rows = 3usize;
        let max_bytes = usize::MAX; // 本テストでは行数上限だけを検証対象にする。
        let arena = VectorArena::build_filtered_with_limits(
            &storage,
            "docs",
            |tenant, _| tenant == "tenant-a",
            max_rows,
            max_bytes,
        )
        .expect(
            "capacity check must be based on the 2 visible rows (<= max_rows), \
             not the 10 total table rows (> max_rows)",
        );

        assert_eq!(arena.len(), 2);
        assert_eq!(arena.ids(), &[8u64, 9u64]);
        for idx in 0..arena.len() {
            assert_eq!(arena.tenant_id(idx), Some("tenant-a"));
        }
    }

    // 対象ビヘイビア: TABLE-8。アリーナは構築時点のスナップショットであり、build 後に
    // 追加された行は反映されない（単一スナップショットで構築する契約）。再 build すれば
    // 反映される。
    #[test]
    fn build_captures_a_snapshot_not_reflecting_later_writes() {
        let path = unique_db_path("snapshot");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                },
            )
            .expect("seed initial row");

        let arena_before = VectorArena::build(&storage, "docs").expect("build before extra write");
        assert_eq!(arena_before.len(), 1);

        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[3.0, 4.0],
                    metadata: b"m",
                },
            )
            .expect("seed additional row after snapshot");

        // build 前に取得した arena_before はそのまま（後続の put の影響を受けない）。
        assert_eq!(arena_before.len(), 1);

        let arena_after = VectorArena::build(&storage, "docs").expect("rebuild after extra write");
        assert_eq!(arena_after.len(), 2);
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。旧 `Storage::put`/`Storage::scan` 系の
    // 平坦な行ストア（`storage.rs::ROWS_TABLE`）へ書き込まれた行は、テーブルスコープ行
    // API（`insert_row_into_table` 等）が使う `user_rows/{table_name}` とは別の redb
    // テーブルであるため、対象テーブルのアリーナには一切混入しないことを検証する
    // （以前の実装は「行への永続的なテーブル識別子」が無く、この分離を保証できなかった）。
    #[test]
    fn build_does_not_mix_in_rows_from_the_flat_legacy_row_store() {
        use crate::storage::RowInput as FlatRowInput;

        let path = unique_db_path("legacy-flat-store");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // 旧経路（テーブル帰属を持たない平坦な行ストア）へ、同次元の行を書き込む。
        storage
            .put(
                999,
                &FlatRowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"legacy-flat-store",
                },
            )
            .expect("seed row into legacy flat row store");

        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");
        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[5.0, 6.0, 7.0, 8.0],
                    metadata: b"table=docs",
                },
            )
            .expect("seed row into table-scoped store");

        let arena = VectorArena::build(&storage, "docs").expect("build arena");
        // 旧経路の行（id=999）は混入せず、テーブルスコープ経路の行（id=0）のみを保持する。
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.ids(), &[0u64]);
        assert_eq!(arena.vector(0), Some([5.0, 6.0, 7.0, 8.0].as_slice()));
    }

    // 以下、旧 `tests/arena_perf.rs`（統合テスト）からの移設分。「コールドスタート時に
    // 一度だけアリーナを構築し、以降の検索はアリーナ上のスライスを参照する経路」が
    // 「クエリの都度 `Storage::scan_table_page` でページングしながら読み直してデコードする
    // 経路」より十分速いことを CI で検出可能にする（`tests/incremental_write_perf.rs`
    // （TASK-143）と同一の計測方針: ウォームアップ 1 回を除外・複数ラウンドの中央値比較・
    // `Duration::saturating_mul` の整数比較で判定・判定閾値は本テスト固有の計測パラメータで
    // spec の実測比そのものは転記しない）。
    //
    // 規模の選定: `Storage::scan_table_page` は 1 ページあたり総バイト量
    // `MAX_SCAN_PAGE_BYTES`（16MiB）超で次ページへ打ち切る（`storage.rs` 参照）。
    // 本テストの行数・次元は、その上限に対して十分な余裕を残し、かつ debug ビルドでも
    // CI 実行時間が長くなりすぎないよう小さく抑えている
    // （ROWS × DIM × 4 バイト ≈ 2.6 MiB で 1 ページに収まる）。
    mod perf {
        use super::*;
        use std::time::{Duration, Instant};

        /// 計測対象テーブル名。カタログにこのテーブルのみを登録し、`VectorArena::build`
        /// のテーブルスコープゲートを満たす。
        const TABLE_NAME: &str = "docs";

        /// 行数・次元（モジュールドキュメントの規模選定を参照）。
        const ROWS: u64 = 5_000;
        const DIM: usize = 128;

        /// 1 ラウンドで実行するクエリ本数。
        const QUERY_COUNT: usize = 40;

        /// ノイズ対策として、両経路それぞれを複数回計測し中央値を取る回数。
        const MEASUREMENT_ROUNDS: usize = 3;

        /// 判定閾値の分母（アリーナ経路は都度読み直し経路の `1 / RATIO_THRESHOLD_DENOM`
        /// 以下の時間で完了すること）。本テストの計測パラメータであり、アサーション
        /// 弱体化は行わない（`.claude/rules/coding-rust.md` 参照）。
        const RATIO_THRESHOLD_DENOM: u32 = 4;

        fn make_vector(rng: &mut Xorshift32, dim: usize) -> Vec<f32> {
            (0..dim).map(|_| rng.next_f32()).collect()
        }

        fn median(mut values: Vec<Duration>) -> Duration {
            values.sort();
            values[values.len() / 2]
        }

        /// 単純な内積（テスト内の素朴なスコアリング。検索カーネル本体は後続タスクの管轄。
        /// モジュールドキュメント参照）。
        fn dot(a: &[f32], b: &[f32]) -> f32 {
            a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
        }

        fn best_score_over_arena(arena: &VectorArena, query: &[f32]) -> f32 {
            let mut best = f32::MIN;
            for i in 0..arena.len() {
                let v = arena.vector(i).expect("index within arena bounds");
                let score = dot(v, query);
                if score > best {
                    best = score;
                }
            }
            best
        }

        fn best_score_over_rescan(storage: &Storage, query: &[f32]) -> f32 {
            // テーブルスコープ経路（`scan_table_page`）でページングしながら都度読み直す。
            // `Storage::scan`（`storage.rs` の平坦な `ROWS_TABLE` 走査）は本テストが使う
            // テーブルスコープ行 API（`insert_rows_into_table`）とは別の redb テーブルを
            // 参照するため、ここでは使えない。
            let mut best = f32::MIN;
            let mut cursor: Option<u64> = None;
            loop {
                let (rows, next_cursor) = storage
                    .scan_table_page(TABLE_NAME, cursor, crate::storage::MAX_SCAN_PAGE_LIMIT)
                    .expect("scan_table_page within configured limits");
                for row in &rows {
                    let score = dot(&row.embedding, query);
                    if score > best {
                        best = score;
                    }
                }
                match next_cursor {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            best
        }

        fn seed_storage(path: &std::path::Path) -> Storage {
            let storage = Storage::open(path).expect("open storage");
            storage
                .create_table(&schema_for(TABLE_NAME, DIM as u32))
                .expect("create_table");
            let mut rng = Xorshift32(0x2545_f491);
            let rows: Vec<(u64, Vec<f32>)> = (0..ROWS)
                .map(|id| (id, make_vector(&mut rng, DIM)))
                .collect();
            let batch: Vec<(u64, RowInput<'_>)> = rows
                .iter()
                .map(|(id, emb)| {
                    (
                        *id,
                        RowInput {
                            tenant_id: TENANT_ID,
                            visibility: Visibility::Public,
                            embedding: emb,
                            metadata: b"m",
                        },
                    )
                })
                .collect();
            storage
                .insert_rows_into_table(TABLE_NAME, &batch)
                .expect("seed dataset");
            storage
        }

        fn make_queries(seed: u32) -> Vec<Vec<f32>> {
            let mut rng = Xorshift32(seed | 1);
            (0..QUERY_COUNT)
                .map(|_| make_vector(&mut rng, DIM))
                .collect()
        }

        // 対象ビヘイビア: TABLE-8。「コールドスタート時に一度だけアリーナを構築し、以降の
        // クエリはアリーナ走査で完結する経路」が「クエリの都度 Storage::scan で全行を
        // 読み直しデコードする経路」より十分速いことを、判定閾値（RATIO_THRESHOLD_DENOM）で
        // 検証する。
        #[test]
        fn table8_arena_query_path_completes_within_ratio_threshold_of_rescan_path() {
            let path = unique_db_path("perf-dataset");
            let _cleanup = CleanupGuard(path.clone());
            let storage = seed_storage(&path);

            // ウォームアップ 1 回（ファイルシステムキャッシュ等の初回コストを計測から
            // 除外する。既存 perf テスト tests/incremental_write_perf.rs と同方針）。
            {
                let warmup_queries = make_queries(0xabad_1dea);
                let arena = VectorArena::build(&storage, TABLE_NAME).expect("warmup build arena");
                for q in &warmup_queries {
                    std::hint::black_box(best_score_over_arena(&arena, q));
                }
                for q in &warmup_queries {
                    std::hint::black_box(best_score_over_rescan(&storage, q));
                }
            }

            let mut arena_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
            let mut rescan_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);

            for round in 0..MEASUREMENT_ROUNDS as u32 {
                let queries = make_queries(0x9e37_79b9u32.wrapping_add(round));

                // 経路 (a): コールドスタート・アリーナを一度 build し、各クエリはアリーナ
                // 走査で完結する。build 自体もこの経路のコストとして計測に含める
                // （都度読み直し経路の各クエリが redb からの読み直しコストを含むのと
                // 対称にするため）。
                let started = Instant::now();
                let arena =
                    VectorArena::build(&storage, TABLE_NAME).expect("build arena (measured)");
                for q in &queries {
                    std::hint::black_box(best_score_over_arena(&arena, q));
                }
                arena_durations.push(started.elapsed());

                // 経路 (b): 各クエリごとに Storage::scan() で全行を読み直しデコードする。
                let started = Instant::now();
                for q in &queries {
                    std::hint::black_box(best_score_over_rescan(&storage, q));
                }
                rescan_durations.push(started.elapsed());
            }

            let t_arena = median(arena_durations);
            let t_rescan = median(rescan_durations);
            let ratio = t_arena.as_secs_f64() / t_rescan.as_secs_f64().max(f64::EPSILON);

            // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
            println!(
                "table8 arena vs rescan perf: t_arena={t_arena:?} t_rescan={t_rescan:?} ratio={ratio:.4}"
            );

            assert!(
                t_arena.saturating_mul(RATIO_THRESHOLD_DENOM) <= t_rescan,
                "arena query path ({t_arena:?}) must complete within 1/{RATIO_THRESHOLD_DENOM} of the \
                 rescan path ({t_rescan:?}), ratio={ratio:.4}"
            );
        }
    }
}
