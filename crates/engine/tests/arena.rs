//! `engine::arena::VectorArena` の機能・fail-closed 統合テスト（TASK-87、
//! 対象ビヘイビア: TABLE-8。ポインタ: `docs/spec/05-tasks.md` TASK-87・
//! `docs/spec/04-behavior/data-model.md` TABLE-8）。
//!
//! ヘルパ（`unique_db_path` / `CleanupGuard` / 決定論的な埋め込み生成）は
//! `tests/incremental_write_perf.rs` の既存方針に倣い、このファイル内に複製する
//! （ヘルパ共通化はスコープ外）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::arena::{ArenaError, VectorArena};
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::storage::{RowInput, Storage, Visibility};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-task87-arena-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(PathBuf);

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
fn schema_for(table_name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
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
    storage.put_batch(&rows).expect("seed rows");

    let arena = VectorArena::build(&storage, "docs").expect("build arena");
    assert_eq!(arena.table_name(), "docs");
    assert_eq!(arena.dim(), dim as u32);
    assert_eq!(arena.len(), 10);
    assert!(!arena.is_empty());
    assert_eq!(arena.vectors().len(), 10 * dim);
    assert_eq!(arena.ids(), &(0u64..10).collect::<Vec<_>>()[..]);

    for i in 0..10usize {
        let expected_row = storage.get(i as u64).expect("read row back via storage");
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
// （ROWS_TABLE 未作成）は空アリーナとして成功すること。
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
        .put(
            0,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0, 3.0, 4.0],
                metadata: b"m",
            },
        )
        .expect("seed matching-dim row");
    storage
        .put(
            1,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0],
                metadata: b"m",
            },
        )
        .expect("seed mismatched-dim row");

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

// 対象ビヘイビア: TABLE-8（P1 レビュー指摘対応）。カタログに対象テーブル以外の
// ユーザーテーブルが存在する場合、たとえ同一次元であっても `build` は
// `Err(MultipleTablesPresent)` で拒否し、他テーブルの行を混入させないこと。
#[test]
fn build_rejects_when_another_table_coexists_even_with_same_dim() {
    let path = unique_db_path("multi-table");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&schema_for("docs_a", 4))
        .expect("create_table docs_a");
    storage
        .create_table(&schema_for("docs_b", 4))
        .expect("create_table docs_b");

    // 同じ ROWS_TABLE へテーブル帰属の区別なく書き込まれる（永続化層の現行制約。
    // モジュールドキュメント参照）。docs_b 側の行のみを書き込む。
    storage
        .put(
            0,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0, 3.0, 4.0],
                metadata: b"table=docs_b",
            },
        )
        .expect("seed docs_b row");

    let err = VectorArena::build(&storage, "docs_a").expect_err("must reject when docs_b coexists");
    match err {
        ArenaError::MultipleTablesPresent { requested, other } => {
            assert_eq!(requested, "docs_a");
            assert_eq!(other, "docs_b");
        }
        other => panic!("expected MultipleTablesPresent, got {other:?}"),
    }

    // docs_b を対象に build しても、カタログに docs_a が残る限り同じゲートで拒否される
    // （テーブル単位で安全に走査できるのは「カタログ上のユーザーテーブルが 1 つだけ」の
    // ときに限られる。モジュールドキュメント参照）。
    let err_b =
        VectorArena::build(&storage, "docs_b").expect_err("must reject when docs_a coexists");
    assert!(matches!(err_b, ArenaError::MultipleTablesPresent { .. }));
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
        .put(
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
        .put(
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
