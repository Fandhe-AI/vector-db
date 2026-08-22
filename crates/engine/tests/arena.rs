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

// 対象ビヘイビア: TABLE-8。複数行を投入して build した結果が、行数・次元・各行の
// 内容とも Storage::get の読み直し結果と一致し、連続バッファの長さが len * dim と
// 一致すること（コールドスタート・アリーナの基本契約）を検証する。
#[test]
fn build_produces_contiguous_arena_matching_storage_rows() {
    let path = unique_db_path("basic");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let dim: usize = 8;
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

    let arena = VectorArena::build(&storage, dim as u32).expect("build arena");
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

// 対象ビヘイビア: TABLE-8。1 行も書き込んでいない DB（ROWS_TABLE 未作成）は
// 空アリーナとして成功すること。
#[test]
fn build_on_empty_database_returns_empty_arena() {
    let path = unique_db_path("empty");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let arena = VectorArena::build(&storage, 16).expect("build arena on empty db");
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

    let err = VectorArena::build(&storage, 4).expect_err("dim mismatch must be rejected");
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

// 対象ビヘイビア: TABLE-8。expected_dim の事前検証（0 または上限超過）を確認する。
#[test]
fn build_rejects_invalid_expected_dim() {
    let path = unique_db_path("invalid-dim");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    assert!(matches!(
        VectorArena::build(&storage, 0),
        Err(ArenaError::InvalidDim)
    ));
    // `crate::storage::MAX_EMBEDDING_DIM` は `pub(crate)` でテストから参照できないため、
    // 現行値（65_536。`storage.rs` 参照）より確実に大きい値を直接指定する。
    assert!(matches!(
        VectorArena::build(&storage, 200_000),
        Err(ArenaError::InvalidDim)
    ));
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

    let arena_before = VectorArena::build(&storage, 2).expect("build before extra write");
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

    let arena_after = VectorArena::build(&storage, 2).expect("rebuild after extra write");
    assert_eq!(arena_after.len(), 2);
}
