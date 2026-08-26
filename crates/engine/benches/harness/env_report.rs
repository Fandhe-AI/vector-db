//! 実行環境の記録（TASK-83。専有環境再測定レポートへの転記用）。
//!
//! `sql_c1_bench.rs` が p95・Recall・A/B 計測の直前に 1 回出力する。専有環境か
//! どうかは自動判定できない（運用者の明示宣言に委ねる。`sql_c1_bench.rs` の
//! `BENCH_DEDICATED_ENV` 参照）ため、本モジュールは OS・アーキテクチャ・論理
//! コア数・検出 ISA・直近の負荷平均という「実測条件を後から確認できる」情報のみを
//! 集める。テナント ID・DB パス等の機微情報は含めない（`.claude/rules/security.md`
//! 「P0 秘密情報の混入防止」・「ログ・情報漏えい」観点）。
//!
//! `std` のみに依存する（`sql_c1.rs` と同じ理由。`engine::isa` は呼び出し元
//! （`sql_c1_bench.rs`）が計測して文字列を渡す——本モジュール自体は `engine` に
//! 依存しない）。

use std::fmt;

/// 1 回の計測実行の環境スナップショット。
#[derive(Debug, Clone)]
pub struct EnvReport {
    pub os: &'static str,
    pub arch: &'static str,
    /// `std::thread::available_parallelism()` が返す論理コア数。取得失敗時は `0`
    /// （fail-closed に「不明」を表す。`Display` では `unavailable` と表示する）。
    pub logical_cpus: usize,
    /// 呼び出し元が `engine::isa::current().isa()` 等から得た検出 ISA の表示文字列
    /// （英語。`Debug` 由来の表現をそのまま渡す想定）。
    pub isa: String,
    /// `/proc/loadavg` の内容（先頭行。Linux 以外・読み取り不可の環境では
    /// `"unavailable"`）。
    pub loadavg: String,
}

impl EnvReport {
    /// 現在の実行環境を集める。`std::thread::available_parallelism` の失敗・
    /// `/proc/loadavg` の読み取り失敗はいずれも panic させず `unavailable` 相当の
    /// 値へ落とす（本モジュールは診断情報の収集であり、収集失敗が計測本体を
    /// 止める理由にはならない。fail-closed が必要なのは合否判定側であって、
    /// 環境記録の欠落そのものではない）。
    ///
    /// `isa` は呼び出し元が計測して渡す（`engine` への依存を持たない設計。
    /// モジュール冒頭コメント参照）。
    pub fn capture(isa: impl Into<String>) -> Self {
        let logical_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
        let loadavg = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
            .unwrap_or_else(|| "unavailable".to_string());
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            logical_cpus,
            isa: isa.into(),
            loadavg,
        }
    }
}

impl fmt::Display for EnvReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cpus = if self.logical_cpus == 0 {
            "unavailable".to_string()
        } else {
            self.logical_cpus.to_string()
        };
        write!(
            f,
            "env: os={} arch={} logical_cpus={} isa={} loadavg={}",
            self.os, self.arch, cpus, self.isa, self.loadavg
        )
    }
}
