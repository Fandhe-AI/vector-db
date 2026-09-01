//! SQL パース・束縛コストの比率計算・レポート整形（Issue #360。ポインタ:
//! `docs/spec/05-tasks.md` TASK-nn なし・本 Issue はセッション内キャッシュの
//! 導入可否を実測で判断する検討タスク）。
//!
//! `sql_parse_bind_bench.rs` から呼ばれる時間非依存ロジックのみを持つ
//! （`harness/sql_c1.rs`・`harness/hybrid_latency.rs` と同じ分離方針: 実測タイマーに
//! 依存する部分とロジックを分け、`tests/parse_bind_bench_accept.rs` から
//! `cargo test`〔`make ci` 対象〕で回帰検証できるようにする）。
//!
//! `std` のみに依存する（`harness/mod.rs` 冒頭コメント参照。本モジュール群は
//! `cargo bench` バイナリと統合テストの複数コンパイル単位から `#[path]` で
//! 取り込まれる共有ソースのため、`crate::` を参照しない）。

use std::time::Duration;

/// 判断ゲートの閾値: 「(validate_sql＋bind) の中央値 / フル実行の中央値」が
/// この値以上なら導入検討（Phase 2a）、未満なら見送り（Phase 2b）と機械的に判定する
/// （Issue #360 の事前登録基準。spec 由来の数値基準ではなく、本 Issue 固有の
/// リポ側判断基準のため定数として持つ）。
pub const INTRODUCE_THRESHOLD_RATIO: f64 = 0.05;

/// 1 構成（行数・次元・k の組み合わせ）における 3 系列の実測中央値。
#[derive(Debug, Clone, Copy)]
pub struct ParseBindMeasurement {
    /// `validate_sql` 単体の中央値。
    pub validate_median: Duration,
    /// `validate_sql` ＋ スキーマ取得 ＋ `bind_in_session` の中央値
    /// （キャッシュヒットで省略できる総コストの近似）。
    pub parse_and_bind_median: Duration,
    /// `EngineCore::execute_sql` によるフル実行の中央値。
    pub full_execution_median: Duration,
}

/// パース・束縛コストの比率（`parse_and_bind_median` / `full_execution_median`）を
/// 算出する。分母（フル実行の中央値）が 0 の場合は算出不能として `Err`
/// （`harness::ab::median_ratio` と同じ fail-closed 方針: NaN・+inf を判定式へ
/// 素通りさせない）。
pub fn parse_bind_ratio(measurement: &ParseBindMeasurement) -> Result<f64, &'static str> {
    if measurement.full_execution_median.is_zero() {
        return Err("cannot compute parse_bind_ratio: full_execution median is zero");
    }
    Ok(measurement.parse_and_bind_median.as_secs_f64()
        / measurement.full_execution_median.as_secs_f64())
}

/// 判断ゲート（[`INTRODUCE_THRESHOLD_RATIO`]）と比率を突き合わせ、導入検討が
/// 妥当かを返す（`ratio >= INTRODUCE_THRESHOLD_RATIO` で `true`）。
pub fn should_consider_introducing(ratio: f64) -> bool {
    ratio >= INTRODUCE_THRESHOLD_RATIO
}

/// 1 構成分の実測結果レポート行を整形する。実測値そのもの（中央値・比率）は
/// spec 由来の非公開閾値ではなく本 Issue の検討材料そのもの（ADR への記録が
/// 受け入れ条件）であるため、`sql_c1_bench.rs` 等と異なり verbose ガードなしで
/// 常に出力する（オーナー判断 2026-08-29 の公開許可範囲。
/// `.claude/rules/spec-confidentiality.md`「許可される参照」参照）。
pub fn render_measurement_line(config_label: &str, measurement: &ParseBindMeasurement) -> String {
    let ratio = parse_bind_ratio(measurement);
    match ratio {
        Ok(r) => format!(
            "parse_bind_ratio({config_label}): validate_median={:?} parse_and_bind_median={:?} full_execution_median={:?} ratio={r:.4} consider_introducing={}",
            measurement.validate_median,
            measurement.parse_and_bind_median,
            measurement.full_execution_median,
            should_consider_introducing(r),
        ),
        Err(e) => format!("parse_bind_ratio({config_label}): unavailable ({e})"),
    }
}
