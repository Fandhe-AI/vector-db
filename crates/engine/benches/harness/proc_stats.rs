//! `/proc/self/status` から常駐メモリ量（VmRSS・VmHWM）を読む std のみのヘルパ
//! （Issue #389）。`examples/feature_bench.rs::parse_kb`／`read_proc_stats` と
//! 同等の実装だが、benches と examples はコンパイル単位が別でモジュールを共有
//! できないため、本モジュールを `harness` 側に独立して持つ（依存追加なし）。
//!
//! `hybrid_profile_bench.rs` が `SparseIndex` 常駐時の RSS 増分を計測する
//! （Issue #389・受け入れ条件 5「メモリ増分を bench-hybrid-profile の RSS で
//! 記録する」）ための呼び出し口。Linux 以外・`/proc` が読めない環境（コンテナ
//! 制限等）では `None` を返し、診断目的であるためベンチ全体は止めない。

/// `"VmRSS:    1234 kB"` のような 1 行から数値部分（kB）を読む純関数。
/// 前置ラベル（`"VmRSS:"` 等）は呼び出し側が `strip_prefix` 済みの残りを渡す。
/// 空白区切りの先頭トークンが数値として読めない場合は `None`（untrusted な
/// 環境差・書式差を診断失敗として扱い、決め打ちの既定値で誤魔化さない）。
pub fn parse_kb_line(rest: &str) -> Option<u64> {
    rest.split_whitespace().next()?.parse().ok()
}

/// 現プロセスの `/proc/self/status` から `VmRSS`（kB）を読む。読めない場合は
/// `None`（Linux 以外・sandbox 制限等）。
pub fn read_vm_rss_kb() -> Option<u64> {
    read_status_field("VmRSS:")
}

/// 現プロセスの `/proc/self/status` から `VmHWM`（ピーク RSS、kB）を読む。
/// 読めない場合は `None`。
pub fn read_vm_hwm_kb() -> Option<u64> {
    read_status_field("VmHWM:")
}

/// `/proc/self/status` から `label`（`"VmRSS:"` 等）で始まる行を探し、
/// [`parse_kb_line`] で数値化する共通実装。
fn read_status_field(label: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(label).and_then(parse_kb_line))
}

// 単体テストは他 `harness/*.rs`（`dot_kernel.rs` 等）と同じ方針で
// `crates/engine/tests/hybrid_profile_accept.rs` に置く（時間非依存の判定
// ロジックを `make ci` 側の `cargo test` から回帰検証する契約。モジュール
// 冒頭ドキュメントコメント参照）。
