//! `benches/harness/parse_bind.rs`（Issue #360「SQL パース・束縛結果のセッション内
//! キャッシュ検討（実測裏付け前提）」）の回帰テスト。
//!
//! `sql_parse_bind_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hybrid_latency_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約——比率計算・判断ゲート・レポート整形——のみを `#[path]` で
//! 取り込み `cargo test`〔`make ci` 対象〕で検証する）。
//!
//! `harness/parse_bind.rs` 自体に `#[cfg(test)] mod tests` を置かない理由は
//! `tests/hybrid_latency_accept.rs` 冒頭コメントと同一（`harness/*.rs` は `#[path]`
//! 経由で bench クレートと複数の統合テストクレートの双方に取り込まれ、bench
//! コンパイル時は `#[test]` 項目が丸ごと除去されるため `use super::*;` が unused
//! import になる）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::time::Duration;

use harness::parse_bind::{
    parse_bind_ratio, render_measurement_line, should_consider_introducing, ParseBindMeasurement,
    INTRODUCE_THRESHOLD_RATIO,
};

// --- parse_bind_ratio ---

#[test]
fn parse_bind_ratio_computes_expected_value() {
    let m = ParseBindMeasurement {
        validate_median: Duration::from_micros(10),
        parse_and_bind_median: Duration::from_micros(50),
        full_execution_median: Duration::from_micros(1000),
    };
    let ratio = parse_bind_ratio(&m).expect("non-zero denominator");
    assert!((ratio - 0.05).abs() < 1e-9);
}

#[test]
fn parse_bind_ratio_rejects_zero_denominator() {
    let m = ParseBindMeasurement {
        validate_median: Duration::from_micros(10),
        parse_and_bind_median: Duration::from_micros(50),
        full_execution_median: Duration::ZERO,
    };
    assert!(parse_bind_ratio(&m).is_err());
}

// --- should_consider_introducing ---

#[test]
fn should_consider_introducing_uses_threshold_boundary() {
    assert!(should_consider_introducing(INTRODUCE_THRESHOLD_RATIO));
    assert!(should_consider_introducing(
        INTRODUCE_THRESHOLD_RATIO + 0.001
    ));
    assert!(!should_consider_introducing(
        INTRODUCE_THRESHOLD_RATIO - 0.001
    ));
}

// --- render_measurement_line ---

#[test]
fn render_measurement_line_includes_ratio_and_decision() {
    let m = ParseBindMeasurement {
        validate_median: Duration::from_micros(10),
        parse_and_bind_median: Duration::from_micros(10),
        full_execution_median: Duration::from_micros(1000),
    };
    let line = render_measurement_line("small", &m);
    assert!(line.contains("small"));
    assert!(line.contains("ratio=0.0100"));
    assert!(line.contains("consider_introducing=false"));
}

#[test]
fn render_measurement_line_marks_introduction_worthy_ratio() {
    let m = ParseBindMeasurement {
        validate_median: Duration::from_micros(100),
        parse_and_bind_median: Duration::from_micros(100),
        full_execution_median: Duration::from_micros(1000),
    };
    let line = render_measurement_line("realistic", &m);
    assert!(line.contains("consider_introducing=true"));
}

#[test]
fn render_measurement_line_reports_unavailable_on_zero_denominator() {
    let m = ParseBindMeasurement {
        validate_median: Duration::from_micros(10),
        parse_and_bind_median: Duration::from_micros(50),
        full_execution_median: Duration::ZERO,
    };
    let line = render_measurement_line("small", &m);
    assert!(line.contains("unavailable"));
}
