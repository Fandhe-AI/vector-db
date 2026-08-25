//! SQL 表層の実行計画層（TASK-75、対象ビヘイビア: SQL-1, SQL-2, SQL-3, SQL-4。
//! ポインタ: `docs/spec/05-tasks.md` TASK-75・`docs/spec/04-behavior/sql-surface.md`）。
//!
//! 責務境界: [`parser::bind`](crate::sql::parser::bind) が返す [`BoundStatement`] を、
//! 固定順序 **RLS → SCALAR → DISTANCE** で実行する（`HINT ORDER` による順序上書きは
//! TASK-76 の管轄でありここでは提供しない）。RLS 段は `WHERE` 句の `visible()` 呼び出し
//! （[`BoundStatement::rls_predicate_present`]）の有無に**関係なく**無条件に適用する
//! （SQL-3・RLS-7: RLS 強制は述語の有無に依存しない。security.md P0「テナント分離の
//! 検査を外す/緩める/バイパス経路を作らない」）。
//!
//! `core.rs::EngineCore::execute_sql`（TASK-75 で追加する固有メソッド。`VectorCore`
//! trait は不変）からのみ呼ばれる想定で、`Storage`・`SearchProvider`・`PolicyContext`
//! を束ねる。

use std::collections::{HashMap, HashSet};

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::{CatalogError, ColumnType, TableSchema};
use crate::core;
use crate::hybrid::{self, HybridError, HybridHit, RrfConfig};
use crate::kernel::{KernelError, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::row_codec::{self, RowCodecError, Value};
use crate::sparse::{DocId, SparseError, SparseIndex};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::parser::{BoundStatement, ProjectedColumn, Ranking};
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

/// 投影結果 1 セル。`row_codec::Value` の公開 enum は変更せず、`id` 疑似列
/// （`u64`）を表現するため独自の enum を持つ。
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Integer(u64),
    Text(String),
    Vector(Vec<f32>),
}

/// 投影結果の列メタデータ。`Id` は疑似列（`ColumnType` を持たない）。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnMeta {
    Id,
    Scalar { name: String, ty: ColumnType },
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

/// `EngineCore::execute_insert_sql` の成功応答（SQL-10、TASK-80）。単一行 INSERT の
/// みを受理するため常に `1` になるが、`INSERT 0 1` 相当の wire 応答（TASK-73）へ
/// 写像しやすいよう件数フィールドとして保持する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertOutcome {
    pub rows_affected: u64,
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
        ArenaError::Storage(_)
        | ArenaError::Catalog(_)
        | ArenaError::InvalidDim
        | ArenaError::DimMismatch { .. }
        | ArenaError::AllocationFailed(_) => SqlSurfaceError::Internal {
            detail: "arena build failed".to_string(),
        },
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
) -> Result<QueryResult, SqlSurfaceError> {
    // RLS 段（無条件）+ SCALAR 段（同一走査の行フック）。可視行だけがフックへ到達する
    // （`arena::VectorArena::build_filtered_with_rows` のドキュメント参照）。
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
    let mut candidate_columns: HashMap<u64, Vec<Value>> = HashMap::new();

    // 投影段（下記ループ）が実際に参照する Text 列インデックスの集合。`VECTOR` 列は
    // `scan_scalar_columns` が常に `None` を返すだけ（実体は `arena` から引く）ため
    // 対象外。この集合に含まれない列は `on_visible_row` で借用のまま素通りし、
    // `String` として複製・保持しない（[`MAX_CANDIDATE_SCALAR_BYTES`] のドキュメント
    // 参照。投影に不要な列まで全候補分複製・保持する P1 指摘への対応）。
    let needed_column_indices: HashSet<usize> = bound
        .projection
        .iter()
        .filter_map(|col| match col {
            ProjectedColumn::Column { index, .. } => Some(*index),
            ProjectedColumn::Id => None,
        })
        .collect();

    // `candidate_columns` に保持する Text 値の実バイト数に加え、行ごとに必ず確保する
    // `Vec<Value>` 自体の構造体サイズも累計する（[`MAX_CANDIDATE_SCALAR_BYTES`] の
    // ドキュメント参照。投影列を持たない `SELECT id ...` でも候補行数に比例して
    // 積み上がる構造体アロケーションを見逃さないため）。
    let mut candidate_scalar_bytes: usize = 0;
    let row_struct_bytes = schema
        .columns
        .len()
        .saturating_mul(std::mem::size_of::<Value>());

    let on_visible_row = |id: u64, metadata: &[u8]| -> std::result::Result<bool, ArenaError> {
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
        for filter in &bound.scalar_filters {
            match scanned.get(filter.column_index) {
                Some(Some(t)) if *t == filter.value => {}
                _ => return Ok(false),
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
                    sparse_docs.push((id, owned));
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
        candidate_columns.insert(id, kept);
        Ok(true)
    };

    let arena = VectorArena::build_filtered_with_rows_in_txn(
        read_txn,
        &bound.table,
        |tenant, visibility| ctx.is_visible(tenant, visibility),
        on_visible_row,
    )
    .map_err(|e| map_arena_error(&bound.table, e))?;

    let visible_id_set: HashSet<u64> = arena.ids().iter().copied().collect();
    // `id -> arena 内インデックス` の対応表。投影段で embedding を
    // `arena.vector(index)`（候補構築と同一スナップショットのバッファ）から
    // 引くために使う（`storage` への再アクセスを行わない）。
    let arena_index_by_id: HashMap<u64, usize> = arena
        .ids()
        .iter()
        .enumerate()
        .map(|(idx, id)| (*id, idx))
        .collect();

    // DISTANCE 段。
    let hits: Vec<(u64, f64)> = match &bound.ranking {
        Ranking::Distance { query } => {
            let input = SearchInput {
                ids: arena.ids(),
                vectors: arena.vectors(),
                dim: arena.dim(),
                query,
                k: bound.limit,
            };
            let raw = provider.search(input).map_err(map_kernel_error)?;
            if !core::provider_result_is_valid(&raw, bound.limit, &visible_id_set) {
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
            let pool_depth = bound.limit.max(DEFAULT_HYBRID_POOL_DEPTH);
            let cfg = RrfConfig::new(60.0, 1.0, 1.0, pool_depth).map_err(|_| {
                SqlSurfaceError::Internal {
                    detail: "invalid hybrid RRF config".to_string(),
                }
            })?;
            let input = SearchInput {
                ids: arena.ids(),
                vectors: arena.vectors(),
                dim: arena.dim(),
                query,
                k: bound.limit,
            };
            let fused: Vec<HybridHit> = if sparse_docs.is_empty() {
                // 疎文書が 0 件（可視行に本文を持つ行が 1 件もない）の場合は
                // `SparseIndex::build` が空コーパスを拒否する（`SparseError::EmptyCorpus`）
                // ため、密側のみを `rrf_fuse` に通して密のみの順序契約へ縮退させる
                // （疎側の寄与を単に 0 件として扱う。密側の可視性検証は
                // `hybrid::hybrid_search` と同じ理由でここでも行う）。
                let dense_input = SearchInput {
                    ids: arena.ids(),
                    vectors: arena.vectors(),
                    dim: arena.dim(),
                    query,
                    k: cfg.pool_depth(),
                };
                let dense_hits = provider.search(dense_input).map_err(map_kernel_error)?;
                if dense_hits.iter().any(|h| !visible_id_set.contains(&h.id)) {
                    return Err(SqlSurfaceError::Internal {
                        detail: "search provider returned a hit outside the visible id set"
                            .to_string(),
                    });
                }
                let mut fused =
                    hybrid::rrf_fuse(&dense_hits, &[], &cfg).map_err(map_hybrid_error)?;
                fused.truncate(bound.limit);
                fused
            } else {
                let doc_refs: Vec<(DocId, &str)> = sparse_docs
                    .iter()
                    .map(|(id, text)| (*id, text.as_str()))
                    .collect();
                let sparse_index = SparseIndex::build(&doc_refs)
                    .map_err(HybridError::Sparse)
                    .map_err(map_hybrid_error)?;
                hybrid::hybrid_search(
                    provider,
                    input,
                    &sparse_index,
                    query_text,
                    bound.limit,
                    &cfg,
                )
                .map_err(map_hybrid_error)?
            };
            fused.into_iter().map(|h| (h.id, h.score)).collect()
        }
    };

    // 投影: `storage` への再取得は行わず、候補選択（`build_filtered_with_rows`）と
    // 同一スナップショットで保持しておいた embedding（`arena`）・デコード済み
    // スカラー列（`candidate_columns`）から返却行を構築する（上記コメント参照。
    // 候補選択後に対象行が更新・削除されても、投影は候補選択時点の値を返す
    // ため、RLS・スカラー WHERE・embedding が候補選択と投影とで食い違うことはない）。
    let mut rows = Vec::with_capacity(hits.len());
    for (id, score) in hits {
        let index = *arena_index_by_id
            .get(&id)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "search hit id missing from candidate arena".to_string(),
            })?;
        let embedding = arena
            .vector(index)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "candidate arena index out of range".to_string(),
            })?;
        let decoded = candidate_columns
            .get(&id)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "search hit id missing from candidate scalar columns".to_string(),
            })?;
        let mut cells = Vec::with_capacity(bound.projection.len());
        for col in &bound.projection {
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
            }
        }
        rows.push(ResultRow { id, score, cells });
    }

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
        })
        .collect();

    Ok(QueryResult { columns, rows })
}

/// [`crate::sql::parser::BoundInsert`] を実行する（SQL-10、TASK-80 の公開 API）。
/// `core.rs::EngineCore::execute_insert_sql` からのみ呼ばれる想定で、`Storage`・
/// `PolicyContext` を束ねる（`execute_statement` と対称の役割）。
///
/// テナントは `ctx.tenant_id()` からサーバー側で導出し（クライアントが列リストへ
/// テナント相当の値を指定しても無視される。`.claude/rules/security.md` P0
/// 「クライアント指定のテナントを信用しない」）、可視性は常に `Visibility::Private`
/// に固定する（`PolicyContext::is_visible` は `Public` 行を他テナントへも可視と
/// するため、既定 `Public` は越境露出になる。fail-closed に `Private` を採用する）。
///
/// 単一の write トランザクション（[`crate::catalog::Storage::insert_typed_row_checked`]）
/// で完結し、既存 `id` への黙った上書きは行わない（拒否は `22000`）。
pub fn execute_insert(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundInsert,
) -> Result<InsertOutcome, SqlSurfaceError> {
    use crate::catalog::CatalogError;
    use crate::storage::Visibility;

    storage
        .insert_typed_row_checked(
            &bound.table,
            bound.id,
            ctx.tenant_id(),
            Visibility::Private,
            &bound.values,
            &bound.operation_id,
        )
        .map_err(|e| match e {
            CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
            // 既存 id 衝突・スキーマ不整合等はいずれも「受理構文だが値が不正」
            // として `22000` へ丸め込む。行内容・所有テナントは含めない
            // （security.md「エラー・ログ経由で他テナントのデータ・存在情報を
            // 漏らさない」）。
            CatalogError::Invalid(_) => {
                SqlSurfaceError::invalid_input("insert rejected: invalid row or duplicate id")
            }
            _ => SqlSurfaceError::Internal {
                detail: "insert failed".to_string(),
            },
        })?;

    Ok(InsertOutcome { rows_affected: 1 })
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
