//! `benches/harness/scan_stage_profile.rs`（Issue #464。`agg_count`／
//! `rls_isolation`／`vector_knn_where` の段別内訳プロファイル）の回帰テスト。
//!
//! `scan_stage_profile_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/knn_profile_accept.rs`・`tests/knn_wire_profile_accept.rs` と同様、
//! 実測タイマー・env に依存しない時間非依存の契約のみを `#[path]` で取り込み
//! `cargo test`〔`make ci` 対象〕で検証する）。
//!
//! 中心は [`decode_dim_and_metadata_reimpl`]（A4 段の再実装。embedding の f32
//! 変換を行わず dim・metadata borrowed slice のみを返す）が `engine::storage::
//! Storage`（pub API・正本）と一致することの検証（ドリフト検出）。加えて
//! rounds／scale のパース、段間差分・ノイズ帯判定の数値例、整合性検証関数
//! （行数一致）を固定する。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::scan_stage_profile::{
    assert_scan_row_counts_match, build_lang_filter, classify_against_bands,
    decode_dim_and_metadata_reimpl, matches_lang_filter, median_of, min_of, parse_rounds,
    parse_scale, reference_band, refuse_under_github_actions, scan_scalar_columns,
    stage_diff_ns_per_row, step_ratio_pct, verify_row_key_tenant_reimpl, BandClass, ScanStageError,
    DEFAULT_ROUNDS, MAX_ROUNDS, MAX_SCALE, MIN_ROUNDS,
};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::row_codec::{encode_scalar_columns, Value};
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
    assert_eq!(err, ScanStageError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_unset() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- parse_rounds ------------------------------------------------------------

#[test]
fn parse_rounds_defaults_when_unset() {
    assert_eq!(parse_rounds(None).unwrap(), DEFAULT_ROUNDS);
}

#[test]
fn parse_rounds_accepts_within_bounds() {
    assert_eq!(parse_rounds(Some("10")).unwrap(), 10);
    assert_eq!(
        parse_rounds(Some(&MIN_ROUNDS.to_string())).unwrap(),
        MIN_ROUNDS
    );
    assert_eq!(
        parse_rounds(Some(&MAX_ROUNDS.to_string())).unwrap(),
        MAX_ROUNDS
    );
}

#[test]
fn parse_rounds_rejects_below_minimum() {
    assert!(matches!(
        parse_rounds(Some("1")),
        Err(ScanStageError::InvalidRounds(_))
    ));
}

#[test]
fn parse_rounds_rejects_above_maximum() {
    assert!(matches!(
        parse_rounds(Some("51")),
        Err(ScanStageError::InvalidRounds(_))
    ));
}

#[test]
fn parse_rounds_rejects_non_numeric() {
    assert!(matches!(
        parse_rounds(Some("abc")),
        Err(ScanStageError::InvalidRounds(_))
    ));
}

// --- parse_scale ---------------------------------------------------------------

#[test]
fn parse_scale_defaults_to_one_when_unset() {
    assert_eq!(parse_scale(None).unwrap(), 1);
}

#[test]
fn parse_scale_accepts_within_bounds() {
    assert_eq!(parse_scale(Some("4")).unwrap(), MAX_SCALE);
}

#[test]
fn parse_scale_rejects_above_maximum() {
    assert!(matches!(
        parse_scale(Some("5")),
        Err(ScanStageError::InvalidScale(_))
    ));
}

#[test]
fn parse_scale_rejects_zero() {
    assert!(matches!(
        parse_scale(Some("0")),
        Err(ScanStageError::InvalidScale(_))
    ));
}

// --- min_of / median_of / reference_band / step_ratio_pct / classify_against_bands

#[test]
fn min_of_and_median_of_basic() {
    let samples = vec![
        Duration::from_millis(10),
        Duration::from_millis(30),
        Duration::from_millis(20),
    ];
    assert_eq!(min_of(&samples).unwrap(), Duration::from_millis(10));
    assert_eq!(median_of(&samples).unwrap(), Duration::from_millis(20));
}

#[test]
fn median_of_even_length_averages_middle_two() {
    let samples = vec![
        Duration::from_millis(10),
        Duration::from_millis(20),
        Duration::from_millis(30),
        Duration::from_millis(40),
    ];
    assert_eq!(median_of(&samples).unwrap(), Duration::from_millis(25));
}

#[test]
fn min_of_median_of_reject_empty() {
    let samples: Vec<Duration> = Vec::new();
    assert_eq!(min_of(&samples).unwrap_err(), ScanStageError::EmptySamples);
    assert_eq!(
        median_of(&samples).unwrap_err(),
        ScanStageError::EmptySamples
    );
}

#[test]
fn reference_band_computes_spread_over_min() {
    let medians = vec![
        Duration::from_millis(100),
        Duration::from_millis(110),
        Duration::from_millis(105),
    ];
    let band = reference_band(&medians).unwrap();
    assert!((band - 0.10).abs() < 1e-9);
}

#[test]
fn step_ratio_pct_rejects_zero_from() {
    assert!(matches!(
        step_ratio_pct(Duration::ZERO, Duration::from_millis(1)),
        Err(ScanStageError::DegenerateRatio(_))
    ));
}

#[test]
fn classify_against_bands_within_fixed_band() {
    assert_eq!(classify_against_bands(4.0, 0.0), BandClass::WithinNoiseBand);
    assert_eq!(classify_against_bands(6.0, 0.0), BandClass::AboveNoiseBand);
    // 実測帯が固定帯より広ければそちらを使う。
    assert_eq!(
        classify_against_bands(8.0, 10.0),
        BandClass::WithinNoiseBand
    );
}

// --- stage_diff_ns_per_row: 非単調は Err -------------------------------------

#[test]
fn stage_diff_ns_per_row_rejects_non_monotonic() {
    let err = stage_diff_ns_per_row(
        Duration::from_millis(10),
        Duration::from_millis(5),
        100,
        "A1",
        "A2",
    )
    .unwrap_err();
    assert!(matches!(err, ScanStageError::ConsistencyViolation(_)));
}

#[test]
fn stage_diff_ns_per_row_computes_ns_per_row() {
    let diff = stage_diff_ns_per_row(
        Duration::from_millis(0),
        Duration::from_millis(1),
        1_000_000,
        "A1",
        "A2",
    )
    .unwrap();
    assert!((diff - 1.0).abs() < 1e-6);
}

// --- assert_scan_row_counts_match --------------------------------------------

#[test]
fn assert_scan_row_counts_match_accepts_equal_counts() {
    assert!(assert_scan_row_counts_match(&[("A1", 10), ("A2", 10), ("A3", 10)]).is_ok());
}

#[test]
fn assert_scan_row_counts_match_rejects_mismatch() {
    let err = assert_scan_row_counts_match(&[("A1", 10), ("A2", 9)]).unwrap_err();
    assert!(matches!(err, ScanStageError::ConsistencyViolation(_)));
}

// --- verify_row_key_tenant_reimpl --------------------------------------------

#[test]
fn verify_row_key_tenant_reimpl_accepts_matching_tenant() {
    assert!(verify_row_key_tenant_reimpl("tenant-a", "tenant-a").is_ok());
}

#[test]
fn verify_row_key_tenant_reimpl_rejects_mismatch() {
    assert!(verify_row_key_tenant_reimpl("tenant-a", "tenant-b").is_err());
}

// --- W1/W2: scan_scalar_columns / build_lang_filter / matches_lang_filter ---

fn scalar_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("topic", ColumnType::Text, false),
        ],
    )
}

#[test]
fn scan_scalar_columns_and_lang_filter_roundtrip() {
    let schema = scalar_schema();
    let metadata = encode_scalar_columns(
        &schema,
        &[
            Value::Vector(vec![1.0, 2.0, 3.0, 4.0]),
            Value::Text("ja".to_string()),
            Value::Text("topic-00".to_string()),
        ],
    )
    .expect("encode_scalar_columns");

    let scanned = scan_scalar_columns(&schema, &metadata).expect("scan_scalar_columns");
    assert_eq!(scanned.len(), schema.columns.len());
    assert_eq!(scanned[1], Some("ja"));
    assert_eq!(scanned[2], Some("topic-00"));

    let filters = vec![build_lang_filter(&schema, "lang", "ja").expect("build_lang_filter")];
    assert!(matches_lang_filter(&filters, &scanned));

    let non_matching_filters =
        vec![build_lang_filter(&schema, "lang", "en").expect("build_lang_filter")];
    assert!(!matches_lang_filter(&non_matching_filters, &scanned));
}

// --- decode_dim_and_metadata_reimpl: ドリフト検出 ---------------------------

fn seed_storage(path: &std::path::Path) -> Vec<(String, Visibility, Vec<f32>, Vec<u8>)> {
    let storage = Storage::open(path).expect("open storage");
    let fixtures: Vec<(String, Visibility, Vec<f32>, Vec<u8>)> = vec![
        (
            "tenant-a".to_string(),
            Visibility::Public,
            vec![1.0, 2.0, 3.0, 4.0],
            b"meta-a".to_vec(),
        ),
        (
            "tenant-a".to_string(),
            Visibility::Private,
            vec![-1.5, 0.0, 2.25],
            Vec::new(),
        ),
        (
            "tenant-b".to_string(),
            Visibility::Public,
            vec![0.0; 8],
            b"meta-b-longer".to_vec(),
        ),
    ];
    for (i, (tenant, visibility, embedding, metadata)) in fixtures.iter().enumerate() {
        storage
            .put(
                i as u64,
                &RowInput {
                    tenant_id: tenant,
                    visibility: *visibility,
                    embedding,
                    metadata,
                },
            )
            .expect("seed row");
    }
    fixtures
}

#[test]
fn decode_dim_and_metadata_reimpl_matches_storage_scan_for_every_row() {
    let path = unique_db_path("issue464-scan-stage-profile-accept-decode");
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

    let mut actual_count = 0usize;
    for entry in table.iter().expect("iter rows table") {
        let (_k, v) = entry.expect("iterate row entry");
        let (dim, metadata) = decode_dim_and_metadata_reimpl(v.value()).expect("reimpl decode");
        let expected_row = expected
            .iter()
            .find(|row| row.embedding.len() == dim as usize && row.metadata == metadata)
            .unwrap_or_else(|| {
                panic!(
                    "reimpl decoded row not found in Storage::scan() result \
                     (layout drift suspected): dim={dim} metadata={metadata:?}"
                )
            });
        assert_eq!(expected_row.embedding.len(), dim as usize);
        actual_count += 1;
    }
    assert_eq!(actual_count, fixtures.len());
}

#[test]
fn decode_dim_and_metadata_reimpl_rejects_truncated_buffer() {
    let path = unique_db_path("issue464-scan-stage-profile-accept-truncated");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .put(
            0,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0, 3.0],
                metadata: b"abc",
            },
        )
        .expect("seed row");
    drop(storage);

    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");
    let (_k, v) = table
        .iter()
        .expect("iter rows table")
        .next()
        .expect("at least one row")
        .expect("iterate row entry");
    let full = v.value();
    // 末尾を切り詰めて破損させる。
    let truncated = &full[..full.len().saturating_sub(2)];
    let err = decode_dim_and_metadata_reimpl(truncated).unwrap_err();
    assert!(matches!(err, ScanStageError::Codec(_)));
}
