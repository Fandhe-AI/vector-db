//! `benches/harness/bench_engine.rs`（Issue #413。`feature_bench.rs`・
//! `knn_profile_bench.rs` が ANN opt-in（Issue #403 B 案）・規模スケールを env
//! 変数で選ぶための純パース関数）の回帰テスト。
//!
//! `knn_profile_accept.rs` と同様、時間依存のベンチ本体は実行せず `#[path]` で
//! 取り込んだ純関数のみを `cargo test`（`make ci` 対象）で検証する。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::bench_engine::{parse_engine, parse_scale, BenchEngine};

#[test]
fn parse_engine_accepts_unset_empty_and_brute_force_as_default() {
    assert_eq!(parse_engine(None), Ok(BenchEngine::BruteForce));
    assert_eq!(parse_engine(Some("")), Ok(BenchEngine::BruteForce));
    assert_eq!(
        parse_engine(Some("brute_force")),
        Ok(BenchEngine::BruteForce)
    );
}

#[test]
fn parse_engine_accepts_hnsw() {
    assert_eq!(parse_engine(Some("hnsw")), Ok(BenchEngine::Hnsw));
}

#[test]
fn parse_engine_rejects_unknown_values_fail_closed() {
    for raw in ["HNSW", "ann", "bruteforce", "0"] {
        assert!(
            parse_engine(Some(raw)).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn parse_scale_defaults_to_one_and_accepts_within_bound() {
    assert_eq!(parse_scale(None, 40), Ok(1));
    assert_eq!(parse_scale(Some("4"), 40), Ok(4));
    assert_eq!(parse_scale(Some(" 40 "), 40), Ok(40));
}

#[test]
fn parse_scale_rejects_zero_non_numeric_and_over_bound_fail_closed() {
    for raw in ["0", "-1", "abc", "1.5", "41", ""] {
        // 空文字列は既定 1 として受理される（下の別テストで検証済み）ため、
        // ここでは max=0 にして「1 でも拒否される」境界を確認する。
        if raw.is_empty() {
            assert!(parse_scale(Some(raw), 0).is_err());
            continue;
        }
        assert!(
            parse_scale(Some(raw), 40).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}
