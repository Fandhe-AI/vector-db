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

use std::collections::HashSet;

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
use crate::storage::{Storage, StorageError};

/// 疎コーパス側へ集約する候補プールの既定深さ。`hybrid::hybrid_search` の
/// `cfg.pool_depth()` に渡す（[`hybrid::RrfConfig::new`] の検証を通過する範囲で、
/// `bound.limit`（`1..=core::MAX_SEARCH_K`）を必ず満たせるよう `bound.limit` との
/// 最大値を取る）。
const DEFAULT_HYBRID_POOL_DEPTH: usize = 200;

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

/// [`BoundStatement`] を実行する（TASK-75 の公開 API）。`schema` は呼び出し元
/// （`core.rs::EngineCore::execute_sql`）が `sql::parser::bind` へ渡したのと同一の
/// スキーマを渡す。
pub fn execute_statement(
    storage: &Storage,
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
    // （`MAX_CORPUS_DOCS`・`MAX_CORPUS_BYTES`）はアロケーション（`String::clone`）の
    // *後* にしか検証できないため、本文を `sparse_docs` へ push する前にここで
    // 同じ上限を検証する（.claude/rules/coding-rust.md「長さフィールドは上限検証して
    // からアロケーションに使う」・security.md「不安全な設計｜無制限リソース確保
    // （DoS）」対応。可視行数の上限は `arena::MAX_ARENA_ROWS`（最大 100 万件）だが、
    // 疎コーパス側の上限はそれよりずっと小さいため、蓄積前の検証がないと候補集合
    // 構築の途中で無制限に近いメモリを確保してから拒否することになる）。
    let mut sparse_bytes: usize = 0;

    let on_visible_row = |id: u64, metadata: &[u8]| -> std::result::Result<bool, ArenaError> {
        let decoded = row_codec::decode_scalar_columns(schema, metadata)
            .map_err(|e| ArenaError::Storage(StorageError::Codec(e.to_string())))?;
        for filter in &bound.scalar_filters {
            match decoded.get(filter.column_index) {
                Some(Value::Text(t)) if *t == filter.value => {}
                _ => return Ok(false),
            }
        }
        if is_hybrid {
            if let Some(idx) = text_column_index {
                if let Some(Value::Text(t)) = decoded.get(idx) {
                    if sparse_docs.len() >= crate::sparse::MAX_CORPUS_DOCS {
                        return Err(ArenaError::CapacityExceeded);
                    }
                    let next_bytes = sparse_bytes.saturating_add(t.len());
                    if next_bytes > crate::sparse::MAX_CORPUS_BYTES {
                        return Err(ArenaError::CapacityExceeded);
                    }
                    sparse_bytes = next_bytes;
                    sparse_docs.push((id, t.clone()));
                }
                // NULL 本文は疎側に含めない（密のみへ寄与する）。
            }
        }
        Ok(true)
    };

    let arena = VectorArena::build_filtered_with_rows(
        storage,
        &bound.table,
        |tenant, visibility| ctx.is_visible(tenant, visibility),
        on_visible_row,
    )
    .map_err(|e| map_arena_error(&bound.table, e))?;

    let visible_id_set: HashSet<u64> = arena.ids().iter().copied().collect();

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

    // 投影: ヒット id ごとに再取得し、二重防御として再度可視性を確認してから
    // スカラーペイロードをデコードして返却行を構築する（並行書き込みに対する
    // 二重防御。`sql::exec` モジュールドキュメント参照）。
    let mut rows = Vec::with_capacity(hits.len());
    for (id, score) in hits {
        let row = storage.get_row_from_table(&bound.table, id).map_err(|_| {
            SqlSurfaceError::Internal {
                detail: "row disappeared between candidate selection and projection".to_string(),
            }
        })?;
        if !ctx.is_visible(&row.tenant_id, row.visibility) {
            return Err(SqlSurfaceError::Internal {
                detail: "row became invisible between candidate selection and projection"
                    .to_string(),
            });
        }
        let decoded = row_codec::decode_scalar_columns(schema, &row.metadata)?;
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
                        ColumnType::Vector(_) => cells.push(Cell::Vector(row.embedding.clone())),
                        ColumnType::Text => match decoded.get(*index) {
                            Some(Value::Text(t)) => cells.push(Cell::Text(t.clone())),
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
