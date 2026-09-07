//! `benches/harness/bench_engine.rs`（Issue #413。`feature_bench.rs`・
//! `knn_profile_bench.rs` が ANN opt-in（Issue #403 B 案）・規模スケールを env
//! 変数で選ぶための純パース関数）の回帰テスト。
//!
//! `knn_profile_accept.rs` と同様、時間依存のベンチ本体は実行せず `#[path]` で
//! 取り込んだ純関数のみを `cargo test`（`make ci` 対象）で検証する。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::bench_engine::{
    expected_arm, expected_arm_acorn, parse_acorn_max_visible_ratio, parse_dim, parse_engine,
    parse_flag, parse_full_scan_ratio, parse_scale, parse_sparse_visited_max, parse_visible_ratio,
    BenchEngine, ExpectedArm,
};

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
fn parse_engine_accepts_hnsw_f16() {
    assert_eq!(parse_engine(Some("hnsw_f16")), Ok(BenchEngine::HnswF16));
}

#[test]
fn parse_engine_accepts_hnsw_i8() {
    assert_eq!(parse_engine(Some("hnsw_i8")), Ok(BenchEngine::HnswI8));
}

#[test]
fn parse_engine_rejects_unknown_values_fail_closed() {
    for raw in [
        "HNSW",
        "ann",
        "bruteforce",
        "0",
        "HNSW_F16",
        "f16",
        "hnswf16",
    ] {
        assert!(
            parse_engine(Some(raw)).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn bench_engine_token_round_trips_through_parse_engine() {
    for engine in [
        BenchEngine::BruteForce,
        BenchEngine::Hnsw,
        BenchEngine::HnswF16,
    ] {
        assert_eq!(parse_engine(Some(engine.token())), Ok(engine));
    }
}

#[test]
fn parse_flag_defaults_to_false_and_accepts_zero_one() {
    assert_eq!(parse_flag(None), Ok(false));
    assert_eq!(parse_flag(Some("")), Ok(false));
    assert_eq!(parse_flag(Some("0")), Ok(false));
    assert_eq!(parse_flag(Some(" 1 ")), Ok(true));
}

#[test]
fn parse_flag_rejects_unknown_values_fail_closed() {
    for raw in ["true", "false", "yes", "no", "2", "-1", "TRUE"] {
        assert!(
            parse_flag(Some(raw)).is_err(),
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

#[test]
fn parse_dim_defaults_and_accepts_within_bound() {
    assert_eq!(parse_dim(None, 128, 4096), Ok(128));
    assert_eq!(parse_dim(Some(""), 128, 4096), Ok(128));
    assert_eq!(parse_dim(Some("768"), 128, 4096), Ok(768));
    assert_eq!(parse_dim(Some(" 1536 "), 128, 4096), Ok(1536));
    assert_eq!(parse_dim(Some("4096"), 128, 4096), Ok(4096));
}

#[test]
fn parse_dim_rejects_zero_non_numeric_and_over_bound_fail_closed() {
    for raw in ["0", "-1", "abc", "1.5", "4097", "128x"] {
        assert!(
            parse_dim(Some(raw), 128, 4096).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn parse_visible_ratio_defaults_to_none_when_unset() {
    assert_eq!(parse_visible_ratio(None, 1_000), Ok(None));
    assert_eq!(parse_visible_ratio(Some(""), 1_000), Ok(None));
}

#[test]
fn parse_visible_ratio_accepts_one_over_n_within_bound() {
    assert_eq!(parse_visible_ratio(Some("1/2"), 1_000), Ok(Some(2)));
    assert_eq!(parse_visible_ratio(Some("1/50"), 1_000), Ok(Some(50)));
    assert_eq!(parse_visible_ratio(Some(" 1/10 "), 1_000), Ok(Some(10)));
    assert_eq!(parse_visible_ratio(Some("1/1000"), 1_000), Ok(Some(1_000)));
}

#[test]
fn parse_visible_ratio_rejects_non_one_numerator_zero_and_over_bound() {
    for raw in ["2/5", "1/0", "0/1", "1/1001", "1", "1/", "1/-1", "abc"] {
        assert!(
            parse_visible_ratio(Some(raw), 1_000).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn parse_full_scan_ratio_defaults_to_none_when_unset() {
    assert_eq!(parse_full_scan_ratio(None), Ok(None));
    assert_eq!(parse_full_scan_ratio(Some("")), Ok(None));
}

#[test]
fn parse_full_scan_ratio_accepts_num_over_den_including_edge_ratios() {
    assert_eq!(parse_full_scan_ratio(Some("1/10")), Ok(Some((1, 10))));
    assert_eq!(parse_full_scan_ratio(Some("0/1")), Ok(Some((0, 1))));
    assert_eq!(parse_full_scan_ratio(Some("1/1")), Ok(Some((1, 1))));
    assert_eq!(parse_full_scan_ratio(Some(" 3/4 ")), Ok(Some((3, 4))));
}

#[test]
fn parse_full_scan_ratio_rejects_zero_denominator_and_numerator_over_denominator() {
    for raw in ["1/0", "2/1", "abc", "1", "1/2/3", "-1/2"] {
        assert!(
            parse_full_scan_ratio(Some(raw)).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn parse_sparse_visited_max_defaults_to_none_when_unset() {
    assert_eq!(parse_sparse_visited_max(None), Ok(None));
    assert_eq!(parse_sparse_visited_max(Some("")), Ok(None));
}

#[test]
fn parse_sparse_visited_max_accepts_non_negative_integers() {
    assert_eq!(parse_sparse_visited_max(Some("0")), Ok(Some(0)));
    assert_eq!(parse_sparse_visited_max(Some("100")), Ok(Some(100)));
    assert_eq!(parse_sparse_visited_max(Some(" 42 ")), Ok(Some(42)));
    assert_eq!(
        parse_sparse_visited_max(Some(&usize::MAX.to_string())),
        Ok(Some(usize::MAX))
    );
}

#[test]
fn parse_sparse_visited_max_rejects_non_integer_and_negative() {
    for raw in ["abc", "-1", "1.5", "1/10", "0x10"] {
        assert!(
            parse_sparse_visited_max(Some(raw)).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn expected_arm_boundary_matches_search_with_overlay_comparison() {
    // `sql::hnsw_cache::search_with_overlay` は `visible * den < index_len * num`
    // なら plain scan（Issue #487 実装コメント参照）。境界ちょうど（等号）は
    // ANN 側になる。
    assert_eq!(
        expected_arm(10, 100, (1, 10)).unwrap(),
        ExpectedArm::AnnMasked
    );
    assert_eq!(
        expected_arm(9, 100, (1, 10)).unwrap(),
        ExpectedArm::PlainScanRatio
    );
}

#[test]
fn expected_arm_zero_ratio_is_always_ann_masked() {
    assert_eq!(
        expected_arm(0, 100, (0, 1)).unwrap(),
        ExpectedArm::AnnMasked
    );
    assert_eq!(
        expected_arm(1, 1_000_000, (0, 1)).unwrap(),
        ExpectedArm::AnnMasked
    );
}

#[test]
fn expected_arm_full_ratio_is_plain_scan_unless_fully_visible() {
    assert_eq!(
        expected_arm(50, 100, (1, 1)).unwrap(),
        ExpectedArm::PlainScanRatio
    );
    assert_eq!(
        expected_arm(100, 100, (1, 1)).unwrap(),
        ExpectedArm::AnnMasked
    );
}

#[test]
fn expected_arm_rejects_zero_denominator_and_reports_overflow_fail_closed() {
    assert!(expected_arm(1, 1, (1, 0)).is_err());
    // `lhs = visible.checked_mul(den)`: den=2 かつ visible=u64::MAX でオーバーフロー。
    assert!(expected_arm(u64::MAX, 2, (1, 2)).is_err());
    // `rhs = index_len.checked_mul(num)`: num=2 かつ index_len=u64::MAX でオーバーフロー。
    assert!(expected_arm(2, u64::MAX, (2, 2)).is_err());
}

// --- ACORN-1（Issue #501・#502）: `parse_acorn_max_visible_ratio`／
// `expected_arm_acorn` ------------------------------------------------------

#[test]
fn parse_acorn_max_visible_ratio_defaults_to_none_when_unset() {
    assert_eq!(parse_acorn_max_visible_ratio(None), Ok(None));
    assert_eq!(parse_acorn_max_visible_ratio(Some("")), Ok(None));
}

#[test]
fn parse_acorn_max_visible_ratio_accepts_num_over_den_including_edge_ratios() {
    assert_eq!(parse_acorn_max_visible_ratio(Some("1/1")), Ok(Some((1, 1))));
    assert_eq!(
        parse_acorn_max_visible_ratio(Some("4/10")),
        Ok(Some((4, 10)))
    );
    assert_eq!(parse_acorn_max_visible_ratio(Some("0/1")), Ok(Some((0, 1))));
    assert_eq!(
        parse_acorn_max_visible_ratio(Some(" 3/4 ")),
        Ok(Some((3, 4)))
    );
}

#[test]
fn parse_acorn_max_visible_ratio_rejects_zero_denominator_and_numerator_over_denominator() {
    for raw in ["1/0", "2/1", "abc", "1", "1/2/3", "-1/2"] {
        assert!(
            parse_acorn_max_visible_ratio(Some(raw)).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn expected_arm_acorn_none_matches_expected_arm() {
    // `acorn_max_visible_ratio` 未設定（`None`）は `expected_arm` と完全に
    // 同一の判定を返す（後方互換・既定経路の出力不変）。
    for (visible, index_len, full_scan_ratio) in [
        (10u64, 100u64, (1u32, 10u32)),
        (9, 100, (1, 10)),
        (50, 100, (1, 1)),
    ] {
        assert_eq!(
            expected_arm_acorn(visible, index_len, full_scan_ratio, None).unwrap(),
            expected_arm(visible, index_len, full_scan_ratio).unwrap()
        );
    }
}

#[test]
fn expected_arm_acorn_upgrades_ann_masked_to_two_hop_within_range() {
    // `full_scan_ratio=1/10`・`acorn_max_visible_ratio=4/10`。r=10/100=1/10
    // は full_scan_ratio 境界ちょうどで base=AnnMasked、かつ 1/10 <= 4/10 の
    // 範囲内なので TwoHop へ格上げされる。
    assert_eq!(
        expected_arm_acorn(10, 100, (1, 10), Some((4, 10))).unwrap(),
        ExpectedArm::AnnMaskedTwoHop
    );
    // r=1/2（50/100）は base=AnnMasked だが 1/2 > 4/10 なので格上げされず
    // 既存の 1-hop 判定（AnnMasked）のまま。
    assert_eq!(
        expected_arm_acorn(50, 100, (1, 10), Some((4, 10))).unwrap(),
        ExpectedArm::AnnMasked
    );
}

#[test]
fn expected_arm_acorn_does_not_upgrade_plain_scan_ratio() {
    // base=PlainScanRatio（r=9/100 < full_scan_ratio=1/10）のときは
    // `acorn_max_visible_ratio` が設定されていても格上げしない
    // （ACORN は `full_scan_ratio` 未満の可視カーディナリティ比には効かない。
    // `sql::hnsw_cache::TraversalRegime::PlainScan` 判定と同じ優先順位）。
    assert_eq!(
        expected_arm_acorn(9, 100, (1, 10), Some((1, 1))).unwrap(),
        ExpectedArm::PlainScanRatio
    );
}

#[test]
fn expected_arm_acorn_boundary_at_acorn_max_visible_ratio_is_inclusive() {
    // `visible * den <= index_len * num` は `<=`（境界含む）。
    // acorn=4/10, index_len=100 のとき visible=40 はちょうど境界。
    assert_eq!(
        expected_arm_acorn(40, 100, (1, 10), Some((4, 10))).unwrap(),
        ExpectedArm::AnnMaskedTwoHop
    );
    assert_eq!(
        expected_arm_acorn(41, 100, (1, 10), Some((4, 10))).unwrap(),
        ExpectedArm::AnnMasked
    );
}

#[test]
fn expected_arm_acorn_rejects_zero_denominator_and_reports_overflow_fail_closed() {
    // full_scan_ratio 側の denominator=0 は `expected_arm` 経由でそのまま拒否。
    assert!(expected_arm_acorn(1, 1, (1, 0), Some((1, 1))).is_err());
    // acorn 側の denominator=0（base が AnnMasked のときのみ到達する分岐）。
    assert!(expected_arm_acorn(100, 100, (1, 1), Some((1, 0))).is_err());
    // acorn 側の乗算オーバーフロー。
    assert!(expected_arm_acorn(u64::MAX, 2, (0, 1), Some((1, 2))).is_err());
    assert!(expected_arm_acorn(2, u64::MAX, (0, 1), Some((2, 2))).is_err());
}
