//! SQL 表層の実行計画層（TASK-75・TASK-76、対象ビヘイビア: SQL-1, SQL-2, SQL-3, SQL-4,
//! SQL-7。ポインタ: `docs/spec/05-tasks.md` TASK-75・TASK-76・
//! `docs/spec/04-behavior/sql-surface.md`・`docs/spec/04-behavior/rls.md` RLS-5）。
//!
//! 責務境界: [`parser::bind`](crate::sql::parser::bind) が返す [`BoundStatement`] を、
//! 既定順序 **RLS → SCALAR → DISTANCE**、または `HINT ORDER(...)` が指定した順序
//! （[`crate::sql::plan::EvaluationOrder`]）で実行する。順序が実効を持つのは
//! SCALAR と DISTANCE の相対位置のみ（[`crate::sql::plan::ExecutionPlan`] 参照）:
//! SCALAR が先なら候補構築時に等価条件を事前適用し（従来どおり正確に `limit` 件）、
//! DISTANCE が先なら DISTANCE 段の後で等価条件を事後適用する（`limit` 未満になり
//! 得る。under-fetch の救済（オーバーサンプル等）は本モジュールの管轄外）。
//!
//! RLS は **`HINT ORDER` の内容に関係なく**、唯一の実効的な防御である候補構築時の
//! 暗黙事前フィルタ（`VectorArena::build_filtered_with_rows_in_txn` の `predicate`。
//! `WHERE` 句の `visible()` 呼び出し（[`BoundStatement::rls_predicate_present`]）の
//! 有無に**関係なく**無条件に適用する。SQL-3・RLS-7・RLS-8）を必ず経由する。この
//! 事前フィルタの述語は RLS の暗黙適用フック（[`crate::rls::ImplicitRlsHook`]）
//! 経由で取得する（TASK-137・RLS-6, RLS-7）。加えて
//! [`crate::rls::RlsSafetyNet`]（TASK-136・RLS-5）を最終結果へ無条件に適用するが、
//! この安全網は事前フィルタと同じ `arena`（既に `ctx.is_visible` を通過済みの候補
//! 集合）由来のラベルで再判定するため、現状の実行経路では不可視行を追加で落とす
//! ことはない。無効化できない構造にしてあるのは、候補集合の構築元が将来広がった
//! 場合の defense-in-depth としてで、「独立した 2 つの検査が効いている」という
//! 意味ではない（詳細は `plan.rs` のモジュールドキュメント参照）。`HINT ORDER` で
//! RLS 段を後段に置いても、事前フィルタの適用そのものは外れない（security.md P0
//! 「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。安全網通過済みの
//! `hits` は [`crate::rls::RlsVerifiedHits`]（witness 型）としてのみ投影段へ渡り、
//! 生の `Vec<(u64, f64)>` から投影へ到達する経路は型として存在しない。
//!
//! `core.rs::EngineCore::execute_sql`（TASK-75 で追加する固有メソッド。`VectorCore`
//! trait は不変）からのみ呼ばれる想定で、`Storage`・`SearchProvider`・`PolicyContext`
//! を束ねる。

use std::collections::HashSet;

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::{CatalogError, ColumnType, TableSchema};
use crate::core;
use crate::declarative_filter;
use crate::hybrid::{self, HybridError, HybridHit, RrfConfig};
use crate::kernel::{KernelError, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::rls::{ImplicitRlsHook, RlsSafetyNet, RlsVerifiedHits};
use crate::row_codec::{self, RowCodecError, Value};
use crate::sparse::{DocId, SparseError, SparseIndex};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::parser::{BoundStatement, ProjectedColumn, Ranking};
use crate::sql::plan::ExecutionPlan;
use crate::sql::udf_call;
use crate::storage::StorageError;

/// 疎コーパス側へ集約する候補プールの既定深さ。`hybrid::hybrid_search` の
/// `cfg.pool_depth()` に渡す（[`hybrid::RrfConfig::new`] の検証を通過する範囲で、
/// `bound.limit`（`1..=core::MAX_SEARCH_K`）を必ず満たせるよう `bound.limit` との
/// 最大値を取る）。
const DEFAULT_HYBRID_POOL_DEPTH: usize = 200;

/// `candidate_columns`（RLS/SCALAR 段を通過した可視行のうち、投影で実際に使う
/// スカラー列だけを保持するキャッシュ）が累計で保持してよいバイト数の上限
/// （Issue #56 レビュー指摘対応・codex P1: 1 フィールド最大 4 MiB（`row_codec::MAX_TEXT_FIELD_LEN`）
/// × 候補最大 `arena::MAX_ARENA_ROWS`（100 万行）の組み合わせでは、投影に不要な列まで
/// 複製・保持すると巨大確保に至り得る。`on_visible_row` は投影で使う列のみを保持し、
/// かつ保持前にこの累計上限をアロケーション前に検証する。上限判定の主体は
/// テキスト実体（`Value::Text` の複製バイト数）であり、これが本対応の中心。
/// 加えて、行ごとに必ず確保される `Vec<Value>` 自体の構造体サイズ
/// （`schema.columns.len() * size_of::<Value>()`）も加算する。列数が少ない
/// スキーマでは `MAX_ARENA_ROWS`（100 万行）分でも本上限には遠く及ばないため、
/// この構造体分の加算は「投影列を一切持たない `SELECT id ...` かつ列数が多い
/// スキーマ」の残余リスクを塞ぐ副次的な安全網であり、主たる歯止めではない。
/// `arena::MAX_ARENA_TOTAL_BYTES`
/// と同一の 1 GiB を採用する（256 MiB 等より小さい値にすると、候補行数が多いが
/// 1 行あたりは小さいテーブルへの `LIMIT` 付き正当なクエリを不必要に拒否しうるため、
/// 密ベクタ側の総量上限と同じ桁数に揃えて過度に厳しくしない）。
const MAX_CANDIDATE_SCALAR_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// `current` に `add` を加えた累計が `cap` を超えないことを検証してから返す
/// （超過時は [`ArenaError::CapacityExceeded`]。アロケーション前の累計バイト量
/// 検証を 1 箇所に集約するための共通ヘルパー。`on_visible_row` の構造体バイト・
/// テキストバイトの両方の検証で使う）。
fn try_accumulate_budget(current: usize, add: usize, cap: usize) -> Result<usize, ArenaError> {
    let next = current.saturating_add(add);
    if next > cap {
        return Err(ArenaError::CapacityExceeded);
    }
    Ok(next)
}

/// `!scalar_prefilter`（DISTANCE 先行・SCALAR 事後フィルタ）の precision 経路で、
/// `visible_len`（`arena.ids().len()`。`ImplicitRlsHook` で RLS 済みの可視集合件数）
/// が `core::MAX_SEARCH_K` を超えるかどうかを純粋関数として切り出したもの
/// （`sql::exec::execute_statement` の DISTANCE 段呼び出し直前で使う）。超える場合、
/// DISTANCE 段の取得件数 `k_eff` は `MAX_SEARCH_K` へクランプされ、可視集合全体を
/// 対象にした「WHERE を満たす候補の完全な順位列」を構築できなくなる（`MAX_SEARCH_K`
/// 件目より後ろに僅差の Top-2 相当が存在しても取得できず、`precision::apply_gate`
/// が「Top-2 不在＝マージン成立」と誤判定する fail-open 経路になる。codex-review
/// 指摘・PRRT_kwDOUAKASM6cPLHE）。呼び出し元は `true` の場合、DISTANCE 検索自体を
/// 実行せず空集合（fail-closed の通常応答）へ倒す。
fn precision_completeness_unbounded(
    is_precision: bool,
    scalar_prefilter: bool,
    visible_len: usize,
) -> bool {
    is_precision && !scalar_prefilter && visible_len > core::MAX_SEARCH_K
}

/// `on_visible_row`（RLS/SCALAR 段の行フック）専用の、借用 `&str` から `String` への
/// 選択的複製ヘルパー（Issue #56 レビュー指摘対応・codex P1: `decode_scalar_columns`
/// の全列無条件確保を廃し、`row_codec::scan_scalar_columns` の borrow 結果から実際に
/// 必要な列だけを複製する設計へ切り替えた本対応の中心）。累計バイト量
/// （`budget`）を `cap` 超過なら**確保前**に `ArenaError::CapacityExceeded` として拒否し
/// （行単位の上限は `on_visible_row` 側で行構造体分を別途計上する）、確保自体も
/// `String::try_reserve_exact` を使うことで、論理サイズが上限内でもホスト側メモリが
/// 実際に不足した場合に abort ではなく `Err(ArenaError::AllocationFailed)` を返す
/// （`try_clone_text`・`arena.rs::try_reserve_exact` と同方針）。
fn try_alloc_text_for_budget(
    text: &str,
    budget: &mut usize,
    cap: usize,
) -> Result<String, ArenaError> {
    *budget = try_accumulate_budget(*budget, text.len(), cap)?;
    let mut owned = String::new();
    owned.try_reserve_exact(text.len()).map_err(|e| {
        ArenaError::AllocationFailed(format!("failed to reserve scalar text field: {e}"))
    })?;
    owned.push_str(text);
    Ok(owned)
}

// `pool_depth = bound.limit.max(DEFAULT_HYBRID_POOL_DEPTH)` が常に
// `hybrid::RrfConfig::new` の検証（`1..=hybrid::MAX_POOL_DEPTH`）を通過するのは、
// `bound.limit` の上限（`core::MAX_SEARCH_K`。`sql::parser::bind` が検証済み）が
// `hybrid::MAX_POOL_DEPTH` を超えない場合に限る。両定数は独立に管理されているため、
// 片方だけの変更でこの前提が崩れないようコンパイル時に固定する
// （`row_codec.rs`・`catalog.rs` の既存 `const _: () = assert!(...)` と同方針）。
const _: () = assert!(
    core::MAX_SEARCH_K <= hybrid::MAX_POOL_DEPTH,
    "sql::exec's hybrid pool_depth derivation assumes core::MAX_SEARCH_K <= hybrid::MAX_POOL_DEPTH"
);

// `precision` 時の `k_eff`（SCALAR 事前フィルタ経路: `bound.limit.max(2)`。
// SCALAR 事後フィルタ経路: `arena.ids().len().clamp(2, core::MAX_SEARCH_K)`。
// いずれも「`LIMIT 1` でも Top-2 を取得する」ための下限が `2`）が常に
// `core::MAX_SEARCH_K` の範囲内に収まることをコンパイル時に固定する
// （`bound.limit` は `sql::parser::bind` が `1..=core::MAX_SEARCH_K` を検証済み、
// 事後フィルタ経路は `.min(core::MAX_SEARCH_K)` で明示的にクランプ済みのため、
// `k_eff` が `MAX_SEARCH_K` を超えるのは `MAX_SEARCH_K < 2` の場合のみ）。
const _: () = assert!(
    2 <= core::MAX_SEARCH_K,
    "sql::exec's precision k_eff derivation assumes core::MAX_SEARCH_K >= 2"
);

/// 投影結果 1 セル。`row_codec::Value` の公開 enum は変更せず、`id` 疑似列
/// （`u64`）を表現するため独自の enum を持つ。
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Float`・`Bool`
/// variant を追加した（宣言的 UDF・組み込み関数呼び出しの結果列。`row_codec::Value`
/// は変更しない方針を踏襲する）。
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Integer(u64),
    Text(String),
    Vector(Vec<f32>),
    /// 式項目（TASK-79・SQL-9）の `Scalar` 型評価結果。
    Float(f64),
    /// 式項目（TASK-79・SQL-9）の `Bool` 型評価結果。
    Bool(bool),
}

/// 投影結果の列メタデータ。`Id` は疑似列（`ColumnType` を持たない）。
///
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Computed` variant を
/// 追加した。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnMeta {
    Id,
    Scalar {
        name: String,
        ty: ColumnType,
    },
    /// 式項目（TASK-79・SQL-9）。`ColumnType` を持たない（`Cell::Float`/`Cell::Bool`/
    /// `Cell::Vector` のいずれになるかは実行時の評価結果の型による）。
    Computed {
        name: String,
    },
}

/// 投影結果 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct ResultRow {
    pub id: u64,
    pub score: f64,
    pub cells: Vec<Cell>,
}

/// `EngineCore::execute_sql` の成功応答。
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<ResultRow>,
}

/// `EngineCore::execute_insert_sql` の成功応答（SQL-10、TASK-80）。行形 `INSERT` は
/// 単一行のみを受理するため `rows_affected` は常に `1` になるが、
/// `INSERT 0 1` 相当の wire 応答（TASK-73）へ写像しやすいよう件数フィールドとして
/// 保持する。ファイル形 `INSERT`（TASK-120・INDEX-1, INDEX-2）は複数チャンク行を
/// 書き込むため `incremental` に計測・件数を保持し、行形では常に `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertOutcome {
    pub rows_affected: u64,
    pub incremental: Option<crate::incremental::IndexOutcome>,
}

impl From<RowCodecError> for SqlSurfaceError {
    /// スカラーペイロードのデコード失敗は、格納済みデータの破損・実装バグの
    /// いずれかであり、SQL 入力自体の不正ではないため fail-closed に `XX000` へ
    /// 丸め込む（他テナントの行内容をエラー経由で漏らさない。security.md 準拠）。
    fn from(_e: RowCodecError) -> Self {
        SqlSurfaceError::Internal {
            detail: "scalar payload decode failed".to_string(),
        }
    }
}

/// `arena::build_filtered_with_rows` の RLS/SCALAR 段が返す `ArenaError` を SQL 表層の
/// エラー契約へ写像する。テーブル不在は `42P01`、容量上限超過は `54000`、それ以外
/// （データ破損・実装バグ）は `XX000` に丸め込む（他テナントの存在情報を漏らさない）。
fn map_arena_error(table: &str, e: ArenaError) -> SqlSurfaceError {
    match e {
        ArenaError::Catalog(CatalogError::TableNotFound(_)) => SqlSurfaceError::UndefinedTable {
            name: table.to_string(),
        },
        ArenaError::CapacityExceeded => {
            SqlSurfaceError::payload_too_large("candidate set exceeds arena capacity")
        }
        // TASK-79（SQL-9）: `WHERE` 式述語の評価（`sql::udf_call::eval`）が 0 除算・
        // 非有限値等で fail-closed に拒否した場合（`expr_eval_error_to_arena` 参照）。
        // データ破損・実装バグ（`Storage`/`Catalog`/`InvalidDim`/`DimMismatch`/
        // `AllocationFailed`）とは異なり、入力値に起因する「受理構文だが値が不正」
        // として `22000` へ写像する（`sql::parser::bind_in_session` の他の
        // `InvalidInput` と同じ分類）。
        ArenaError::InvalidInput(detail) => SqlSurfaceError::invalid_input(detail),
        ArenaError::Storage(_)
        | ArenaError::Catalog(_)
        | ArenaError::InvalidDim
        | ArenaError::DimMismatch { .. }
        | ArenaError::AllocationFailed(_) => SqlSurfaceError::Internal {
            detail: "arena build failed".to_string(),
        },
    }
}

/// `sql::udf_call::eval` が返す [`SqlSurfaceError`] を、`on_visible_row`（行フック。
/// 戻り値が [`ArenaError`] に固定されている）から返せる形へ写像する（TASK-79・
/// SQL-9）。`map_arena_error` がこの逆写像（`ArenaError` → `SqlSurfaceError`）を
/// 呼び出し元で行い、`22000`／`54000` の wire_code を保つ（`arena.rs` は sql 表層の
/// 型に依存しないため、両関数の対で往復させる）。
fn expr_eval_error_to_arena(e: SqlSurfaceError) -> ArenaError {
    match e {
        SqlSurfaceError::PayloadTooLarge { detail } => {
            let _ = detail;
            ArenaError::CapacityExceeded
        }
        SqlSurfaceError::InvalidInput { detail } => ArenaError::InvalidInput(detail),
        other => ArenaError::Storage(StorageError::Codec(other.to_string())),
    }
}

fn map_kernel_error(_e: KernelError) -> SqlSurfaceError {
    // クエリの次元・有限性は `sql::parser::bind`（ベクトルリテラル解析）が既に
    // 検証済みのため、ここへ到達するのは provider 側の契約違反・実装バグのみ
    // （fail-closed に内部エラーへ丸め込む）。
    SqlSurfaceError::Internal {
        detail: "search provider error".to_string(),
    }
}

/// 投影段（`VECTOR` 列を返す `SELECT *`・明示投影）で、候補選択と同一スナップショット
/// の `embedding`（`arena` が保持するバッファのスライス）を応答行用に複製する
/// （Issue #56 レビュー指摘対応・codex P1: `catalog::validate_schema` は 1 テーブルに
/// つき `VECTOR` 列を高々 1 本しか許さず、`hits.len()` は候補集合（`arena`。既に
/// `arena::MAX_ARENA_TOTAL_BYTES` で総量検査済み）の部分集合であるため、この複製の
/// 論理上限は arena 自体の容量検査で既に閉じている。残る懸念は `Vec::to_vec`
/// （内部で `Vec::with_capacity` を使い、確保失敗時に abort する）が、arena の
/// 検査を通過した論理サイズであってもホスト側のメモリが実際に不足した場合に
/// プロセスを OOM abort させてしまう点。`try_reserve_exact` を使うことで、その場合を
/// abort ではなく `Err`（fail-closed に `PayloadTooLarge`）として呼び出し元へ伝える
/// （`arena.rs::try_reserve_exact`・security.md「不安全な設計｜無制限リソース確保
/// （DoS）」と同方針）。
fn try_clone_embedding(embedding: &[f32]) -> Result<Vec<f32>, SqlSurfaceError> {
    let mut out: Vec<f32> = Vec::new();
    out.try_reserve_exact(embedding.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("projected vector payload exceeds available memory")
    })?;
    out.extend_from_slice(embedding);
    Ok(out)
}

/// 投影段で `Text` 列を応答行用に複製する。`candidate_columns`（`on_visible_row`）
/// 側は [`MAX_CANDIDATE_SCALAR_BYTES`] で累計バイト量を検証済みだが、`String::clone`
/// （内部で `String::with_capacity` 相当を使い、確保失敗時に abort する）を素朴に
/// 使うと、その論理上限内であってもホスト側のメモリが実際に不足した場合に
/// プロセスを OOM abort させ得る（Issue #56 レビュー指摘対応・codex P1:
/// `decode_scalar_columns`／`candidate_columns` の複製に関する指摘と同根の問題が
/// 応答構築側にも残っていたための追加対応。[`try_clone_embedding`] と同方針で
/// `try_reserve_exact` を使い、失敗を abort ではなく `Err`（fail-closed に
/// `PayloadTooLarge`）へ変換する）。
fn try_clone_text(text: &str) -> Result<String, SqlSurfaceError> {
    let mut out = String::new();
    out.try_reserve_exact(text.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("projected text payload exceeds available memory")
    })?;
    out.push_str(text);
    Ok(out)
}

fn map_hybrid_error(e: HybridError) -> SqlSurfaceError {
    match e {
        HybridError::Sparse(SparseError::QueryTooLong { .. })
        | HybridError::Sparse(SparseError::TooManyQueryTerms { .. }) => {
            SqlSurfaceError::invalid_input("hybrid query text exceeds allowed length")
        }
        HybridError::Sparse(
            SparseError::DocTooLong { .. }
            | SparseError::TooManyDocs { .. }
            | SparseError::CorpusTooLarge { .. }
            | SparseError::TooManyTokens { .. },
        ) => SqlSurfaceError::payload_too_large("hybrid sparse corpus exceeds capacity"),
        _ => SqlSurfaceError::Internal {
            detail: "hybrid search error".to_string(),
        },
    }
}

/// [`BoundStatement`] を実行する（TASK-75 の公開 API）。`read_txn`・`schema` は
/// 呼び出し元（`core.rs::EngineCore::execute_sql`）が単一の read トランザクション上で
/// `catalog::get_table_schema_in_txn` により取得し、`sql::parser::bind` へ渡したのと
/// 同一のものを渡す契約とする（Issue #56 レビュー指摘対応・codex P1: スキーマ取得
/// （`bind` 用）と候補走査（[`VectorArena::build_filtered_with_rows_in_txn`]）が別
/// read トランザクションに分かれていると、その間に並行 DDL（`alter_table_add_column`
/// 等）がコミットされた場合、`bind` が束縛した旧スキーマで新スナップショットの行を
/// 走査することになり、新設列の欠落や `row_codec` デコード失敗（スキーマ世代不一致）
/// が生じ得た。同一 `read_txn` を型で強制することで、スキーマ取得・bind・候補走査を
/// 単一スナップショットへ閉じ込める）。
pub fn execute_statement(
    read_txn: &redb::ReadTransaction,
    provider: &dyn SearchProvider,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundStatement,
    precision_policy: &crate::precision::PrecisionPolicy,
) -> Result<QueryResult, SqlSurfaceError> {
    // TASK-162（対象ビヘイビア SEARCH-9）: `precision` の実行契約本体は
    // `crate::precision`（確信度判定・空集合 fail-closed 応答の純粋関数群）に
    // 切り出してあり、本関数は適用位置（DISTANCE 段＋事後 SCALAR フィルタの後・
    // `RlsSafetyNet::apply` の前）を決めるだけの薄い配線を担う（詳細は
    // `crate::precision` モジュールドキュメント「PRECISION ゲート段」参照）。
    // `recall`（既定）の実行経路はこの分岐に触れないため一切変わらない。
    let is_precision = bound.mode.mode == crate::sql::mode::SearchMode::Precision;

    // HINT ORDER（未指定なら既定の RLS→SCALAR→DISTANCE）から導出する実行方針。
    // `scalar_prefilter` のみが分岐点（`ExecutionPlan::from_evaluation_order` の
    // ドキュメント参照。TASK-76・SQL-7）。RLS 安全網はこの構造体に分岐用の
    // フィールドを持たせず、下記で無条件に呼び出す。
    let plan = ExecutionPlan::from_evaluation_order(bound.evaluation_order);

    // RLS 段（無条件）+ SCALAR 段（同一走査の行フック。`plan.scalar_prefilter` が
    // `false` の場合はここでは等価条件を判定せず、DISTANCE 段の後で事後適用する）。
    // 可視行だけがフックへ到達する（`arena::VectorArena::build_filtered_with_rows`
    // のドキュメント参照）。
    let mut sparse_docs: Vec<(u64, String)> = Vec::new();
    let is_hybrid = matches!(bound.ranking, Ranking::Hybrid { .. });
    let text_column_index = match &bound.ranking {
        Ranking::Hybrid {
            text_column_index, ..
        } => Some(*text_column_index),
        Ranking::Distance { .. } => None,
    };

    // 疎コーパスへ蓄積する累計文書数・バイト量。`sparse::SparseIndex::build` の上限
    // （`MAX_CORPUS_DOCS`・`MAX_CORPUS_BYTES`）は `try_alloc_text_for_budget` が
    // `String` 確保の**前**に検証する（.claude/rules/coding-rust.md「長さフィールドは
    // 上限検証してからアロケーションに使う」・security.md「不安全な設計｜無制限
    // リソース確保（DoS）」対応。可視行数の上限は `arena::MAX_ARENA_ROWS`（最大 100 万件）
    // だが、疎コーパス側の上限はそれよりずっと小さいため、蓄積前の検証がないと候補集合
    // 構築の途中で無制限に近いメモリを確保してから拒否することになる）。
    let mut sparse_bytes: usize = 0;

    // RLS → SCALAR 段（`on_visible_row`）で候補選択に使ったのと同一の read
    // トランザクション由来のデコード済みスカラー列を、投影段でそのまま再利用する
    // ために保持する（Issue #56 レビュー指摘対応・codex P1 / Bugbot Medium:
    // 以前は投影段で `storage.get_row_from_table` により id ごとに**別スナップショット**
    // を再取得しており、可視性のみ再検証してスカラー WHERE 条件は再検証していなかった
    // ため、候補選択後に行が更新されるとスカラー条件不一致の値や候補選択時と異なる
    // embedding を旧 score と組み合わせて返し得た。ここで候補構築時の行データを
    // 保持して投影に流用することで、SQL 実行全体を単一スナップショット
    // （`VectorArena::build_filtered_with_rows` が開く read トランザクション）に
    // 閉じ込め、再取得によるスナップショット不一致の窓をなくす）。
    //
    // 対応づけのキーは行 `id` ではなく**アリーナのスロット番号**
    // （`VectorArena::build_filtered_with_rows_in_txn` が `on_visible_row` の第 1 引数で
    // 渡す値。`arena.ids()` 等の添字と一致する）。行 `id` の一意性スコープはテナント内に
    // 閉じている（対象ビヘイビア: TABLE-12）ため、1 つの可視集合に同じ `id` の行が
    // 複数含まれうる（自テナント行と他テナントの `Public` 行）。`id` をキーにすると
    // embedding（アリーナ由来）とスカラー列（本表由来）が別テナントの行から混ざる
    // （Bugbot High 指摘）。スロット番号は `(tenant_id, id)` の行と 1 対 1 に対応するため、
    // 混線が構造的に起こらない。`Vec` の添字がスロット番号そのものであることは
    // `on_visible_row` が `true` を返した行だけが順番に push される契約で担保する。
    let mut candidate_columns: Vec<Vec<Value>> = Vec::new();

    // 投影段（下記ループ）が実際に参照する Text 列インデックスの集合。`VECTOR` 列は
    // `scan_scalar_columns` が常に `None` を返すだけ（実体は `arena` から引く）ため
    // 対象外。この集合に含まれない列は `on_visible_row` で借用のまま素通りし、
    // `String` として複製・保持しない（[`MAX_CANDIDATE_SCALAR_BYTES`] のドキュメント
    // 参照。投影に不要な列まで全候補分複製・保持する P1 指摘への対応）。
    // DISTANCE 先行時（`!plan.scalar_prefilter`）は SCALAR 条件を DISTANCE 段の後で
    // 事後適用するため、`metadata_filters`（TASK-147・EXT-3。等価・前方一致）が
    // 参照する列も候補構築時に保持しておく必要がある（TASK-76・SQL-7）。SCALAR
    // 先行時（既定）はここで条件を判定し終えるため、値の保持自体は不要（従来どおり
    // 投影列のみ保持する）。
    let mut needed_column_indices: HashSet<usize> = bound
        .projection
        .iter()
        .filter_map(|col| match col {
            ProjectedColumn::Column { index, .. } => Some(*index),
            // `Computed`（TASK-79・SQL-9）は候補構築時に保持した `candidate_columns`
            // （`Text` 列のデコード結果）を参照しない。評価に使うのは `arena` 由来の
            // `id`／embedding のみ（`sql::udf_call::eval` 参照。`TEXT` 列参照は
            // 束縛段で拒否済み）。
            ProjectedColumn::Id | ProjectedColumn::Computed { .. } => None,
        })
        .collect();
    if !plan.scalar_prefilter {
        needed_column_indices.extend(bound.metadata_filters.iter().map(|f| f.column_index()));
    }

    // `candidate_columns` に保持する Text 値の実バイト数に加え、行ごとに必ず確保する
    // `Vec<Value>` 自体の構造体サイズも累計する（[`MAX_CANDIDATE_SCALAR_BYTES`] の
    // ドキュメント参照。投影列を持たない `SELECT id ...` でも候補行数に比例して
    // 積み上がる構造体アロケーションを見逃さないため）。
    let mut candidate_scalar_bytes: usize = 0;
    let row_struct_bytes = schema
        .columns
        .len()
        .saturating_mul(std::mem::size_of::<Value>());

    // Issue #353: `expr_program::ExprProgram::eval` の明示スタック。
    // `on_visible_row`（下記。`FnMut` として `build_filtered_with_rows_in_txn`
    // へ渡される。`Send`/`Sync` は要求されないため単純な可変借用で足り、
    // `RefCell`/`Mutex` は不要）が行ごとに使い回す。
    let mut expr_scratch: Vec<udf_call::ExprValue> = Vec::new();

    let on_visible_row = |slot: usize,
                          id: u64,
                          embedding: &[f32],
                          metadata: &[u8]|
     -> std::result::Result<bool, ArenaError> {
        // Issue #56 レビュー指摘対応・codex P1: 旧実装は `decode_scalar_columns` で
        // 全 `Text` 列を無条件に `to_string()` 確保してから、投影に不要な列を
        // 捨てていた。最大長 `Text` 列を多数持つスキーマでは `SELECT id` のような
        // 値を保持しないクエリでも 1 行分の巨大な一時確保が先に発生し得た
        // （security.md「不安全な設計｜無制限リソース確保（DoS）」）。
        // `scan_scalar_columns` は構造検証のみを行い `Text` 値を `metadata` 借用の
        // `&str` として返すため、ここではヒープ確保が一切発生しない。フィルタ条件
        // の突合も借用のまま行い、投影・hybrid 本文として実際に必要な列だけを
        // 下のループで選択的に複製する。
        let scanned = row_codec::scan_scalar_columns(schema, metadata)
            .map_err(|e| ArenaError::Storage(StorageError::Codec(e.to_string())))?;
        // SCALAR 先行（既定）の場合のみここでメタデータフィルタ（TASK-147・EXT-3。
        // 等価・前方一致）を事前適用する。DISTANCE 先行（`HINT ORDER`）の場合は
        // 可視行を無条件に通過させ、DISTANCE 段の後で `apply_scalar_postfilter` が
        // 事後適用する（§モジュールドキュメント参照）。
        if plan.scalar_prefilter {
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                return Ok(false);
            }
            // TASK-79（SQL-9）: `WHERE` の式述語（宣言的 UDF・組み込み関数呼び出し）を
            // 既存のメタデータフィルタと同じ SCALAR 段の一部として事前適用する。可視行
            // （RLS-8 の暗黙適用を通過した行）にのみ到達するため、不可視行では
            // 式が一切評価されない。評価エラー（0 除算・非有限値等）は当該行を
            // 黙ってスキップせず、クエリ全体の失敗として fail-closed に伝播する
            // （`expr_eval_error_to_arena` 経由で `22000`／`54000` へ写像）。
            for program in &bound.expr_filter_programs {
                match program
                    .eval(id, embedding, &mut expr_scratch)
                    .map_err(expr_eval_error_to_arena)?
                {
                    udf_call::ExprValue::Bool(true) => {}
                    udf_call::ExprValue::Bool(false) => return Ok(false),
                    // 束縛段（`sql::parser::bind_in_session`）が `WHERE` 式述語の型を
                    // `Bool` に限定済みのため到達しない（`ExprType::Bool` 検査参照）。
                    _ => {
                        return Err(ArenaError::InvalidInput(
                            "WHERE expression did not evaluate to a boolean".to_string(),
                        ))
                    }
                }
            }
        }
        if is_hybrid {
            if let Some(idx) = text_column_index {
                if let Some(Some(t)) = scanned.get(idx) {
                    if sparse_docs.len() >= crate::sparse::MAX_CORPUS_DOCS {
                        return Err(ArenaError::CapacityExceeded);
                    }
                    let owned = try_alloc_text_for_budget(
                        t,
                        &mut sparse_bytes,
                        crate::sparse::MAX_CORPUS_BYTES,
                    )?;
                    // 疎コーパスの文書 id もスロット番号（`DocId = u64`）にする。
                    // 行 `id` を使うと、同一 `id` の可視行が 2 テナント分あるときに
                    // `SparseIndex::build` が `DuplicateDocId` で失敗し、ハイブリッド
                    // SQL 全体が落ちる（Bugbot Medium 指摘。対象ビヘイビア: TABLE-12）。
                    let doc_id = u64::try_from(slot).map_err(|_| ArenaError::CapacityExceeded)?;
                    sparse_docs.push((doc_id, owned));
                }
                // NULL 本文は疎側に含めない（密のみへ寄与する）。
            }
        }
        // スカラー条件（あれば）を通過した行のみを、投影段で再利用するために保持する。
        // ただし投影で実際に参照される列（`needed_column_indices`）だけを複製し、
        // それ以外は `Value::Null` として保持する（複製しない）。行を保持することで
        // 増える累計バイト量（行構造体分 `row_struct_bytes` を先に、Text 実体分は
        // 複製する列ごとに）を、それぞれの確保の**前**に検証し、上限超過は
        // fail-closed に `ArenaError::CapacityExceeded`（`map_arena_error` 経由で
        // `PayloadTooLarge`）として拒否する（投影列を持たない `SELECT id ...` でも、
        // 候補行数に比例する `Vec<Value>` 構造体分だけで無制限に積み上がらない
        // ようにする）。行単位の上限は「投影された列と hybrid 本文 1 本のみが複製
        // 対象になる」という構造そのものが担保する（必要な列だけを、必要な分だけ
        // 確保前に検証してから複製するため、行あたりの複製量は投影列の実バイト数
        // を超えない）。
        candidate_scalar_bytes = try_accumulate_budget(
            candidate_scalar_bytes,
            row_struct_bytes,
            MAX_CANDIDATE_SCALAR_BYTES,
        )?;
        // 投影で参照される列が 1 つも無い（`SELECT id ...`。TASK-83 条件7 の C1
        // 規範形）場合、下のループは全件 `Value::Null` を積むだけになる。その値は
        // `project_rows` で一切読まれない（`candidate_columns.get(slot)` は存在
        // 確認のみに使われ、`ProjectedColumn::Id` のみの投影では中身を見ない。
        // `project_rows` のドキュメント参照）ため、行数分のヒープ確保・push を
        // 省略できる（Issue #314・SQL-1・TASK-83 条件7: SQL 表層 C1 経路の固定
        // コスト削減）。`Vec::new()` は容量 0 でヒープ確保しない。スロット添字
        // 契約（`candidate_columns[slot]` が可視行と 1 対 1）は空 `Vec` でも
        // `candidate_columns.push` によって維持される。
        let kept: Vec<Value> = if needed_column_indices.is_empty() {
            Vec::new()
        } else {
            let mut kept: Vec<Value> = Vec::new();
            kept.try_reserve_exact(scanned.len()).map_err(|e| {
                ArenaError::AllocationFailed(format!("failed to reserve scalar column slots: {e}"))
            })?;
            for (idx, slot) in scanned.into_iter().enumerate() {
                if !needed_column_indices.contains(&idx) {
                    kept.push(Value::Null);
                    continue;
                }
                match slot {
                    None => kept.push(Value::Null),
                    Some(t) => {
                        let owned = try_alloc_text_for_budget(
                            t,
                            &mut candidate_scalar_bytes,
                            MAX_CANDIDATE_SCALAR_BYTES,
                        )?;
                        kept.push(Value::Text(owned));
                    }
                }
            }
            kept
        };
        // `slot` は「これから push される行の添字」（アリーナ側の契約）。ここでの
        // push により `candidate_columns[slot] == kept` が成立する。両者がずれた場合は
        // 後続の投影で誤った行を返しうるため、デバッグビルドで不変条件を固定する。
        debug_assert_eq!(candidate_columns.len(), slot);
        candidate_columns.push(kept);
        Ok(true)
    };

    let rls_hook = ImplicitRlsHook::new(ctx);
    let arena = VectorArena::build_filtered_with_rows_in_txn(
        read_txn,
        &bound.table,
        rls_hook.predicate(),
        on_visible_row,
    )
    .map_err(|e| map_arena_error(&bound.table, e))?;

    // provider へ渡す id は行 `id` ではなく**アリーナのスロット番号**（0..n）にする。
    // 行 `id` の一意性スコープはテナント内（対象ビヘイビア: TABLE-12）であり、1 つの
    // 可視集合に同じ `id` の行が複数含まれうるため、`id` のままでは
    // (a) provider 結果の重複検証、(b) RRF 融合の結合キー、(c) 投影段の行同定 の
    // いずれもが別テナントの行を取り違えうる（Bugbot High/Medium 指摘）。スロット番号は
    // 可視集合内で一意かつ `(tenant_id, id)` の行と 1 対 1 に対応するため、これらの
    // 契約が構造的に回復する。クライアントへ返す `ResultRow.id` は投影段で
    // `arena.ids()[slot]`（本来の行 id）へ戻す。
    let mut slot_ids: Vec<u64> = Vec::new();
    slot_ids
        .try_reserve_exact(arena.ids().len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve candidate slot ids: {e}"),
        })?;
    for slot in 0..arena.ids().len() {
        let slot_id = u64::try_from(slot).map_err(|_| SqlSurfaceError::Internal {
            detail: "candidate slot index does not fit in u64".to_string(),
        })?;
        slot_ids.push(slot_id);
    }
    // スロット番号は重複しないため、多重集合の各件数は必ず 1 になる
    // （`core::provider_result_is_valid` の (3)(4) 検証はそのまま使える）。
    let visible_id_counts = core::visible_id_counts(&slot_ids);

    // `precision` 時は DISTANCE 段の取得件数を `bound.limit` より広く取る
    // （`LIMIT 1` でも Top-2 を取得し、`precision::apply_gate` のマージン判定を
    // 行えるようにする。`crate::precision` モジュールドキュメント参照）。`recall`
    // 時は従来どおり `bound.limit`。
    //
    // `!plan.scalar_prefilter`（DISTANCE 先行・SCALAR 事後フィルタ経路）では
    // `bound.limit.max(2)` 件だけを取得すると、事後フィルタで候補が間引かれた
    // 結果「Top-2 が最初から存在しなかった」のか「取得件数不足で Top-2 を
    // 取りこぼしただけ」なのかを区別できない。後者を前者と誤認すると
    // `precision::apply_gate` が「Top-2 不在＝マージン条件成立」と誤判定し、
    // 本来は僅差で拒否すべき候補を通す fail-open 経路になる（codex-review・
    // Bugbot 指摘。SEARCH-9 は確信度不足時の fail-closed を要求する）。この
    // 経路のみ可視集合全体（`arena.ids().len()`。`ImplicitRlsHook` で RLS 済み）
    // を取得し、事後フィルタ後の残存件数が「WHERE を満たす候補の完全な順位列」
    // になるようにする。`core::MAX_SEARCH_K` でクランプする（下記 `const _` で
    // `2 <= core::MAX_SEARCH_K` を固定済みのため `k_eff` は常に provider・RRF
    // 設定の許容範囲に収まる）。
    let k_eff = if is_precision {
        if plan.scalar_prefilter {
            bound.limit.max(2)
        } else {
            arena.ids().len().clamp(2, core::MAX_SEARCH_K)
        }
    } else {
        bound.limit
    };

    // `!plan.scalar_prefilter`（DISTANCE 先行・SCALAR 事後フィルタ）の precision
    // 経路で `arena.ids().len()` が `core::MAX_SEARCH_K` を超える場合、直上の
    // `k_eff` は `MAX_SEARCH_K` へクランプされ、DISTANCE 段は可視集合全体ではなく
    // 先頭 `MAX_SEARCH_K` 件しか取得できない（provider の `k` 契約上限）。この場合
    // `MAX_SEARCH_K` 件目より後ろに WHERE を満たす僅差の Top-2 相当が存在しても
    // 取得できず、SCALAR 事後フィルタ後に「Top-2 を取りこぼしただけ」なのか
    // 「Top-2 が最初から存在しない」のかを再び区別できなくなる（直上のコメントが
    // 解消しようとした fail-open の再発。codex-review 指摘・PRRT_kwDOUAKASM6cPLHE）。
    // 「WHERE を満たす候補の完全な順位列を取得できる」という本経路の前提が崩れる
    // 以上、DISTANCE 検索自体を実行せず空集合へ倒す（`crate::precision` モジュール
    // ドキュメントが定める「確信度が判定できない＝空集合の通常応答」という既存の
    // fail-closed パターンに合わせる。完全性を保証できないこと自体は仕様上の
    // fail-closed 応答であり `SqlSurfaceError` への昇格対象ではない）。
    let completeness_unbounded =
        precision_completeness_unbounded(is_precision, plan.scalar_prefilter, arena.ids().len());

    // DISTANCE 段。
    let hits: Vec<(u64, f64)> = if completeness_unbounded {
        Vec::new()
    } else {
        match &bound.ranking {
            Ranking::Distance { query } => {
                let input = SearchInput {
                    ids: &slot_ids,
                    vectors: arena.vectors(),
                    dim: arena.dim(),
                    query,
                    k: k_eff,
                };
                let raw = provider.search(input).map_err(map_kernel_error)?;
                if !core::provider_result_is_valid(&raw, k_eff, &visible_id_counts) {
                    return Err(SqlSurfaceError::Internal {
                        detail: "search provider returned a result violating the top-k contract"
                            .to_string(),
                    });
                }
                raw.into_iter().map(|h| (h.id, h.score as f64)).collect()
            }
            Ranking::Hybrid {
                query, query_text, ..
            } => {
                let pool_depth = k_eff.max(DEFAULT_HYBRID_POOL_DEPTH);
                let cfg = RrfConfig::new(60.0, 1.0, 1.0, pool_depth).map_err(|_| {
                    SqlSurfaceError::Internal {
                        detail: "invalid hybrid RRF config".to_string(),
                    }
                })?;
                let input = SearchInput {
                    ids: &slot_ids,
                    vectors: arena.vectors(),
                    dim: arena.dim(),
                    query,
                    k: k_eff,
                };
                let fused: Vec<HybridHit> = if sparse_docs.is_empty() {
                    // 疎文書が 0 件（可視行に本文を持つ行が 1 件もない）の場合は
                    // `SparseIndex::build` が空コーパスを拒否する（`SparseError::EmptyCorpus`）
                    // ため、密側のみを `rrf_fuse` に通して密のみの順序契約へ縮退させる
                    // （疎側の寄与を単に 0 件として扱う。密側の可視性検証は
                    // `hybrid::hybrid_search` と同じ理由でここでも行う）。
                    let dense_input = SearchInput {
                        ids: &slot_ids,
                        vectors: arena.vectors(),
                        dim: arena.dim(),
                        query,
                        k: cfg.pool_depth(),
                    };
                    let dense_hits = provider.search(dense_input).map_err(map_kernel_error)?;
                    // 可視集合への包含判定（件数ではなく所属のみを見るため多重集合の
                    // キー存在で判定する。TABLE-12 の重複 id については
                    // `core::provider_result_is_valid` のドキュメント参照）。
                    if dense_hits
                        .iter()
                        .any(|h| !visible_id_counts.contains_key(&h.id))
                    {
                        return Err(SqlSurfaceError::Internal {
                            detail: "search provider returned a hit outside the visible id set"
                                .to_string(),
                        });
                    }
                    let mut fused =
                        hybrid::rrf_fuse(&dense_hits, &[], &cfg).map_err(map_hybrid_error)?;
                    fused.truncate(k_eff);
                    fused
                } else {
                    let doc_refs: Vec<(DocId, &str)> = sparse_docs
                        .iter()
                        .map(|(id, text)| (*id, text.as_str()))
                        .collect();
                    let sparse_index = SparseIndex::build(&doc_refs)
                        .map_err(HybridError::Sparse)
                        .map_err(map_hybrid_error)?;
                    hybrid::hybrid_search(provider, input, &sparse_index, query_text, k_eff, &cfg)
                        .map_err(map_hybrid_error)?
                };
                fused.into_iter().map(|h| (h.id, h.score)).collect()
            }
        }
    };

    // SCALAR 事後フィルタ（DISTANCE 先行時のみ。TASK-76・SQL-7）。`on_visible_row` は
    // `plan.scalar_prefilter == false` の間、等価条件を判定せず可視行を通過させて
    // いるため、ここで `candidate_columns`（`needed_column_indices` により対象列を
    // 保持済み）と突合する。不一致・値の取得不能（データ不整合。fail-closed）は
    // 除去する。返却件数が `limit` 未満になり得る（under-fetch。オーバーサンプルに
    // よる救済は行わない）。
    let hits: Vec<(u64, f64)> = if plan.scalar_prefilter {
        hits
    } else {
        // TASK-79（SQL-9）: `WHERE` の式述語も既存のメタデータフィルタ（TASK-147・
        // EXT-3）と同じ SCALAR 事後フィルタの一部として扱う（DISTANCE 先行時。
        // §モジュールドキュメント参照）。評価に使う embedding は候補選択と同一
        // スナップショットの `arena` から（投影段と同じ経路。再取得なし）。
        // 評価エラーはこの場で fail-closed にクエリ全体の失敗として伝播する
        // （当該行だけを黙ってスキップしない。`on_visible_row` の事前フィルタ経路と
        // 同じ方針）。
        let mut filtered = Vec::with_capacity(hits.len());
        // Issue #353: DISTANCE 段の後で事後適用する `expr_filter_programs` の
        // 明示スタック（`on_visible_row` とは別ループのため個別に確保。行数分
        // 使い回す）。
        let mut expr_scratch: Vec<udf_call::ExprValue> = Vec::new();
        for (slot_id, score) in hits {
            // `slot_id` はアリーナのスロット番号（上記参照）。範囲外はデータ不整合
            // として fail-closed に除去する。
            let Some(slot) = usize::try_from(slot_id).ok() else {
                continue;
            };
            let Some(columns) = candidate_columns.get(slot) else {
                continue;
            };
            let scanned: Vec<Option<&str>> = columns
                .iter()
                .map(|v| match v {
                    Value::Text(t) => Some(t.as_str()),
                    Value::Null | Value::Vector(_) => None,
                })
                .collect();
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            if !bound.expr_filter_programs.is_empty() {
                let Some(embedding) = arena.vector(slot) else {
                    continue;
                };
                let Some(row_id) = arena.ids().get(slot).copied() else {
                    continue;
                };
                let mut expr_ok = true;
                for program in &bound.expr_filter_programs {
                    match program.eval(row_id, embedding, &mut expr_scratch)? {
                        udf_call::ExprValue::Bool(true) => {}
                        udf_call::ExprValue::Bool(false) => {
                            expr_ok = false;
                            break;
                        }
                        // 束縛段が型を `Bool` に限定済みのため到達しない。
                        _ => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "WHERE expression did not evaluate to a boolean"
                                    .to_string(),
                            })
                        }
                    }
                }
                if !expr_ok {
                    continue;
                }
            }
            filtered.push((slot_id, score));
        }
        filtered
    };

    // PRECISION ゲート段（TASK-162・SEARCH-9）。SCALAR 事後フィルタの**後**・
    // `RlsSafetyNet::apply` の**前**に置く（`crate::precision` モジュール
    // ドキュメント参照）。`WHERE` を満たす行だけを対象に Top-1／Top-2 の確信度を
    // 見るため SCALAR 段より後、`RlsSafetyNet` は行を「減らす」ことしかしないため
    // ゲート通過後に安全網が行を落としても確信のない行が増える方向にはならず、
    // この位置より前に置く必要がある。候補集合自体は `ImplicitRlsHook` により
    // 事前フィルタ済み（`arena` 構築時）のため、他テナント不可視行が Top-1／Top-2
    // の比較対象に混入することはない。`HINT ORDER` の内容に関係なく無条件に適用
    // する（`is_precision` は `bound.mode` のみに依存し、`plan.scalar_prefilter`
    // の分岐に触れない）。`recall`（既定）はこのブロックを一切通らないため、
    // 挙動は変わらない（SEARCH-1〜8 不変）。
    let hits: Vec<(u64, f64)> = if is_precision {
        // 確信度指標はランキング方式ごとに正規化する（`crate::precision` モジュール
        // ドキュメント参照）: dense はクエリ・候補 embedding の cosine 類似度、
        // hybrid は融合スコアを理論最大値で割った正規化 RRF スコア。確信度は
        // 先頭 `min(limit, max_results) + 1` 件分だけ計算する（有界・小規模。
        // DoS 対策）。
        let thresholds = match &bound.ranking {
            Ranking::Distance { .. } => precision_policy.dense(),
            Ranking::Hybrid { .. } => precision_policy.hybrid(),
        };
        let want = bound
            .limit
            .min(precision_policy.max_results())
            .saturating_add(1);
        let take_n = want.min(hits.len());
        let mut conf: Vec<f64> = Vec::with_capacity(take_n);
        match &bound.ranking {
            Ranking::Distance { query } => {
                for (slot_id, _score) in hits.iter().take(take_n) {
                    let slot =
                        usize::try_from(*slot_id).map_err(|_| SqlSurfaceError::Internal {
                            detail: "candidate slot index out of range".to_string(),
                        })?;
                    let embedding =
                        arena
                            .vector(slot)
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "candidate arena index out of range".to_string(),
                            })?;
                    // ノルム 0・次元不一致・非有限値は「確信なし」（`0.0`）として
                    // 扱う。`ConfidenceThresholds::new` が閾値を厳密に正へ限定して
                    // いるため、`0.0` は常に `min_top1` 未満となり空集合へ倒れる
                    // （fail-closed。`crate::precision::cosine_similarity` の
                    // ドキュメント参照）。
                    conf.push(crate::precision::cosine_similarity(query, embedding).unwrap_or(0.0));
                }
            }
            Ranking::Hybrid { .. } => {
                // 正規化に使う重み・ランク減衰定数は DISTANCE 段で使ったものと
                // 同一の固定値（`60.0, 1.0, 1.0`）で、`pool_depth` は正規化の
                // 計算式に現れないため既定値で構わない
                // （`crate::precision::rrf_normalized` のドキュメント参照）。
                let cfg = RrfConfig::default();
                for (_slot_id, score) in hits.iter().take(take_n) {
                    conf.push(crate::precision::rrf_normalized(*score, &cfg).unwrap_or(0.0));
                }
            }
        }
        let n = crate::precision::apply_gate(
            &conf,
            &thresholds,
            bound.limit,
            precision_policy.max_results(),
        )
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("precision gate contract violation: {e}"),
        })?;
        let mut truncated = hits;
        truncated.truncate(n);
        truncated
    } else {
        hits
    };

    // RLS 実行時安全網（TASK-136・RLS-5）。`HINT ORDER` の内容に関係なく常に適用する
    // （モジュールドキュメント参照）。`ExecutionPlan` にはこの適用を分岐させる
    // フィールドを持たせておらず（`plan.rs` のドキュメント参照）、呼び出しを
    // 迂回する経路が型として存在しない。`arena` は候補構築と同一スナップショットの
    // テナント・可視性ラベルを保持しているため、`storage` の再取得なしに安全網を
    // 評価できる。現状は事前フィルタと同じ `arena` から再判定するため、この安全網
    // 単体で不可視行を追加で落とすことはない（defense-in-depth。モジュール
    // ドキュメント参照）。戻り値の [`RlsVerifiedHits`]（witness 型）は
    // [`project_rows`] へのみ渡り、安全網を経由しない生の `hits` から投影へ
    // 到達する経路は型として存在しない（`rls.rs` の型ドキュメント参照）。
    // `hits` の第 1 要素はアリーナのスロット番号。範囲外のスロット（provider の契約
    // 違反・データ不整合）は `None` を返し、`RlsSafetyNet::apply` が fail-closed に
    // 除去する（従来の「id が索引に無い」case と同じ扱い）。
    let verified: RlsVerifiedHits = RlsSafetyNet::new(ctx).apply(hits, |slot_id| {
        let slot = usize::try_from(slot_id).ok()?;
        let tenant = arena.tenant_id(slot)?;
        let visibility = arena.visibility(slot)?;
        Some((tenant, visibility))
    });

    let rows = project_rows(
        verified,
        &bound.projection,
        schema,
        &arena,
        &candidate_columns,
    )?;

    let columns = bound
        .projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Id => ColumnMeta::Id,
            ProjectedColumn::Column { index, name } => ColumnMeta::Scalar {
                name: name.clone(),
                ty: schema
                    .columns
                    .get(*index)
                    .map(|c| c.ty)
                    .unwrap_or(ColumnType::Text),
            },
            ProjectedColumn::Computed { name, .. } => ColumnMeta::Computed { name: name.clone() },
        })
        .collect();

    Ok(QueryResult { columns, rows })
}

/// 投影段（TASK-136・RLS-5）。引数を [`RlsVerifiedHits`]（witness 型）に固定する
/// ことで、[`RlsSafetyNet::apply`] を経由しない生の `Vec<(u64, f64)>` から本関数へ
/// 到達する経路を型として作れなくする（`execute_statement` からのみ呼ばれる）。
/// `storage` への再取得は行わず、候補選択（`build_filtered_with_rows`）と同一
/// スナップショットで保持しておいた embedding（`arena`）・デコード済みスカラー列
/// （`candidate_columns`）から返却行を構築する（候補選択後に対象行が更新・削除
/// されても、投影は候補選択時点の値を返すため、RLS・スカラー WHERE・embedding が
/// 候補選択と投影とで食い違うことはない）。
fn project_rows(
    verified: RlsVerifiedHits,
    projection: &[ProjectedColumn],
    schema: &TableSchema,
    arena: &VectorArena,
    candidate_columns: &[Vec<Value>],
) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    let hits = verified.into_hits();
    let mut rows = Vec::with_capacity(hits.len());
    // Issue #353: `ProjectedColumn`（`pub` enum）の形状は変えず、`Computed` 列の
    // 式を行ループの**外**（本関数の入口）で 1 回だけステップ列コンパイルする。
    // `projection` と添字が 1 対 1 対応する（`Computed` 以外は `None`）。行ループは
    // 明示スタック（`expr_scratch`）を使い回し、`sql::udf_call::eval` の再帰
    // 呼び出しを一切行わない。
    let computed_programs: Vec<Option<crate::sql::expr_program::ExprProgram>> = projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Computed { expr, .. } => {
                Some(crate::sql::expr_program::ExprProgram::compile(expr))
            }
            ProjectedColumn::Id | ProjectedColumn::Column { .. } => None,
        })
        .collect();
    let mut expr_scratch: Vec<udf_call::ExprValue> = Vec::new();
    for (slot_id, score) in hits {
        // ヒットの第 1 要素はアリーナのスロット番号（`execute_statement` の
        // `slot_ids` 参照）。embedding・スカラー列・行 `id` の 3 者すべてを同じ
        // スロットから引くため、同一 `id` の別テナント行が混ざる余地がない
        // （対象ビヘイビア: TABLE-12。Bugbot High 指摘への対応）。
        let slot = usize::try_from(slot_id).map_err(|_| SqlSurfaceError::Internal {
            detail: "candidate slot index out of range".to_string(),
        })?;
        let embedding = arena
            .vector(slot)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "candidate arena index out of range".to_string(),
            })?;
        let decoded = candidate_columns
            .get(slot)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "search hit is missing from candidate scalar columns".to_string(),
            })?;
        // クライアントへ返す id は本来の行 `id`（スロット番号ではない）。
        let id = *arena
            .ids()
            .get(slot)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "candidate arena index out of range".to_string(),
            })?;
        let mut cells = Vec::with_capacity(projection.len());
        for (col_idx, col) in projection.iter().enumerate() {
            match col {
                ProjectedColumn::Id => cells.push(Cell::Integer(id)),
                ProjectedColumn::Column { index, .. } => {
                    let column =
                        schema
                            .columns
                            .get(*index)
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "projected column index out of range".to_string(),
                            })?;
                    match column.ty {
                        ColumnType::Vector(_) => {
                            cells.push(Cell::Vector(try_clone_embedding(embedding)?))
                        }
                        ColumnType::Text => match decoded.get(*index) {
                            Some(Value::Text(t)) => cells.push(Cell::Text(try_clone_text(t)?)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Vector(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                    }
                }
                ProjectedColumn::Computed { .. } => {
                    // TASK-79（SQL-9）: 結果列位置の式項目。候補選択と同一スナップショット
                    // の `id`・embedding で評価する（投影段は再取得を行わない既存契約と
                    // 同方針）。評価エラーはクエリ全体の失敗として fail-closed に伝播する
                    // （行値・テナントを含まない固定文言。`sql::udf_call::eval` 参照）。
                    // Issue #353: `expr` の再帰評価ではなく、入口で 1 回だけ
                    // コンパイルした `computed_programs[col_idx]` を線形実行する。
                    let program = computed_programs
                        .get(col_idx)
                        .and_then(|p| p.as_ref())
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "computed projection program missing at evaluation time"
                                .to_string(),
                        })?;
                    match program.eval(id, embedding, &mut expr_scratch)? {
                        udf_call::ExprValue::Scalar(v) => cells.push(Cell::Float(v)),
                        udf_call::ExprValue::Vector(v) => cells.push(Cell::Vector(v)),
                        udf_call::ExprValue::Bool(b) => cells.push(Cell::Bool(b)),
                    }
                }
            }
        }
        rows.push(ResultRow { id, score, cells });
    }
    Ok(rows)
}

/// [`crate::sql::parser::BoundInsert`] を実行する（SQL-10、TASK-80）。
/// `core.rs::EngineCore::execute_insert_sql` からのみ呼ばれる想定で、`Storage`・
/// `PolicyContext` を束ねる（`execute_statement` と対称の役割）。クレート外へ公開する
/// 契約は持たず、可視性は `pub(crate)` に留める（TASK-93・codex-review P1 指摘・PR #226:
/// `ledger_mode` 引数の追加をソース互換性の破壊的変更として扱う必要をなくすため）。
///
/// 行の書き込みはガードなし実体 [`crate::tenant::insert_typed_row_unchecked`]（`pub(crate)`）
/// へ委譲する（TASK-95・TABLE-12・RLS-9。`operation_id` 必須化ガードは本関数の
/// 呼び出し前に `sql::allowlist::validate_insert` が適用済みのため、ガード付き公開版
/// `crate::tenant::insert_typed_row` は経由しない。TASK-92・RECOVER-1・
/// codex-review P1 指摘・PR #217）。`catalog.rs` の生の挿入 API は `pub(crate)` かつ
/// テナント名前空間の指定を呼び出し元任せにするため、SQL 表層からは使わない
/// （ガードを迂回できる書き込み入口を増やさない。security.md P0）。テナントは
/// `ctx.tenant_id()` からサーバー側で導出され（クライアントが列リストへテナント
/// 相当の値を指定しても無視される）、可視性は常に `Visibility::Private` に固定する
/// （`PolicyContext::is_visible` は `Public` 行を他テナントへも可視とするため、
/// 既定 `Public` は越境露出になる。fail-closed に `Private` を採用する）。
///
/// 単一の write トランザクションで完結し、既存 `id` への黙った上書きは行わない。
/// 重複検出のスコープは**呼び出し元テナントの名前空間内**に閉じる（行ストアの物理
/// キーが `(tenant_id, id)` であるため。TABLE-12・RLS-9）。したがって:
///
/// - 同一テナント内の `id` 重複は [`SqlSurfaceError::IdConflict`]（`23505`）
/// - 同一 `(tenant_id, table, operation_id)` への 2 回目以降の書き込みは
///   [`SqlSurfaceError::DuplicateOperationId`]（`23505`。TASK-94・対象ビヘイビア:
///   RECOVER-3）。台帳追記が行書き込みより先に同一 write トランザクション内で行われる
///   ため（`tenant.rs` モジュールドキュメント参照）、同一文の再送はこちらが先に検出する
/// - 他テナントが同じ `id`／`operation_id` を保持していても本経路は成功する
///   （応答・実行経路のいずれも他テナントの存在で分岐しないため、存在オラクルに
///   ならない）
///
/// `23505` は行キー `(tenant_id, id)` の衝突と `operation_id` の重複という 2 つの
/// 別原因を共有する（`error_format.rs::ErrorClass::UniqueViolation` 参照）。
/// 内容不一致検出（同一 `operation_id`・異なる内容）は TASK-101、対象ビヘイビア:
/// RECOVER-10 の管轄で未提供。
///
/// `ledger_mode` は `core.rs::EngineCore` が保持する構成をそのまま受け取り、
/// `ledger_mode.resolve(bound.operation_id.as_ref())` で
/// [`crate::recovery::ledger::LedgerWrite`] へ変換したうえで
/// [`crate::tenant::insert_typed_row_unchecked`] へ渡す（TASK-93、対象ビヘイビア:
/// RECOVER-2。台帳への追記が行書き込みと同一 write トランザクションで行われる点は
/// `tenant.rs` モジュールドキュメント参照）。`bound.operation_id` の必須化
/// （TASK-92・RECOVER-1）は呼び出し元 `sql::allowlist::validate_insert` が
/// `LedgerMode::require` 経由で本関数の呼び出し前に適用済みのため、
/// `LedgerMode::Ledgered`（既定）では常に `Some`。`resolve` はその判定を再利用する
/// ため、ここで改めて `MissingOperationId` を返す経路は実質到達しない
/// （`LedgerMode::CompareOnlyWithoutLedger` でのみ `None` になり得るが、その場合
/// `resolve` は `LedgerWrite::Disabled` を返し、台帳へは触れない）。
pub(crate) fn execute_insert(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundInsert,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<InsertOutcome, SqlSurfaceError> {
    use crate::catalog::CatalogError;
    use crate::storage::{StorageError, Visibility};
    use crate::tenant::TenantWriteError;

    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    crate::tenant::insert_typed_row_unchecked(
        storage,
        &bound.table,
        ctx,
        bound.id,
        Visibility::Private,
        &bound.values,
        ledger_write,
    )
    .map_err(|e| match e {
        // 同一テナント内の id 重複（`23505`）。SQL-10 の再送判定が識別できるよう、
        // 値不正（`22000`）へ丸めずに専用の wire_code を維持する。
        TenantWriteError::IdConflict => SqlSurfaceError::IdConflict,
        // `tenant::insert_typed_row_unchecked` 自体は `operation_id` 必須化ガード
        // （`recovery::required_op_id::LedgerMode`）を持たない（`tenant.rs` モジュール
        // ドキュメント参照）。本経路（SQL `INSERT`）ではガードを
        // `sql::allowlist::validate_insert` が書き込みトランザクション開始前に
        // 既に適用済みのため、この写像アームは実際には到達しない。ただし
        // `TenantWriteError` の網羅性を保ち、`23502` を返す正しい写像を明示しておく
        // （TASK-92・対象ビヘイビア: RECOVER-1）。
        TenantWriteError::MissingOperationId => SqlSurfaceError::MissingOperationId,
        // 台帳照合（TASK-101・RECOVER-10）: 同一 operation_id・同一内容の再送は
        // `23505`、内容不一致（v1 レガシーエントリへの再送を含む）は `22023` へ
        // 写像する。`_` 節（`XX000`）へ丸めると、再送検知の応答をクライアントが
        // 判別できなくなる。
        TenantWriteError::DuplicateOperationId => SqlSurfaceError::DuplicateOperationId,
        TenantWriteError::OperationIdContentMismatch => SqlSurfaceError::OperationIdContentMismatch,
        TenantWriteError::Catalog(CatalogError::TableNotFound(name)) => {
            SqlSurfaceError::UndefinedTable { name }
        }
        // 入力値に起因すると型で確認できるものだけを「受理構文だが値が不正」として
        // `22000` へ丸め込む（`CatalogError::Invalid` は識別子・次元・スキーマ検証の
        // 失敗、`StorageError::Codec` は行エンコード時の不正値）。エラー文言に行内容・
        // 所有テナントは含めない（security.md「エラー・ログ経由で他テナントのデータ・
        // 存在情報を漏らさない」）。`TenantWriteError::LedgerCorrupted`（`op_ledger` の
        // 未知フォーマット・バックエンド障害）はここに含めない: 台帳破損は送信された
        // 行データとは無関係のサーバー内部事象であり、クライアントの行を「不正」と
        // 誤認させると誤った再試行を誘発する（Cursor Bugbot 指摘・PR #226）。下の
        // `_` 節（`XX000`）へ委ねる。
        TenantWriteError::Catalog(CatalogError::Invalid(_))
        | TenantWriteError::Storage(StorageError::Codec(_)) => {
            SqlSurfaceError::invalid_input("insert rejected: invalid row")
        }
        // それ以外（redb バックエンド障害・commit 失敗・カタログ破損・認可失敗・
        // 台帳破損等）はサーバー側の内部事象として `XX000` へ写像する
        // （codex-review P0/P1 指摘・PR #189: バックエンド障害を入力不正として返すと
        // 再試行・障害判定を誤らせる。また `TenantWriteError` の `Display`/`Debug` は
        // 原因を秘匿する契約のため、detail には原因を一切展開せず固定文言に留める）。
        _ => SqlSurfaceError::Internal {
            detail: "insert failed".to_string(),
        },
    })?;

    Ok(InsertOutcome {
        rows_affected: 1,
        incremental: None,
    })
}

/// ファイル形 `INSERT`（TASK-120・対象ビヘイビア: INDEX-1, INDEX-2）を実行する。
/// `sql::parser::bind_insert_form` が判別した `BoundFileInsert` を受け取り、
/// `incremental::index_file` へ委譲する。`embedder` が未設定（`None`）の場合は
/// fail-closed に拒否する（意味のないベクトルが黙って索引化される fail-open を
/// 防ぐ。`core.rs::EngineCore` モジュールドキュメント参照）。
///
/// `IncrementalError` → `SqlSurfaceError` の写像をここに集約する（他の SQL 表層
/// エラー写像と同じ「呼び出し元の型で fail-closed に丸める」方針。security.md
/// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応）。
/// 可視性: `operation_id` 必須化ガード（TASK-92・RECOVER-1）は呼び出し元
/// `core::EngineCore::execute_insert_sql` が `sql::allowlist::validate_insert` 経由で
/// 適用済みであり、本関数自体はガードを持たない。クレート外へ公開すると
/// `BoundFileInsert` を直接構築してガードを迂回できるため `pub(crate)` に閉じる
/// （codex-review P1 指摘・PR #221。security.md P0）。
pub(crate) fn execute_file_insert(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    embedder: Option<&dyn crate::embedding::Embedder>,
    config: &crate::incremental::IncrementalConfig,
    bound: &crate::sql::parser::BoundFileInsert,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<InsertOutcome, SqlSurfaceError> {
    // 行形 `INSERT`（[`execute_insert`]）と同一の契約で `operation_id` を解決し、
    // 台帳書き込み指示（`LedgerWrite`）へ写像する（TASK-93・RECOVER-2）。
    // `LedgerMode::Ledgered` で `operation_id` が無い場合は `23502`（この分岐は
    // `sql::allowlist::validate_insert` が既に拒否済みのため通常到達しない）。
    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let embedder = embedder.ok_or_else(|| SqlSurfaceError::Internal {
        detail: "no embedder configured for file-form insert".to_string(),
    })?;

    let input = crate::incremental::BoundFileIndexInput {
        table: &bound.table,
        path: &bound.path,
        body: &bound.body,
        template_values: &bound.template_values,
        path_column_index: bound.path_column_index,
        body_column_index: bound.body_column_index,
        vector_column_index: bound.vector_column_index,
    };

    let outcome =
        crate::incremental::index_file(storage, ctx, embedder, config, &input, ledger_write)
            .map_err(map_incremental_error)?;

    Ok(InsertOutcome {
        rows_affected: outcome.rows_replaced as u64,
        incremental: Some(outcome),
    })
}

/// [`crate::incremental::IncrementalError`] を SQL 表層のエラー契約へ写像する
/// （`execute_file_insert` の唯一の呼び出し元。detail はサーバー内部の固定文言・
/// チャンク数等の非機密情報のみで、本文・応答本文は含めない）。
fn map_incremental_error(e: crate::incremental::IncrementalError) -> SqlSurfaceError {
    use crate::incremental::IncrementalError;
    use crate::tenant::TenantWriteError;

    match e {
        IncrementalError::ChunkingTooLarge(detail) => SqlSurfaceError::payload_too_large(detail),
        // サーバー構成の誤り・メモリ逼迫はクライアント入力起因の `54000` ではなく
        // 内部失敗 `XX000` として返す（Cursor Bugbot 指摘・PR #221）。
        IncrementalError::Internal(detail) => SqlSurfaceError::Internal {
            detail: detail.to_string(),
        },
        IncrementalError::Embed(crate::embedding::EmbedError::TooManyInputs { len, max }) => {
            SqlSurfaceError::payload_too_large(format!(
                "embedding batch too large: {len} inputs (max {max})"
            ))
        }
        IncrementalError::Embed(_) => SqlSurfaceError::Internal {
            detail: "embedding failed".to_string(),
        },
        IncrementalError::Write(TenantWriteError::Catalog(CatalogError::TableNotFound(name))) => {
            SqlSurfaceError::UndefinedTable { name }
        }
        IncrementalError::Write(TenantWriteError::Catalog(CatalogError::Invalid(_))) => {
            SqlSurfaceError::invalid_input("insert rejected: invalid row")
        }
        // 台帳照合（TASK-101・RECOVER-10。TASK-94・RECOVER-3 の重複拒否契約を包含する）:
        // ファイル形 INSERT（`replace_typed_rows_by_text_key` 経由）でも行形 INSERT
        // （[`execute_insert`]）と同じ写像を適用する。`_` 節（`XX000`）へ丸めない。
        IncrementalError::Write(TenantWriteError::DuplicateOperationId) => {
            SqlSurfaceError::DuplicateOperationId
        }
        IncrementalError::Write(TenantWriteError::OperationIdContentMismatch) => {
            SqlSurfaceError::OperationIdContentMismatch
        }
        IncrementalError::Write(_) => SqlSurfaceError::Internal {
            detail: "incremental index write failed".to_string(),
        },
        IncrementalError::EmptyChunks => {
            SqlSurfaceError::invalid_input("insert rejected: file body produced no chunks to index")
        }
    }
}

/// [`crate::incremental::BatchIncrementalError`] を SQL 表層のエラー契約へ写像する
/// （TASK-122・対象ビヘイビア: INDEX-4。`core::EngineCore::execute_insert_sql_batch`
/// の唯一の呼び出し元）。一括投入 4 上限（`crate::batch_limits::BatchLimitsError`）は
/// 全 variant が `54000`（`payload_too_large`。ERR-2・TASK-152）へ写像する。個々の
/// ファイルに起因する非上限系の失敗（`Item`）は単一ファイル経路と同じ写像
/// （[`map_incremental_error`]）を再利用し、バッチ内のどのファイルかは detail に
/// 含めない（本文・パス・テナント情報を含めない契約は単一ファイル経路と同一）。
/// 特定ファイルに起因しないバッチ全体の内部エラー（`Internal`。一括バッファの
/// 確保拒否等）は `Internal` 系へ写像する。
pub(crate) fn map_batch_incremental_error(
    e: crate::incremental::BatchIncrementalError,
) -> SqlSurfaceError {
    use crate::incremental::BatchIncrementalError;

    match e {
        BatchIncrementalError::Limits(limits_err) => {
            SqlSurfaceError::payload_too_large(limits_err.to_string())
        }
        BatchIncrementalError::Item { source, .. } => map_incremental_error(source),
        BatchIncrementalError::Internal(detail) => SqlSurfaceError::Internal {
            detail: detail.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #56 レビュー指摘対応・codex P1 の 2 件（`decode_scalar_columns` の
    // 無制限保持・`embedding.to_vec()` の abort し得る確保）に対する境界値テスト。
    // 本体（`execute_statement`）を通した巨大データでの再現は非現実的なため、
    // 上限判定を担う純粋関数を直接検証する（`arena.rs`・`sparse.rs` の
    // 境界値テストと同方針）。

    #[test]
    fn try_accumulate_budget_accepts_up_to_cap_exactly() {
        assert_eq!(try_accumulate_budget(90, 10, 100).unwrap(), 100);
    }

    #[test]
    fn try_accumulate_budget_rejects_one_byte_over_cap() {
        assert!(matches!(
            try_accumulate_budget(90, 11, 100),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn try_accumulate_budget_saturates_instead_of_overflowing() {
        // `saturating_add` により usize::MAX 近傍でもオーバーフローで未定義動作にならず、
        // 必ず `CapacityExceeded` として拒否される（coding-rust.md「整数演算は
        // checked_*／saturating_* を使う」対応）。
        assert!(matches!(
            try_accumulate_budget(usize::MAX - 1, 10, 100),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    // codex-review 指摘（PRRT_kwDOUAKASM6cPLHE）の回帰テスト: `!scalar_prefilter`
    // の precision 経路で可視集合が `core::MAX_SEARCH_K` を超えるかどうかの判定
    // （`k_eff` クランプにより完全な順位列を構築できなくなる境界）。本体
    // （`execute_statement`）を `MAX_SEARCH_K` 超の巨大データで再現するのは
    // 非現実的なため、上限判定を担う純粋関数を直接検証する（`try_accumulate_budget`
    // と同方針）。

    #[test]
    fn precision_completeness_unbounded_false_when_recall_mode() {
        // `is_precision == false`（recall）は本経路自体を通らないため常に `false`。
        assert!(!precision_completeness_unbounded(
            false,
            false,
            core::MAX_SEARCH_K + 1
        ));
    }

    #[test]
    fn precision_completeness_unbounded_false_when_scalar_prefilter() {
        // SCALAR 先行経路は `k_eff = bound.limit.max(2)` を使い、可視集合の広さに
        // 依存しないため常に `false`。
        assert!(!precision_completeness_unbounded(
            true,
            true,
            core::MAX_SEARCH_K + 1
        ));
    }

    #[test]
    fn precision_completeness_unbounded_false_at_max_search_k_boundary() {
        // 可視集合が `MAX_SEARCH_K` ちょうどなら `k_eff` はクランプされず完全な
        // 順位列を取得できるため `false`。
        assert!(!precision_completeness_unbounded(
            true,
            false,
            core::MAX_SEARCH_K
        ));
    }

    #[test]
    fn precision_completeness_unbounded_true_one_over_max_search_k() {
        // 1 件でも超過すれば `k_eff` がクランプされ完全性を保証できなくなるため
        // `true`（呼び出し元は空集合へ倒す）。
        assert!(precision_completeness_unbounded(
            true,
            false,
            core::MAX_SEARCH_K + 1
        ));
    }

    #[test]
    fn try_clone_embedding_copies_values_without_aliasing_source() {
        let source = vec![1.0f32, 2.0, 3.0];
        let cloned = try_clone_embedding(&source).expect("small copy must succeed");
        assert_eq!(cloned, source);
    }

    #[test]
    fn try_clone_embedding_handles_empty_slice() {
        let cloned = try_clone_embedding(&[]).expect("empty copy must succeed");
        assert!(cloned.is_empty());
    }

    #[test]
    fn try_clone_text_copies_without_aliasing_source() {
        let source = "hello world".to_string();
        let cloned = try_clone_text(&source).expect("small copy must succeed");
        assert_eq!(cloned, source);
    }

    #[test]
    fn try_clone_text_handles_empty_string() {
        let cloned = try_clone_text("").expect("empty copy must succeed");
        assert!(cloned.is_empty());
    }
}
