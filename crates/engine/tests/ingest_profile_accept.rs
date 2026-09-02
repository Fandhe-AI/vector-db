//! `benches/harness/ingest_profile.rs`（Issue #396。ingest 経路
//! 〔`engine::tenant::insert_rows` → `insert_rows_unchecked`〕の段別内訳
//! プロファイル。encode／content_hash／台帳値の再実装の回帰）の回帰テスト。
//!
//! `ingest_profile_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/knn_profile_accept.rs`・`tests/hybrid_profile_accept.rs` と同様、
//! 実測タイマー・env に依存しない時間非依存の契約のみを `#[path]` で取り込み
//! `cargo test`〔`make ci` 対象〕で検証する）。
//!
//! 本テストの中心は以下 2 点のドリフト検出:
//! - [`encode_row_reimpl`] が `engine::storage::Storage`（pub API・正本）で
//!   書き込んだ行のバイト列と一致すること。
//! - [`content_hash_insert_batch_reimpl`] が `tenant::insert_rows` が実際に
//!   `op_ledger` へ記録した内容ハッシュと一致すること。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::ingest_profile::{
    content_hash_insert_batch_reimpl, decode_ledger_entry_v2_reimpl, encode_row_reimpl,
    last_op_entry_reimpl, ledger_entry_v2_reimpl, ns_per_row, parse_bounded_env,
    refuse_under_github_actions, residual_ns_per_row, sha256_reimpl, sum_durations,
    IngestProfileError, StageId, StageSamples,
};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

use redb::{Database, ReadableDatabase, TableDefinition};

use std::time::Duration;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ROW_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("user_rows/docs");
const OP_LEDGER_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("op_ledger");
const LAST_OP_TABLE: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("last_op");

// --- refuse_under_github_actions --------------------------------------------

#[test]
fn refuse_under_github_actions_rejects_when_set() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, IngestProfileError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_unset() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- parse_bounded_env -------------------------------------------------------

#[test]
fn parse_bounded_env_uses_default_when_unset() {
    assert_eq!(
        parse_bounded_env("X", None, 1_000, 1, 10_000).unwrap(),
        1_000
    );
}

#[test]
fn parse_bounded_env_accepts_boundary_values() {
    assert_eq!(
        parse_bounded_env("X", Some("1"), 1_000, 1, 10_000).unwrap(),
        1
    );
    assert_eq!(
        parse_bounded_env("X", Some("10000"), 1_000, 1, 10_000).unwrap(),
        10_000
    );
}

#[test]
fn parse_bounded_env_rejects_empty_string() {
    let err = parse_bounded_env("X", Some(""), 1_000, 1, 10_000).unwrap_err();
    assert!(matches!(
        err,
        IngestProfileError::InvalidEnv { name: "X", .. }
    ));
}

#[test]
fn parse_bounded_env_rejects_non_numeric() {
    let err = parse_bounded_env("X", Some("abc"), 1_000, 1, 10_000).unwrap_err();
    assert!(matches!(
        err,
        IngestProfileError::InvalidEnv { name: "X", .. }
    ));
}

#[test]
fn parse_bounded_env_rejects_out_of_range() {
    assert!(parse_bounded_env("X", Some("0"), 1_000, 1, 10_000).is_err());
    assert!(parse_bounded_env("X", Some("10001"), 1_000, 1, 10_000).is_err());
}

// --- sha256_reimpl: FIPS 180-4 公開テストベクタ ------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn sha256_reimpl_matches_fips_test_vector_abc() {
    let digest = sha256_reimpl(b"abc");
    assert_eq!(
        hex(&digest),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sha256_reimpl_matches_known_digest_for_empty_input() {
    let digest = sha256_reimpl(b"");
    assert_eq!(
        hex(&digest),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_reimpl_matches_fips_test_vector_two_blocks() {
    let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    let digest = sha256_reimpl(input);
    assert_eq!(
        hex(&digest),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

// --- encode_row_reimpl: fail-closed --------------------------------------

#[test]
fn encode_row_reimpl_rejects_empty_tenant_id() {
    let err = encode_row_reimpl("", true, &[1.0, 2.0], b"meta").unwrap_err();
    assert!(matches!(err, IngestProfileError::Codec(_)));
}

#[test]
fn encode_row_reimpl_rejects_oversized_tenant_id() {
    let long_tenant = "a".repeat(257);
    let err = encode_row_reimpl(&long_tenant, true, &[1.0], b"meta").unwrap_err();
    assert!(matches!(err, IngestProfileError::Codec(_)));
}

#[test]
fn encode_row_reimpl_accepts_max_tenant_id_len() {
    let tenant = "a".repeat(256);
    assert!(encode_row_reimpl(&tenant, true, &[1.0], b"meta").is_ok());
}

// --- encode_row_reimpl / content_hash_insert_batch_reimpl: ドリフト検出 ----

/// `Storage`（pub API・正本）で書き込んだ行を生 `redb::Database` で再オープンし、
/// [`encode_row_reimpl`] の出力とバイト単位で一致することを検証する。
#[test]
fn encode_row_reimpl_matches_storage_encoded_bytes() {
    let path = unique_db_path("issue396-ingest-accept-encode");
    let _guard = CleanupGuard(path.clone());

    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
    );
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");

    let rows: Vec<(u64, RowInput<'_>)> = vec![
        (
            1,
            RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0, 3.0, 4.0],
                metadata: b"meta-1",
            },
        ),
        (
            2,
            RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[5.0, 6.0, 7.0, 8.0],
                metadata: b"",
            },
        ),
    ];
    let op_id = OperationId::parse("accept-encode-1").expect("valid operation id");
    tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).expect("insert_rows");
    drop(storage);

    let db = Database::open(&path).expect("reopen db read-only");
    let read_txn = db.begin_read().expect("begin_read");
    let table = read_txn.open_table(ROW_TABLE).expect("open row table");

    let expected_1 = encode_row_reimpl("tenant-a", true, &[1.0, 2.0, 3.0, 4.0], b"meta-1")
        .expect("encode row 1");
    let stored_1 = table
        .get(("tenant-a", 1u64))
        .expect("get row 1")
        .expect("row 1 exists");
    assert_eq!(stored_1.value(), expected_1.as_slice());

    let expected_2 =
        encode_row_reimpl("tenant-a", false, &[5.0, 6.0, 7.0, 8.0], b"").expect("encode row 2");
    let stored_2 = table
        .get(("tenant-a", 2u64))
        .expect("get row 2")
        .expect("row 2 exists");
    assert_eq!(stored_2.value(), expected_2.as_slice());
}

/// `tenant::insert_rows` が実際に `op_ledger` へ記録した内容ハッシュ（v2）が、
/// [`content_hash_insert_batch_reimpl`] の再計算結果と一致することを検証する
/// （content_hash 再実装のドリフト検出）。
#[test]
fn content_hash_insert_batch_reimpl_matches_stored_op_ledger_hash() {
    let path = unique_db_path("issue396-ingest-accept-hash");
    let _guard = CleanupGuard(path.clone());

    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
    );
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");

    let embeddings: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let metas: [&[u8]; 3] = [b"m1", b"m2", b""];
    let rows: Vec<(u64, RowInput<'_>)> = (0..3u64)
        .map(|i| {
            (
                i + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: if i % 2 == 0 {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    },
                    embedding: &embeddings[i as usize],
                    metadata: metas[i as usize],
                },
            )
        })
        .collect();
    let op_id = OperationId::parse("accept-hash-1").expect("valid operation id");
    tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).expect("insert_rows");
    drop(storage);

    let db = Database::open(&path).expect("reopen db read-only");
    let read_txn = db.begin_read().expect("begin_read");
    let ledger_table = read_txn
        .open_table(OP_LEDGER_TABLE)
        .expect("open op_ledger");
    let stored = ledger_table
        .get(("tenant-a", "docs", "accept-hash-1"))
        .expect("get ledger entry")
        .expect("ledger entry exists");
    let stored_hash = decode_ledger_entry_v2_reimpl(stored.value()).expect("decode ledger entry");

    let encoded: Vec<Vec<u8>> = (0..3u64)
        .map(|i| {
            encode_row_reimpl(
                "tenant-a",
                i % 2 == 0,
                &embeddings[i as usize],
                metas[i as usize],
            )
            .expect("encode row for hash cross-check")
        })
        .collect();
    let rows_for_hash: Vec<(u64, &[u8])> = (0..3u64)
        .zip(encoded.iter())
        .map(|(i, enc)| (i + 1, enc.as_slice()))
        .collect();
    let recomputed = content_hash_insert_batch_reimpl(&rows_for_hash).expect("recompute hash");
    assert_eq!(recomputed, stored_hash);

    // `last_op`（RECOVER-7）も同じトランザクションで一致する。
    let last_op_table = read_txn.open_table(LAST_OP_TABLE).expect("open last_op");
    let stored_last_op = last_op_table
        .get(("tenant-a", "docs"))
        .expect("get last_op")
        .expect("last_op entry exists");
    assert_eq!(
        stored_last_op.value(),
        last_op_entry_reimpl("accept-hash-1")
    );
}

/// 同一 `operation_id`・同一内容の再送は `TenantWriteError::DuplicateOperationId`
/// （`23505`）として拒否される（既存の重複拒否契約 `23505` の追認。エラー種別の
/// 細目〔内容不一致 `22023` との切り分け〕は既存テスト `recovery_content_hash.rs`
/// に委ね、ここでは content_hash 再実装が絡む経路〔本テストの主眼〕で契約が
/// 破れていないことだけを確認する）。
#[test]
fn insert_rows_rejects_retry_with_identical_content_as_duplicate() {
    let path = unique_db_path("issue396-ingest-accept-retry");
    let _guard = CleanupGuard(path.clone());

    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    );
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");

    let rows: Vec<(u64, RowInput<'_>)> = vec![(
        1,
        RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0],
            metadata: b"m",
        },
    )];
    let op_id = OperationId::parse("accept-retry-1").expect("valid operation id");
    tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).expect("first insert_rows");
    // 同一 operation_id・同一内容の再送は `DuplicateOperationId`（`23505`）で拒否
    // される（keep-first。台帳は追記せず既存記録を保つ）。
    let err = tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).unwrap_err();
    assert!(matches!(
        err,
        engine::tenant::TenantWriteError::DuplicateOperationId
    ));
}

// --- ledger_entry_v2_reimpl / decode_ledger_entry_v2_reimpl -----------------

#[test]
fn ledger_entry_v2_reimpl_roundtrips() {
    let hash = sha256_reimpl(b"roundtrip");
    let encoded = ledger_entry_v2_reimpl(&hash);
    let decoded = decode_ledger_entry_v2_reimpl(&encoded).expect("decode");
    assert_eq!(decoded, hash);
}

#[test]
fn decode_ledger_entry_v2_reimpl_rejects_empty_value() {
    assert!(decode_ledger_entry_v2_reimpl(&[]).is_err());
}

#[test]
fn decode_ledger_entry_v2_reimpl_rejects_unknown_version() {
    let mut buf = vec![9u8];
    buf.extend_from_slice(&[0u8; 32]);
    assert!(decode_ledger_entry_v2_reimpl(&buf).is_err());
}

#[test]
fn decode_ledger_entry_v2_reimpl_rejects_wrong_hash_length() {
    let buf = vec![2u8, 1, 2, 3];
    assert!(decode_ledger_entry_v2_reimpl(&buf).is_err());
}

// --- StageSamples / sum_durations / residual_ns_per_row / ns_per_row -------

#[test]
fn stage_samples_push_and_samples_for_roundtrip() {
    let mut samples = StageSamples::new();
    samples.push(StageId::Precheck, Duration::from_micros(10));
    samples.push(StageId::Precheck, Duration::from_micros(20));
    samples.push(StageId::Commit, Duration::from_micros(5));
    assert_eq!(samples.samples_for(StageId::Precheck).len(), 2);
    assert_eq!(samples.samples_for(StageId::Commit).len(), 1);
    assert!(samples.samples_for(StageId::Encode).is_empty());
}

#[test]
fn sum_durations_sums_all_values() {
    let values = [
        Duration::from_micros(1),
        Duration::from_micros(2),
        Duration::from_micros(3),
    ];
    assert_eq!(sum_durations(&values), Duration::from_micros(6));
}

#[test]
fn sum_durations_of_empty_slice_is_zero() {
    assert_eq!(sum_durations(&[]), Duration::ZERO);
}

#[test]
fn ns_per_row_computes_expected_value() {
    let total = Duration::from_micros(1_000);
    let npr = ns_per_row(total, 1_000).expect("ns_per_row");
    assert!((npr - 1_000.0).abs() < 1e-6);
}

#[test]
fn ns_per_row_rejects_zero_rows() {
    let err = ns_per_row(Duration::from_micros(1), 0).unwrap_err();
    assert_eq!(err, IngestProfileError::ZeroRows);
}

#[test]
fn residual_ns_per_row_positive_when_e2e_exceeds_stage_sum() {
    let e2e = Duration::from_micros(1_000);
    let stage_sum = Duration::from_micros(600);
    let residual = residual_ns_per_row(e2e, stage_sum, 1_000).expect("residual");
    assert!((residual - 400.0).abs() < 1e-6);
}

#[test]
fn residual_ns_per_row_reports_non_monotonic_when_stage_sum_exceeds_e2e() {
    let e2e = Duration::from_micros(600);
    let stage_sum = Duration::from_micros(1_000);
    let err = residual_ns_per_row(e2e, stage_sum, 1_000).unwrap_err();
    assert_eq!(err, IngestProfileError::NonMonotonic);
}

#[test]
fn stage_id_labels_are_unique_and_all_covers_eight_stages() {
    assert_eq!(StageId::ALL.len(), 8);
    let mut labels: Vec<&str> = StageId::ALL.iter().map(|s| s.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), 8);
}
