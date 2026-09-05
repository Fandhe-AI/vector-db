//! `benches/harness/gpu_scaling.rs`（GPU バッチ検索 vs CPU-SIMD の規模別実測
//! ベンチ。手動専用・`benches/gpu_scaling_bench.rs`）の時間非依存な回帰テスト。
//!
//! GPU デバイスにも `engine` クレートの内部型にも依存しない純関数
//! （env パース・出力整形・同点許容つき不一致検知）のみを対象とする
//! （`tests/bench_engine_accept.rs`・`tests/knn_profile_accept.rs` と同一方針）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::gpu_scaling::{
    count_boundary_tolerant_mismatches, format_skip_line, format_unavailable_line, parse_batches,
    parse_dims, parse_measured_iterations, parse_rows, parse_top_k, speedup_ratio,
    GpuScalingResult,
};
use std::time::Duration;

// ---------------------------------------------------------------------
// parse_rows / parse_dims / parse_batches
// ---------------------------------------------------------------------

#[test]
fn parse_rows_defaults_when_unset_or_empty() {
    let default = [20_000, 100_000, 500_000];
    assert_eq!(parse_rows(None, &default), Ok(default.to_vec()));
    assert_eq!(parse_rows(Some(""), &default), Ok(default.to_vec()));
    assert_eq!(parse_rows(Some("   "), &default), Ok(default.to_vec()));
}

#[test]
fn parse_rows_accepts_comma_separated_list_with_whitespace() {
    let default = [1];
    assert_eq!(
        parse_rows(Some(" 10, 20 ,30"), &default),
        Ok(vec![10, 20, 30])
    );
}

#[test]
fn parse_rows_rejects_zero_non_numeric_and_out_of_bound_fail_closed() {
    let default = [1];
    for raw in ["0", "abc", "1.5", "-1", ""] {
        if raw.is_empty() {
            continue; // 空文字列は既定値として受理される契約（別テストで検証済み）
        }
        assert!(
            parse_rows(Some(raw), &default).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
    // 上限（5,000,000）超過は拒否する。
    assert!(parse_rows(Some("5000001"), &default).is_err());
}

#[test]
fn parse_rows_rejects_too_many_entries_fail_closed() {
    let default = [1];
    let too_many = (0..17)
        .map(|i| (i + 1).to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_rows(Some(&too_many), &default).is_err());
}

#[test]
fn parse_dims_and_parse_batches_share_the_same_list_parser() {
    assert_eq!(parse_dims(Some("128,256"), &[1]), Ok(vec![128, 256]));
    assert_eq!(parse_batches(Some("1,8,64"), &[1]), Ok(vec![1, 8, 64]));
    assert!(parse_dims(Some("0"), &[1]).is_err());
    assert!(parse_batches(Some("-5"), &[1]).is_err());
}

// ---------------------------------------------------------------------
// parse_top_k / parse_measured_iterations
// ---------------------------------------------------------------------

#[test]
fn parse_top_k_defaults_and_accepts_within_bound() {
    assert_eq!(parse_top_k(None, 10), Ok(10));
    assert_eq!(parse_top_k(Some(""), 10), Ok(10));
    assert_eq!(parse_top_k(Some("5"), 10), Ok(5));
}

#[test]
fn parse_top_k_rejects_zero_non_numeric_and_over_bound_fail_closed() {
    assert!(parse_top_k(Some("0"), 10).is_err());
    assert!(parse_top_k(Some("abc"), 10).is_err());
    assert!(parse_top_k(Some("10001"), 10).is_err());
}

#[test]
fn parse_measured_iterations_defaults_and_accepts_within_bound() {
    assert_eq!(parse_measured_iterations(None, 20), Ok(20));
    assert_eq!(parse_measured_iterations(Some(""), 20), Ok(20));
    assert_eq!(parse_measured_iterations(Some("50"), 20), Ok(50));
}

#[test]
fn parse_measured_iterations_rejects_below_protocol_minimum_and_non_numeric() {
    assert!(parse_measured_iterations(Some("19"), 20).is_err());
    assert!(parse_measured_iterations(Some("abc"), 20).is_err());
    assert!(parse_measured_iterations(Some("-1"), 20).is_err());
}

// ---------------------------------------------------------------------
// speedup_ratio
// ---------------------------------------------------------------------

#[test]
fn speedup_ratio_computes_cpu_over_gpu() {
    let cpu = Duration::from_micros(1000);
    let gpu = Duration::from_micros(250);
    let ratio = speedup_ratio(cpu, gpu).expect("non-zero denominator");
    assert!((ratio - 4.0).abs() < 1e-9);
}

#[test]
fn speedup_ratio_rejects_zero_denominator_fail_closed() {
    assert!(speedup_ratio(Duration::from_micros(1000), Duration::ZERO).is_err());
}

// ---------------------------------------------------------------------
// count_boundary_tolerant_mismatches
// ---------------------------------------------------------------------

#[test]
fn mismatches_are_zero_when_candidate_matches_baseline_ids_exactly() {
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(3u64, 1.0f32), (1, 3.0), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 0);
}

#[test]
fn mismatches_tolerate_ties_at_the_boundary_score() {
    // baseline の境界スコア（最小値）は 1.0。candidate の id=9 は baseline に
    // 含まれないが、スコアが境界と同値（同点）のため不一致に数えない。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (9, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 0);
}

#[test]
fn mismatches_count_ids_below_boundary_and_absent_from_baseline() {
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    // id=9 は baseline に無くスコアも境界(1.0)未満のため不一致。
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (9, 0.5)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
}

#[test]
fn mismatches_with_empty_baseline_counts_all_candidates_fail_closed() {
    let baseline: Vec<(u64, f32)> = Vec::new();
    let candidate = vec![(1u64, 1.0f32), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 2);
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &[]), 0);
}

#[test]
fn mismatches_count_missing_hits_when_candidate_is_shorter_than_baseline() {
    // GPU 経路が Top-k を取りこぼした（候補が短い・空）場合も欠落分を不一致に
    // 数える。余分な候補だけを数えると取りこぼしが `mismatch=0` に見えてしまう。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &[]), 3);
    // 欠落と境界未満の余分な候補は加算される。
    let candidate = vec![(1u64, 3.0f32), (9, 0.5)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 2);
}

#[test]
fn mismatches_count_baseline_ids_above_boundary_missing_from_candidate() {
    // 境界スコア（1.0）と同点の id=9 が、境界より上位の id=2（2.0）を置き換えている。
    // 件数は一致し id=9 は同点許容の対象だが、上位 id の欠落は不一致に数える。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (3, 1.0), (9, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
    // 境界上（1.0）の id=3 が同点の id=9 に置き換わるだけなら許容する。
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (9, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 0);
}

#[test]
fn mismatches_count_duplicate_ids_in_candidate() {
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    // 長さは一致するが id=1 が重複しており id=3 が欠けている → 重複 1 件。
    let candidate = vec![(1u64, 3.0f32), (1, 3.0), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
}

#[test]
fn mismatches_count_excess_candidates_beyond_baseline_length() {
    // codex-review P2: baseline の全件を含み境界同点の id=4 を 1 件追加しただけの
    // candidate（境界スコア未満ではないため `extra` に捕捉されない）が、件数超過
    // チェック無しでは不一致 0 になっていた。件数超過は不一致として計上する。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0), (4, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
}

// ---------------------------------------------------------------------
// 出力整形
// ---------------------------------------------------------------------

#[test]
fn format_skip_line_includes_all_dimensions_and_reason() {
    let line = format_skip_line(500_000, 256, 256, 10, "exceeds MAX_BATCH_WORK");
    assert!(line.starts_with("gpu_scaling: skip "));
    assert!(line.contains("rows=500000"));
    assert!(line.contains("dim=256"));
    assert!(line.contains("batch=256"));
    assert!(line.contains("k=10"));
    assert!(line.contains("exceeds MAX_BATCH_WORK"));
}

#[test]
fn format_unavailable_line_distinguishes_process_wide_and_combo_scoped_failures() {
    let process_wide = format_unavailable_line(None, "adapter request timed out");
    assert_eq!(
        process_wide,
        "gpu_scaling: gpu unavailable (adapter request timed out)"
    );

    let combo_scoped = format_unavailable_line(Some((500_000, 256, 256, 10)), "buffer too large");
    assert!(combo_scoped.starts_with("gpu_scaling: not measurable "));
    assert!(combo_scoped.contains("rows=500000"));
    assert!(combo_scoped.contains("buffer too large"));
}

#[test]
fn result_line_contains_all_required_fields() {
    let result = GpuScalingResult {
        rows: 100_000,
        dim: 256,
        batch: 64,
        k: 10,
        cpu_simd_p50: Duration::from_micros(500),
        cpu_simd_p95: Duration::from_micros(700),
        gpu_f16_p50: Duration::from_micros(100),
        gpu_f16_p95: Duration::from_micros(150),
        gpu_f32_p50: Duration::from_micros(120),
        gpu_f32_p95: Duration::from_micros(180),
        per_query_cpu_p50: Duration::from_micros(500) / 64,
        per_query_gpu_f16_p50: Duration::from_micros(100) / 64,
        speedup_f16_p95: speedup_ratio(Duration::from_micros(700), Duration::from_micros(150))
            .expect("non-zero denominator"),
        mismatch: 0,
    };
    let line = result.to_string();
    for expected in [
        "gpu_scaling:",
        "rows=100000",
        "dim=256",
        "batch=64",
        "k=10",
        "cpu_simd_p50=",
        "cpu_simd_p95=",
        "gpu_f16_p50=",
        "gpu_f16_p95=",
        "gpu_f32_p50=",
        "gpu_f32_p95=",
        "per_query_cpu_p50=",
        "per_query_gpu_f16_p50=",
        "speedup_f16_p95=",
        "mismatch=0",
    ] {
        assert!(line.contains(expected), "missing {expected:?} in {line:?}");
    }
}
