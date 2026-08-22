//! TASK-146 拡張機能（EXT-1, EXT-2）の統合テスト（対象ビヘイビア: EXT-1, EXT-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-146・`docs/spec/04-behavior/extensions.md`）。
//!
//! `crates/engine/src/catalog.rs` へ追加したテーブルスコープ行 API
//! （`insert_row_into_table` / `insert_rows_into_table` / `get_row_from_table` /
//! `scan_table_page`）を検証する。`tests/multi_dim_tables.rs`（TASK-91）は
//! production コードに行とテーブルを関連付ける機構がまだ無かった時点で、
//! id レンジ分離によりテーブル帰属をテスト側で模擬していたが、本ファイルは
//! その機構そのもの（本タスクの成果物）を直接検証する。
//!
//! スコープ境界: 検索カーネル本体・類似度計算はデータ層の責務外（別タスク管轄）。
//! EXT-1 の「検索」に対応する範囲として、本ファイルではテーブルスコープ scan で
//! 読み出した embedding に対しテスト側でコサイン類似度の brute-force top-k を計算し、
//! データ層の挙動を確認するに留める。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use engine::storage::{RowInput, Storage, Visibility};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに一意な DB ファイルパスを払い出す（`multi_dim_tables.rs` と同じ方針）。
fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-task146-extensions-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

/// テスト終了時（panic 時含む）に DB ファイルを確実に削除するガード。
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

/// `dim` 次元・`seed` に基づく決定論的な埋め込みを生成する。
fn make_embedding(dim: u32, seed: u32) -> Vec<f32> {
    let mut rng = Xorshift32(seed | 1); // seed 0 は xorshift の不動点になるため奇数化する。
    (0..dim).map(|_| rng.next_f32()).collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

fn vector_table_schema(name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("body", ColumnType::Text, true),
        ],
    )
}

// --- EXT-1: 既定 768 次元での挿入・読み出し・検索動作 ------------------------------

#[test]
fn ext1_insert_and_read_back_768_dim_rows() {
    let path = unique_db_path("ext1-insert-read");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 768))
        .expect("create_table");

    let embeddings: Vec<Vec<f32>> = (0..20u32).map(|i| make_embedding(768, i + 1)).collect();
    for (i, embedding) in embeddings.iter().enumerate() {
        storage
            .insert_row_into_table(
                "docs",
                i as u64,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"m",
                },
            )
            .unwrap_or_else(|e| panic!("insert_row_into_table failed for id={i}: {e}"));
    }

    for (i, embedding) in embeddings.iter().enumerate() {
        let row = storage
            .get_row_from_table("docs", i as u64)
            .unwrap_or_else(|e| panic!("get_row_from_table failed for id={i}: {e}"));
        assert_eq!(row.id, i as u64);
        assert_eq!(row.embedding.len(), 768);
        assert_eq!(&row.embedding, embedding);
    }

    let (page, cursor) = storage
        .scan_table_page("docs", None, 100)
        .expect("scan_table_page");
    assert_eq!(page.len(), 20);
    assert_eq!(cursor, None);
    for row in &page {
        assert_eq!(row.embedding.len(), 768);
    }
}

#[test]
fn ext1_brute_force_top_k_ranks_self_match_first() {
    // 検索カーネル本体（類似度計算・ランキング）は別タスク管轄（TASK-124〜）。
    // ここではデータ層から読み出した embedding に対しテスト側で brute-force に
    // コサイン類似度を計算し、自己一致がランク 1 になることでデータ層の
    // 挿入・読み出し動作を確認する（EXT-1）。
    let path = unique_db_path("ext1-topk");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 768))
        .expect("create_table");

    let embeddings: Vec<Vec<f32>> = (0..10u32).map(|i| make_embedding(768, i + 1)).collect();
    let rows: Vec<(u64, RowInput<'_>)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, embedding)| {
            (
                i as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"m",
                },
            )
        })
        .collect();
    storage
        .insert_rows_into_table("docs", &rows)
        .expect("insert_rows_into_table");

    let (all_rows, _cursor) = storage
        .scan_table_page("docs", None, 100)
        .expect("scan_table_page");

    // クエリ = 挿入済み embedding のうち 1 件（自己一致が最上位になるべき）。
    let query_idx = 3usize;
    let query = &embeddings[query_idx];

    let mut ranked: Vec<(u64, f32)> = all_rows
        .iter()
        .map(|row| (row.id, cosine_similarity(query, &row.embedding)))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).expect("similarity is not NaN"));

    assert_eq!(
        ranked.first().map(|(id, _)| *id),
        Some(query_idx as u64),
        "self-match must rank first: {ranked:?}"
    );
}

// --- EXT-2: テーブル粒度次元固定・複数テーブル共存 --------------------------------

#[test]
fn ext2_multiple_tables_with_distinct_dims_coexist() {
    let path = unique_db_path("ext2-coexist");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let dims: [(&str, u32); 3] = [("small", 384), ("mid", 768), ("large", 1536)];
    for (name, dim) in dims {
        storage
            .create_table(&vector_table_schema(name, dim))
            .unwrap_or_else(|e| panic!("create_table({name}) failed: {e}"));
    }

    for (name, dim) in dims {
        let embedding = make_embedding(dim, 7);
        storage
            .insert_row_into_table(
                name,
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &embedding,
                    metadata: b"m",
                },
            )
            .unwrap_or_else(|e| panic!("insert into {name} (dim {dim}) failed: {e}"));
    }

    for (name, dim) in dims {
        let row = storage
            .get_row_from_table(name, 1)
            .unwrap_or_else(|e| panic!("get_row_from_table({name}) failed: {e}"));
        assert_eq!(row.embedding.len(), dim as usize);
    }
}

#[test]
fn ext2_rejects_dimension_mismatch_per_table_fail_closed() {
    let path = unique_db_path("ext2-dim-mismatch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("small", 384))
        .expect("create_table(small)");
    storage
        .create_table(&vector_table_schema("mid", 768))
        .expect("create_table(mid)");

    // 他テーブルの次元（768）を small（384）へ挿入しようとすると拒否される。
    let wrong_dim_embedding = make_embedding(768, 1);
    let err = storage
        .insert_row_into_table(
            "small",
            1,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &wrong_dim_embedding,
                metadata: b"m",
            },
        )
        .expect_err("mismatched dim must be rejected");
    assert!(matches!(err, CatalogError::Invalid(_)));

    // 次元 0 の埋め込みも拒否される。
    let err = storage
        .insert_row_into_table(
            "small",
            2,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &[],
                metadata: b"m",
            },
        )
        .expect_err("zero-length embedding must be rejected");
    assert!(matches!(err, CatalogError::Invalid(_)));

    // small テーブルには何も挿入されていないはず（拒否されたので）。
    let (rows, _cursor) = storage
        .scan_table_page("small", None, 100)
        .expect("scan_table_page(small)");
    assert!(rows.is_empty());
}

#[test]
fn ext2_same_id_coexists_independently_across_tables() {
    // TASK-91 の id レンジ模擬（`multi_dim_tables.rs`）では検証不能だった点。
    // テーブル帰属した独立ストア（本タスクの production 実装）を直接証明する。
    let path = unique_db_path("ext2-same-id");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("small", 384))
        .expect("create_table(small)");
    storage
        .create_table(&vector_table_schema("mid", 768))
        .expect("create_table(mid)");

    let small_embedding = make_embedding(384, 11);
    let mid_embedding = make_embedding(768, 22);

    storage
        .insert_row_into_table(
            "small",
            42,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &small_embedding,
                metadata: b"small-42",
            },
        )
        .expect("insert into small id=42");
    storage
        .insert_row_into_table(
            "mid",
            42,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &mid_embedding,
                metadata: b"mid-42",
            },
        )
        .expect("insert into mid id=42");

    let small_row = storage
        .get_row_from_table("small", 42)
        .expect("get small id=42");
    let mid_row = storage
        .get_row_from_table("mid", 42)
        .expect("get mid id=42");

    assert_eq!(small_row.embedding, small_embedding);
    assert_eq!(small_row.metadata, b"small-42");
    assert_eq!(mid_row.embedding, mid_embedding);
    assert_eq!(mid_row.metadata, b"mid-42");
}

#[test]
fn ext2_scan_table_page_returns_only_own_table_rows() {
    let path = unique_db_path("ext2-scan-isolation");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("small", 384))
        .expect("create_table(small)");
    storage
        .create_table(&vector_table_schema("mid", 768))
        .expect("create_table(mid)");

    // `RowInput` は埋め込みバッファを借用するため、先にすべての埋め込みを実体として
    // 保持してから `RowInput` を組み立てる（借用元の生存期間を `insert_rows_into_table`
    // 呼び出しまで確実に確保するため）。
    let small_embeddings: Vec<Vec<f32>> = (0..5u64)
        .map(|i| make_embedding(384, i as u32 + 1))
        .collect();
    let small_rows: Vec<(u64, RowInput<'_>)> = small_embeddings
        .iter()
        .enumerate()
        .map(|(i, embedding)| {
            (
                i as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"s",
                },
            )
        })
        .collect();
    storage
        .insert_rows_into_table("small", &small_rows)
        .expect("seed small");

    let mid_embeddings: Vec<Vec<f32>> = (100..103u64)
        .map(|i| make_embedding(768, i as u32 + 1))
        .collect();
    let mid_rows: Vec<(u64, RowInput<'_>)> = mid_embeddings
        .iter()
        .enumerate()
        .map(|(idx, embedding)| {
            (
                100u64 + idx as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"m",
                },
            )
        })
        .collect();
    storage
        .insert_rows_into_table("mid", &mid_rows)
        .expect("seed mid");

    let (small_page, _) = storage
        .scan_table_page("small", None, 100)
        .expect("scan small");
    assert_eq!(small_page.len(), 5);
    assert!(small_page.iter().all(|r| r.embedding.len() == 384));
    assert!(small_page.iter().all(|r| r.id < 100));

    let (mid_page, _) = storage.scan_table_page("mid", None, 100).expect("scan mid");
    assert_eq!(mid_page.len(), 3);
    assert!(mid_page.iter().all(|r| r.embedding.len() == 768));
    assert!(mid_page.iter().all(|r| r.id >= 100));
}

#[test]
fn ext2_state_survives_close_and_reopen() {
    let path = unique_db_path("ext2-persist");
    let _cleanup = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage (first)");
        storage
            .create_table(&vector_table_schema("small", 384))
            .expect("create_table(small)");
        storage
            .create_table(&vector_table_schema("mid", 768))
            .expect("create_table(mid)");

        let small_embedding = make_embedding(384, 3);
        let mid_embedding = make_embedding(768, 5);
        storage
            .insert_row_into_table(
                "small",
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &small_embedding,
                    metadata: b"s1",
                },
            )
            .expect("insert small id=1");
        storage
            .insert_row_into_table(
                "mid",
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &mid_embedding,
                    metadata: b"m1",
                },
            )
            .expect("insert mid id=1");
        // `storage` はここでスコープを抜けて drop される（close 相当）。
    }

    {
        let storage = Storage::open(&path).expect("open storage (second)");
        let small_row = storage
            .get_row_from_table("small", 1)
            .expect("get small id=1 after reopen");
        let mid_row = storage
            .get_row_from_table("mid", 1)
            .expect("get mid id=1 after reopen");
        assert_eq!(small_row.embedding.len(), 384);
        assert_eq!(mid_row.embedding.len(), 768);
        assert_eq!(small_row.metadata, b"s1");
        assert_eq!(mid_row.metadata, b"m1");

        // 誤次元挿入の拒否・同一 id の独立共存も再 open 後に不変であることを確認する。
        let wrong_dim = make_embedding(768, 9);
        let err = storage
            .insert_row_into_table(
                "small",
                2,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &wrong_dim,
                    metadata: b"s2",
                },
            )
            .expect_err("mismatched dim must still be rejected after reopen");
        assert!(matches!(err, CatalogError::Invalid(_)));
    }
}

#[test]
fn ext2_rejects_operations_on_nonexistent_or_vectorless_table() {
    let path = unique_db_path("ext2-notfound");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // 不存在テーブルへの挿入・取得・走査は TableNotFound。
    let embedding = make_embedding(8, 1);
    let err = storage
        .insert_row_into_table(
            "ghost",
            1,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &embedding,
                metadata: b"m",
            },
        )
        .expect_err("insert into nonexistent table must fail");
    assert!(matches!(err, CatalogError::TableNotFound(_)));

    let err = storage
        .get_row_from_table("ghost", 1)
        .expect_err("get from nonexistent table must fail");
    assert!(matches!(err, CatalogError::TableNotFound(_)));

    let err = storage
        .scan_table_page("ghost", None, 10)
        .expect_err("scan of nonexistent table must fail");
    assert!(matches!(err, CatalogError::TableNotFound(_)));

    // VECTOR 列を持たないテーブルへの embedding 付き挿入は拒否される。
    let text_only = TableSchema::new(
        "notes",
        vec![ColumnDef::new("body", ColumnType::Text, false)],
    );
    storage
        .create_table(&text_only)
        .expect("create_table(notes)");
    let err = storage
        .insert_row_into_table(
            "notes",
            1,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &embedding,
                metadata: b"m",
            },
        )
        .expect_err("insert with embedding into VECTOR-less table must fail");
    assert!(matches!(err, CatalogError::Invalid(_)));
}
