//! `benches/harness/knn_profile.rs`（Issue #362。KNN 経路の段別内訳プロファイル。
//! 走査・ヘッダデコード・f32 デコード・arena 構築・距離計算の切り分け）の回帰テスト。
//!
//! `knn_profile_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hybrid_latency_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。
//!
//! 本テストの中心は [`decode_header_reimpl`]・[`decode_row_reimpl`]（ベンチ内の
//! 行フォーマット v2 再実装）が `engine::storage::Storage`（pub API・正本）と
//! 一致することの検証（ドリフト検出）。`Storage::put`（`RowInput` 経由）で
//! 書き込んだ行を、生 `redb::Database` で再オープンして本モジュールの再実装で
//! デコードし、`Storage::scan()` の結果と突き合わせる。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::knn_profile::{
    assert_scan_row_counts_match, decode_header_reimpl, decode_row_reimpl, ns_per_row,
    refuse_under_github_actions, stage_diff_ns_per_row, KnnProfileError,
};

use engine::storage::{RowInput, Storage, Visibility};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use std::time::Duration;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ROWS_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("rows");

// --- refuse_under_github_actions --------------------------------------------

#[test]
fn refuse_under_github_actions_rejects_when_set() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, KnnProfileError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_unset() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- decode_header_reimpl / decode_row_reimpl: ドリフト検出 -----------------

fn seed_storage(path: &std::path::Path) -> Vec<(String, Visibility, Vec<f32>)> {
    let storage = Storage::open(path).expect("open storage");
    let fixtures: Vec<(String, Visibility, Vec<f32>)> = vec![
        (
            "tenant-a".to_string(),
            Visibility::Public,
            vec![1.0, 2.0, 3.0, 4.0],
        ),
        (
            "tenant-a".to_string(),
            Visibility::Private,
            vec![-1.5, 0.0, 2.25],
        ),
        ("tenant-b".to_string(), Visibility::Public, vec![0.0; 8]),
    ];
    for (i, (tenant, visibility, embedding)) in fixtures.iter().enumerate() {
        storage
            .put(
                i as u64,
                &RowInput {
                    tenant_id: tenant,
                    visibility: *visibility,
                    embedding,
                    metadata: b"",
                },
            )
            .expect("seed row");
    }
    fixtures
}

#[test]
fn decode_row_reimpl_matches_storage_scan_for_every_row() {
    let path = unique_db_path("issue362-knn-profile-accept-decode");
    let _guard = CleanupGuard(path.clone());
    let fixtures = seed_storage(&path);

    // 正本: `Storage::scan()`（pub API・完全デコード）。
    let storage = Storage::open(&path).expect("reopen storage for scan");
    let expected = storage.scan().expect("scan must succeed");
    assert_eq!(expected.len(), fixtures.len());
    drop(storage);

    // 再実装: 生 `redb::Database` で再オープンし、本モジュールのデコードを通す。
    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");

    let mut scratch: Vec<f32> = Vec::new();
    let mut actual_count = 0usize;
    for entry in table.iter().expect("iter rows table") {
        let (_k, v) = entry.expect("iterate row entry");
        let decoded = decode_row_reimpl(v.value(), &mut scratch).expect("reimpl decode");
        let expected_row = expected
            .iter()
            .find(|row| {
                row.tenant_id == decoded.tenant_id
                    && (row.visibility == Visibility::Public) == decoded.is_public
                    && row.embedding == decoded.embedding
            })
            .unwrap_or_else(|| {
                panic!(
                    "reimpl decoded row not found in Storage::scan() result \
                     (layout drift suspected): tenant_id={} is_public={} embedding={:?}",
                    decoded.tenant_id, decoded.is_public, decoded.embedding
                )
            });
        assert_eq!(expected_row.metadata.len(), decoded.metadata_len);
        actual_count += 1;
    }
    assert_eq!(actual_count, fixtures.len());
}

#[test]
fn decode_header_reimpl_matches_tenant_and_visibility() {
    let path = unique_db_path("issue362-knn-profile-accept-header");
    let _guard = CleanupGuard(path.clone());
    let _fixtures = seed_storage(&path);

    let storage = Storage::open(&path).expect("reopen storage for scan");
    let expected = storage.scan().expect("scan must succeed");
    drop(storage);

    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");

    let mut matched = 0usize;
    for entry in table.iter().expect("iter rows table") {
        let (_k, v) = entry.expect("iterate row entry");
        let (tenant_id, is_public, _offset) =
            decode_header_reimpl(v.value()).expect("header decode");
        assert!(expected
            .iter()
            .any(|row| row.tenant_id == tenant_id
                && (row.visibility == Visibility::Public) == is_public));
        matched += 1;
    }
    assert_eq!(matched, expected.len());
}

#[test]
fn decode_row_reimpl_rejects_truncated_buffer() {
    // ヘッダより短いバイト列は `Err` になる（`storage.rs::decode_row_header` と
    // 同じ fail-closed 契約をベンチ内再実装でも維持していることの確認）。
    let mut scratch: Vec<f32> = Vec::new();
    let truncated = [2u8, 0u8]; // version=2, tenant_len の途中で切れている
    let err = decode_row_reimpl(&truncated, &mut scratch).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn decode_row_reimpl_rejects_unsupported_version() {
    let mut scratch: Vec<f32> = Vec::new();
    let buf = [9u8, 0u8, 0u8]; // version=9 は未対応
    let err = decode_row_reimpl(&buf, &mut scratch).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

// --- ns_per_row / stage_diff_ns_per_row --------------------------------------

#[test]
fn ns_per_row_computes_expected_rate() {
    let total = Duration::from_millis(10);
    let rate = ns_per_row(total, 1_000).expect("non-zero rows");
    assert!((rate - 10_000.0).abs() < 1.0, "rate={rate}");
}

#[test]
fn ns_per_row_rejects_zero_rows() {
    let err = ns_per_row(Duration::from_millis(1), 0).unwrap_err();
    assert_eq!(err, KnnProfileError::ZeroRows);
}

#[test]
fn stage_diff_ns_per_row_computes_positive_diff() {
    let earlier = Duration::from_millis(5);
    let later = Duration::from_millis(8);
    let diff = stage_diff_ns_per_row(earlier, later, 1_000, "S1", "S2").expect("later >= earlier");
    assert!((diff - 3_000.0).abs() < 1.0, "diff={diff}");
}

#[test]
fn stage_diff_ns_per_row_rejects_non_monotonic_stages() {
    let earlier = Duration::from_millis(8);
    let later = Duration::from_millis(5);
    let err = stage_diff_ns_per_row(earlier, later, 1_000, "S1", "S2").unwrap_err();
    assert_eq!(
        err,
        KnnProfileError::NonMonotonicStages {
            earlier: "S1",
            later: "S2"
        }
    );
}

// --- assert_scan_row_counts_match --------------------------------------------

#[test]
fn assert_scan_row_counts_match_accepts_equal_counts() {
    assert!(assert_scan_row_counts_match(&[("S1", 100), ("S2", 100), ("S3", 100)]).is_ok());
}

#[test]
fn assert_scan_row_counts_match_rejects_mismatched_counts() {
    let err = assert_scan_row_counts_match(&[("S1", 100), ("S2", 99)]).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn assert_scan_row_counts_match_accepts_empty_input() {
    assert!(assert_scan_row_counts_match(&[]).is_ok());
}
