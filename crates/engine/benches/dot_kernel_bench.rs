//! `isa.rs` dot カーネルの複数アキュムレータ化（Issue #365。前提: Issue #362・
//! `docs/design/knn-stage-profile.md`「`dot_lanes` の実アセンブリ確認」節）の
//! マイクロベンチ実測入口。`docs/design/dot-kernel-multi-accumulator.md` の
//! 交互実行手順（ベースライン/候補バイナリを別々にビルドし、本バイナリの出力を
//! 比較する）から使う。単一ビルド内での自己 A/B（旧カーネルの複製）は行わない
//! （`unsafe` を増やさないため。同 ADR「不採用形」節参照）。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make bench-dot-kernel`
//! からの手動実行専用）。`harness::dot_kernel::refuse_under_github_actions` で
//! defense-in-depth の拒否を行う（`hybrid_latency_bench.rs` 等と同一方針）。
//!
//! # 測定対象・出力
//!
//! `dims = [100, 128, 384, 768, 1536]` × `WorkingSet::{CacheResident, ArenaScale}`
//! の組み合わせごとに、`engine::isa::current().dot` を `#[inline(never)]` の
//! `dot_wrapper` 越しに全行へ適用して総和するワークロードを計測する
//! （`dot_wrapper` は `objdump` での逆アセンブル確認の入口も兼ねる。
//! `docs/design/dot-kernel-multi-accumulator.md`「実アセンブリ確認」節参照）。
//! 各 dim で `engine::isa::dot_scalar` との許容差検証を行い、不一致なら実測値を
//! 出力せず非ゼロ終了する（fail-closed）。最後に診断 A/B（`current().dot` vs
//! `dot_scalar`。SIMD 実効倍率の情報提供・合否に数えない）を出力する。
//!
//! `isa::current().isa()` が `Scalar`（SIMD 拡張なし）の環境では SIMD 経路が
//! 測定不能なため、`simd_bench.rs` と同じ方針で非ゼロ終了する。

#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::dot_kernel::{
    check_matches_scalar_reference, classify_change, generate_corpus, generate_query, ns_per_dot,
    refuse_under_github_actions, render_line, rows_for, speedup_ratio, WorkingSet,
};
use harness::env_report::EnvReport;
use harness::protocol::{run, MeasurementConfig};

use engine::isa::{self, DetectedIsa};

const DIMS: [usize; 5] = [100, 128, 384, 768, 1536];
const WORKING_SETS: [WorkingSet; 2] = [WorkingSet::CacheResident, WorkingSet::ArenaScale];

/// cache 常駐段でタイマー粒度を稼ぐための作業集合の反復走査回数（1 サンプルの
/// 中で作業集合を複数回なめることで、1 反復あたりの計測時間を `Instant` の
/// 実用的な分解能より十分大きくする。`knn_profile_bench.rs` の S5' と同形）。
const CACHE_RESIDENT_REPEAT: usize = 200;

/// `GITHUB_ACTIONS` が設定されているか（値は見ず存在有無のみ判定。
/// `simd_bench.rs::running_under_github_actions` と同一パターン）。
fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// dim・rows からコーパスを生成し `dot_wrapper` 適用の総和を計測する 1 段分。
/// `check_matches_scalar_reference` に失敗した場合は `Err` を返し呼び出し元
/// （`main`）が非ゼロ終了する（fail-closed。実測値を出力しない）。
fn measure_stage(label: &str, working_set: WorkingSet, dim: usize) -> Result<(usize, f64), String> {
    let rows = rows_for(working_set, dim).map_err(|e| e.to_string())?;
    let corpus = generate_corpus(0xC0FF_EE00 ^ dim as u64, dim, rows).map_err(|e| e.to_string())?;
    let query = generate_query(0xC0FF_EE00 ^ dim as u64, dim);

    let repeat = match working_set {
        WorkingSet::CacheResident => CACHE_RESIDENT_REPEAT,
        WorkingSet::ArenaScale => 1,
    };

    // 各行を個別にスカラー参照と突き合わせる（計測ループへ入る前の fail-closed
    // 検証）。行ごとの誤差を総和してから比較すると複数行の正負誤差が相殺されて
    // 個々の行の誤計算を見逃しうる（codex-review 指摘）ため、`dot_wrapper` の
    // 各行結果を対応する `dot_scalar` 結果と 1 行ずつ照合し、最初の不一致で
    // 即座に拒否する。
    for (row_idx, chunk) in corpus.chunks_exact(dim).enumerate() {
        let expected = isa::dot_scalar(chunk, &query);
        let actual = dot_wrapper(chunk, &query);
        check_matches_scalar_reference(actual, expected, expected)
            .map_err(|e| format!("{label} dim={dim} row={row_idx}: {e}"))?;
    }

    let config = MeasurementConfig::new(20, 50, 0xC0FF_EE00 ^ dim as u64)
        .map_err(|e| format!("{label} dim={dim}: {e}"))?;
    let measurement = run(&config, || {
        let mut sum = 0f32;
        for _ in 0..repeat {
            for chunk in corpus.chunks_exact(dim) {
                sum += dot_wrapper(chunk, &query);
            }
        }
        sum
    })
    .map_err(|e| format!("{label} dim={dim}: {e}"))?;

    let total_dots = rows.saturating_mul(repeat);
    let ns = ns_per_dot(measurement.summary.median, total_dots).map_err(|e| e.to_string())?;
    println!(
        "{}",
        render_line(
            label,
            working_set,
            dim,
            rows,
            measurement.summary.median,
            ns
        )
    );
    Ok((rows, ns))
}

/// `engine::isa::current().dot` への `#[inline(never)]` 入口。計測ループ・
/// `objdump` での逆アセンブル確認の双方から使う（モジュール冒頭コメント参照）。
#[inline(never)]
fn dot_wrapper(a: &[f32], b: &[f32]) -> f32 {
    isa::current().dot(a, b)
}

/// `engine::isa::dot_scalar` への `#[inline(never)]` 入口（診断 A/B の B 側）。
#[inline(never)]
fn dot_scalar_wrapper(a: &[f32], b: &[f32]) -> f32 {
    isa::dot_scalar(a, b)
}

fn main() {
    if let Err(e) = refuse_under_github_actions(running_under_github_actions()) {
        eprintln!("dot_kernel_bench: {e}");
        std::process::exit(1);
    }

    let detected = isa::current().isa();
    let env = EnvReport::capture(format!("{detected:?}"));
    println!("{env}");

    if detected == DetectedIsa::Scalar {
        eprintln!(
            "dot_kernel_bench: detected ISA is Scalar; SIMD dot kernel path is unmeasurable on this host"
        );
        std::process::exit(1);
    }

    let mut had_error = false;
    for &working_set in &WORKING_SETS {
        for &dim in &DIMS {
            if let Err(e) = measure_stage("current", working_set, dim) {
                eprintln!("dot_kernel_bench: {e}");
                had_error = true;
            }
        }
    }
    if had_error {
        std::process::exit(1);
    }

    // 診断 A/B: SIMD 実効倍率の情報提供（合否に数えない。`simd_bench.rs::
    // diagnostic_ab` と同型の位置付け）。cache 常駐・dim768 のみで代表させる
    // （全 dim × working_set を A/B すると `bench.yml` タイムアウト級の時間が
    // かかる既存ベンチの教訓〔`simd_bench.rs` DIAG_AB_ROW_COUNT コメント参照〕を
    // 踏まえ、本ベンチも代表点 1 つに絞る）。
    let diag_dim = 768usize;
    let diag_rows = match rows_for(WorkingSet::CacheResident, diag_dim) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("dot_kernel_bench: diagnostic ab skipped: {e}");
            return;
        }
    };
    let diag_corpus = match generate_corpus(0xDEAD_BEEF, diag_dim, diag_rows) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dot_kernel_bench: diagnostic ab skipped: {e}");
            return;
        }
    };
    let diag_query = generate_query(0xDEAD_BEEF, diag_dim);
    let diag_config = match MeasurementConfig::new(20, 20, 0xDEAD_BEEF) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dot_kernel_bench: diagnostic ab skipped: {e}");
            return;
        }
    };
    match run_ab(
        &diag_config,
        || {
            let mut sum = 0f32;
            for chunk in diag_corpus.chunks_exact(diag_dim) {
                sum += dot_wrapper(chunk, &diag_query);
            }
            sum
        },
        || {
            let mut sum = 0f32;
            for chunk in diag_corpus.chunks_exact(diag_dim) {
                sum += dot_scalar_wrapper(chunk, &diag_query);
            }
            sum
        },
    ) {
        Ok(ab) => {
            let ratio = speedup_ratio(
                ab.b.summary.median.as_secs_f64(),
                ab.a.summary.median.as_secs_f64(),
            );
            let class = classify_change(ratio, 0.05);
            println!(
                "dot_kernel: diagnostic_ab dim={diag_dim} rows={diag_rows} simd_vs_scalar_ratio={ratio:.3} class={class:?}"
            );
        }
        Err(e) => {
            eprintln!("dot_kernel_bench: diagnostic ab failed: {e}");
        }
    }
}
