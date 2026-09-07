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
//!
//! # block4 A/B（Issue #512・opt-in）
//!
//! `isa.rs::SimdKernel::dot_block4`（Issue #510・#511。`docs/design/
//! dot-kernel-row-block.md`）の前後比較・採否記録（Issue #512）向けに、
//! `BENCH_DOT_KERNEL_BLOCK_AB=1` を設定したときのみ追加で計測する opt-in
//! セクションを末尾に持つ（未設定・`0` の既定経路の出力・所要時間は不変）。
//! 単一バイナリ内で A（1 行版 `dot` を全行へ適用）・B（4 行組へ
//! `dot_block4` を適用）を [`run_ab`] で交互実行し、`harness::dot_block` の
//! ビット同一検証（fail-closed）を経てから計測する。`docs/design/
//! dot-kernel-multi-accumulator.md`「行間再利用（Issue #512）」節が前後比較の
//! 記録先。

#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::dot_block::{
    check_block_bit_identical, parse_block_ab_env, relative_band, render_block_ab_line,
    render_block_ab_reference_line, BlockAbMode, BLOCK_AB_DIMS,
};
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

/// block4 A/B・A 側（1 行版 `dot` を 4 行分連続で呼ぶ。#510 以前の
/// `search_range` 相当の形）への `#[inline(never)]` 入口。
#[inline(never)]
fn block_ab_single_row_wrapper(rows: [&[f32]; 4], query: &[f32]) -> [f32; 4] {
    let k = isa::current();
    [
        k.dot(rows[0], query),
        k.dot(rows[1], query),
        k.dot(rows[2], query),
        k.dot(rows[3], query),
    ]
}

/// block4 A/B・B 側（`dot_block4`。#510 以降の `search_range` 経路）への
/// `#[inline(never)]` 入口。
#[inline(never)]
fn block_ab_block4_wrapper(rows: [&[f32]; 4], query: &[f32]) -> [f32; 4] {
    isa::current().dot_block4(rows, query)
}

/// 参照区間（1 行版 `dot`。`measure_stage` の `label=current` 計測と同一
/// カーネル）への `#[inline(never)]` 入口。block4 A/B の diff とは独立に、
/// 変更を含まない区間の run 内相対ノイズ帯を算出するために使う。
#[inline(never)]
fn block_ab_reference_wrapper(a: &[f32], b: &[f32]) -> f32 {
    isa::current().dot(a, b)
}

/// block4 A/B の 1 区間（1 working_set × 1 dim）を計測する。事前に全ブロック
/// で `dot_block4` が 1 行版 `dot` とビット同一であることを検証してから
/// [`run_ab`] で交互実行し、A/B の中央値比（`speedup_ratio`）に加え、
/// 参照区間（1 行版 `dot` の反復走査）の run 内相対ノイズ帯（[`relative_band`]
/// 相当。ここでは `run_ab` のサンプル列から直接算出）を返す。
///
/// 契約: ビット不一致が 1 件でもあれば実測せず `Err` を返す（fail-closed。
/// `docs/design/dot-kernel-row-block.md` §4 の契約を計測前に再確認する）。
fn measure_block_ab_stage(working_set: WorkingSet, dim: usize) -> Result<(), String> {
    let rows = rows_for(working_set, dim).map_err(|e| e.to_string())?;
    // 4 行組で扱うためコーパス行数を 4 の倍数へ切り詰める（端数行は本 A/B の
    // 対象外。production `search_range` の端数行縮退経路は既存の
    // `dot_block4_falls_back_to_single_row_dot_when_lengths_are_not_uniform`
    // 等が別途固定済み）。
    let usable_rows = (rows / 4) * 4;
    if usable_rows == 0 {
        return Err(format!(
            "block4_ab working_set={working_set:?} dim={dim}: rows={rows} too small for a 4-row block"
        ));
    }
    let corpus =
        generate_corpus(0xB10C_0000 ^ dim as u64, dim, usable_rows).map_err(|e| e.to_string())?;
    let query = generate_query(0xB10C_0000 ^ dim as u64, dim);

    let repeat = match working_set {
        WorkingSet::CacheResident => CACHE_RESIDENT_REPEAT,
        WorkingSet::ArenaScale => 1,
    };

    // fail-closed ビット同一検証（計測前・1 回のみ）。
    for (block_idx, block) in corpus.chunks_exact(dim * 4).enumerate() {
        let (r0, rest) = block.split_at(dim);
        let (r1, rest) = rest.split_at(dim);
        let (r2, r3) = rest.split_at(dim);
        let actual = block_ab_block4_wrapper([r0, r1, r2, r3], &query);
        let expected = block_ab_single_row_wrapper([r0, r1, r2, r3], &query);
        for lane in 0..4 {
            check_block_bit_identical(dim, block_idx, lane, actual[lane], expected[lane])
                .map_err(|e| e.to_string())?;
        }
    }

    let config = MeasurementConfig::new(20, 50, 0xB10C_0000 ^ dim as u64)
        .map_err(|e| format!("block4_ab dim={dim}: {e}"))?;
    let ab = run_ab(
        &config,
        || {
            let mut sum = [0f32; 4];
            for _ in 0..repeat {
                for block in corpus.chunks_exact(dim * 4) {
                    let (r0, rest) = block.split_at(dim);
                    let (r1, rest) = rest.split_at(dim);
                    let (r2, r3) = rest.split_at(dim);
                    let out = block_ab_single_row_wrapper([r0, r1, r2, r3], &query);
                    for i in 0..4 {
                        sum[i] += out[i];
                    }
                }
            }
            sum
        },
        || {
            let mut sum = [0f32; 4];
            for _ in 0..repeat {
                for block in corpus.chunks_exact(dim * 4) {
                    let (r0, rest) = block.split_at(dim);
                    let (r1, rest) = rest.split_at(dim);
                    let (r2, r3) = rest.split_at(dim);
                    let out = block_ab_block4_wrapper([r0, r1, r2, r3], &query);
                    for i in 0..4 {
                        sum[i] += out[i];
                    }
                }
            }
            sum
        },
    )
    .map_err(|e| format!("block4_ab dim={dim}: {e}"))?;

    let ratio = speedup_ratio(
        ab.a.summary.median.as_secs_f64(),
        ab.b.summary.median.as_secs_f64(),
    );
    let ws_label = match working_set {
        WorkingSet::CacheResident => "cache_resident",
        WorkingSet::ArenaScale => "arena_scale",
    };
    println!(
        "{}",
        render_block_ab_line(
            ws_label,
            dim,
            ab.a.summary.median,
            ab.b.summary.median,
            ratio
        )
    );

    // 参照区間（1 行版 dot の反復走査。block4 A/B の対象外）を同一プロセス内で
    // 追加計測する。A/B 側の 2 クロージャと同じ `repeat` 回のコーパス走査に
    // 揃える（Cursor Bugbot 指摘。以前は `CACHE_RESIDENT_REPEAT` を反映せず
    // cache_resident でもコーパスを 1 周しか走査しておらず、200 回反復する
    // A/B 側とはタイマー粒度・ノイズの取り方が非対称だった）。
    //
    // ここで出力するのは (1) 本プロセス内の反復間ノイズ帯（`band`。情報提供の
    // 参考値に留める）と (2) 本プロセスの参照区間代表値（`ref_median_ms`）の
    // 2 つ。`benchmark-judgement-policy.md` §4 が要求する「変更を含まない
    // 参照区間の run-to-run（プロセス起動間）幅」は (2) を N ≥ 5 プロセス
    // 起動ぶん集めて `relative_band` へ渡すことで初めて算出できる——单一
    // プロセス内の反復間ノイズ帯 (1) はこれとは異なる量であり、算出式の分母も
    // 一致しない（前者はプロセス起動条件のばらつき、後者はキャッシュ・
    // 周波数遷移等プロセス内のばらつきを捉える）。層 A の再現手順（本 ADR
    // 「再現手順（層 A）」節）で 5 回の `ref_median_ms` 行を保存し、doc 側で
    // `relative_band` により run-to-run 幅を再計算する。
    let ref_config = MeasurementConfig::new(20, 50, 0xAEF0_0000_u64.wrapping_add(dim as u64))
        .map_err(|e| format!("block4_ab_ref dim={dim}: {e}"))?;
    let ref_measurement = run(&ref_config, || {
        let mut sum = 0f32;
        for _ in 0..repeat {
            for chunk in corpus.chunks_exact(dim) {
                sum += block_ab_reference_wrapper(chunk, &query);
            }
        }
        sum
    })
    .map_err(|e| format!("block4_ab_ref dim={dim}: {e}"))?;
    let ref_secs: Vec<f64> = ref_measurement
        .samples
        .iter()
        .map(|d| d.as_secs_f64())
        .collect();
    let band = relative_band(&ref_secs).map_err(|e| format!("block4_ab_ref dim={dim}: {e}"))?;
    println!(
        "{}",
        render_block_ab_reference_line(ws_label, dim, ref_measurement.summary.median, band)
    );

    Ok(())
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

    // block4 A/B（Issue #512・opt-in）。`BENCH_DOT_KERNEL_BLOCK_AB` 未設定・`0`
    // では既定経路の出力・所要時間に一切影響しない（fail-closed パース。
    // 非 UTF-8 env は `to_str` の時点で拒否する）。
    let block_ab_raw = std::env::var_os("BENCH_DOT_KERNEL_BLOCK_AB");
    let block_ab_str = match &block_ab_raw {
        None => None,
        Some(v) => match v.to_str() {
            Some(s) => Some(s),
            None => {
                eprintln!("dot_kernel_bench: BENCH_DOT_KERNEL_BLOCK_AB must be valid UTF-8");
                std::process::exit(1);
            }
        },
    };
    let mode = match parse_block_ab_env(block_ab_str) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("dot_kernel_bench: {e}");
            std::process::exit(1);
        }
    };
    if mode == BlockAbMode::On {
        let mut block_ab_had_error = false;
        for &working_set in &WORKING_SETS {
            for &dim in &BLOCK_AB_DIMS {
                if let Err(e) = measure_block_ab_stage(working_set, dim) {
                    eprintln!("dot_kernel_bench: {e}");
                    block_ab_had_error = true;
                }
            }
        }
        if block_ab_had_error {
            std::process::exit(1);
        }
    }
}
