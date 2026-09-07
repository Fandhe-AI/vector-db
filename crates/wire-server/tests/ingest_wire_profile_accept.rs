//! `benches/harness/ingest_wire.rs`（Issue #484。`ingest_wire_profile_bench.rs`
//! が単文 `INSERT` の wire 往復を切り分けるための rows／rounds パース・可分性
//! 検証・rows/s 換算を担う純関数群）の回帰テスト。
//!
//! `crates/wire-server/tests/knn_wire_profile_accept.rs` と同様、時間依存の
//! ベンチ本体は実行せず `#[path]` で取り込んだ純関数のみを `cargo test`
//! （`make ci` 対象）で検証する。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::time::Duration;

use harness::ingest_wire::{
    parse_rounds, parse_rows, refuse_under_github_actions, rows_per_round, rows_per_sec,
    IngestWireError, DEFAULT_ROUNDS, DEFAULT_ROWS,
};

#[test]
fn refuse_under_github_actions_rejects_when_set() {
    assert_eq!(
        refuse_under_github_actions(true),
        Err(IngestWireError::RefusedUnderGitHubActions)
    );
}

#[test]
fn refuse_under_github_actions_allows_when_unset() {
    assert_eq!(refuse_under_github_actions(false), Ok(()));
}

#[test]
fn parse_rows_defaults_when_unset() {
    assert_eq!(parse_rows(None), Ok(DEFAULT_ROWS));
}

#[test]
fn parse_rows_accepts_boundary_values() {
    assert_eq!(parse_rows(Some("5000")), Ok(5_000));
    assert_eq!(parse_rows(Some("100000")), Ok(100_000));
}

#[test]
fn parse_rows_rejects_below_minimum() {
    assert!(matches!(
        parse_rows(Some("4999")),
        Err(IngestWireError::InvalidEnv { .. })
    ));
}

#[test]
fn parse_rows_rejects_above_maximum() {
    assert!(matches!(
        parse_rows(Some("100001")),
        Err(IngestWireError::InvalidEnv { .. })
    ));
}

#[test]
fn parse_rows_rejects_non_numeric() {
    assert!(matches!(
        parse_rows(Some("abc")),
        Err(IngestWireError::InvalidEnv { .. })
    ));
}

#[test]
fn parse_rounds_defaults_when_unset() {
    assert_eq!(parse_rounds(None), Ok(DEFAULT_ROUNDS));
}

#[test]
fn parse_rounds_accepts_boundary_values() {
    assert_eq!(parse_rounds(Some("5")), Ok(5));
    assert_eq!(parse_rounds(Some("50")), Ok(50));
}

#[test]
fn parse_rounds_rejects_below_minimum() {
    assert!(matches!(
        parse_rounds(Some("4")),
        Err(IngestWireError::InvalidEnv { .. })
    ));
}

#[test]
fn parse_rounds_rejects_above_maximum() {
    assert!(matches!(
        parse_rounds(Some("51")),
        Err(IngestWireError::InvalidEnv { .. })
    ));
}

#[test]
fn rows_per_round_divides_evenly() {
    assert_eq!(rows_per_round(25_000, 5), Ok(5_000));
}

#[test]
fn rows_per_round_rejects_non_divisible() {
    assert!(matches!(
        rows_per_round(5_001, 5),
        Err(IngestWireError::InvalidEnv { .. })
    ));
}

#[test]
fn rows_per_sec_computes_expected_value() {
    let v = rows_per_sec(1000, Duration::from_secs(2)).expect("rows_per_sec");
    assert!((v - 500.0).abs() < 1e-9);
}

#[test]
fn rows_per_sec_rejects_zero_stmts_or_zero_elapsed() {
    assert!(matches!(
        rows_per_sec(0, Duration::from_secs(1)),
        Err(IngestWireError::ZeroRows)
    ));
    assert!(matches!(
        rows_per_sec(10, Duration::ZERO),
        Err(IngestWireError::ZeroRows)
    ));
}
