//! HNSW 探索（`engine::hnsw::HnswIndex::search`／`search_masked`）のレイテンシに
//! ついて、受理判定後 prefetch（Issue #490・PR #574。`hnsw.rs::search_layer` の
//! 隣接ループへ hnswlib `searchBaseLayerST` 型のソフトウェアパイプライン先読みを
//! 追加）を導入する前後（before `4d2bd23`／after `eabff3a`）を比較するための
//! 1 規模点計測ベンチ（Issue #491）。
//!
//! # 1 プロセス = 1 規模点
//!
//! `docs/design/benchmark-judgement-policy.md` §5（複数規模点の同一プロセス内
//! 逐次比較は不可。Issue #313 の教訓）に従い、本ベンチは env（`rows` × `dim` ×
//! マスク有無）で選んだ 1 点だけを計測する。8 点（10k／100k・dim 128／768・
//! マスク有無）の前後比較・比率算出・採否判定は、呼び出し元シェルスクリプトが
//! before/after バイナリを交互起動して行う（`make bench-hnsw-search` 参照）。
//!
//! # 索引構築は逐次のみ
//!
//! `HnswIndex::build`（`build_with_threads` ではない）で構築する。逐次構築は
//! 同一シードなら before/after バイナリで完全に同一のグラフになる契約
//! （`hnsw.rs::HnswIndex::build` ドキュメンテーションコメント）ため、探索
//! レイテンシの差分が「同じグラフに対する prefetch の有無」だけに帰属する
//! （並列構築 `build_with_threads(threads>1)` は run ごとにグラフの形状が
//! 変わり得るため before/after を交絡させる。使わない）。
//!
//! # 計測対象コミットの記録（`BENCH_HNSW_SEARCH_COMMIT`）
//!
//! `git archive <commit> | tar -x` で取り出した作業ツリーには `.git` が
//! 含まれないため、`current_commit()` の実行時 `git rev-parse HEAD`
//! フォールバックはビルド元コミットではなく「起動時のカレントディレクトリの
//! HEAD」を返す——before/after バイナリを同じ作業ディレクトリから交互起動
//! すると両方に同一の値が記録され、結果と実装の対応を誤らせる
//! （codex-review 指摘・Issue #491）。この対応関係を保証するため、
//! `BENCH_HNSW_SEARCH_COMMIT=<sha>` をビルド時（`cargo bench --no-run`）に
//! 渡すと `option_env!` でバイナリへ焼き込む方式を追加した。値を渡さず
//! ビルドした場合のみ実行時 `git rev-parse HEAD` へフォールバックする
//! （`.git` があるリポジトリルートから直接 `cargo bench` する通常経路向け）。
//! 出力ヘッダの `commit_source` フィールド（`build_env`／`runtime_git`／
//! `unknown`）でどちらの経路の値かを区別できる。
//!
//! なお rustc は `option_env!` が読む環境変数を自動で再ビルドの
//! フィンガープリントへ追跡する（Rust 1.46 以降。値が変われば
//! 増分ビルドが検知し再コンパイルする）。本ベンチの再現手順（後述）は
//! before/after を別々の `CARGO_TARGET_DIR` でビルドするため、この
//! 自動追跡に依存せずとも対応関係は保たれる。
//!
//! # 参照区間（代表値のみ出力・ノイズ帯はここでは算出しない）
//!
//! 変更（prefetch）を含まない区間として、探索本体と同一のクエリサイクル・
//! 同一シードで brute-force Top-k（`engine::kernel::CpuScalarProvider`）を
//! 計測し、`target=hnsw_search` 行と同型の代表値（`min_us`／`median_us`）を
//! 出力する。ノイズ帯（実測帯）は単一プロセス内では算出しない——プロセス
//! 内の分布にはクエリサイクルに含まれる各クエリ間の所要時間差が混入し、
//! `docs/design/benchmark-judgement-policy.md` §4 が求める run-to-run
//! （プロセス実行間）幅にならないため（codex-review 指摘・Issue #491）。
//! 呼び出し元シェルスクリプトが交互起動した複数プロセスの代表値列を
//! `harness::hnsw_search_latency::reference_band` へ渡してノイズ帯を算出する。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make
//! bench-hnsw-search` からの手動実行専用。`hnsw_build_bench.rs`・
//! `hnsw_compare_bench.rs` と同一方針の defense-in-depth 拒否）。
//!
//! # 測定条件
//!
//! | env | 既定 | 意味 |
//! | --- | --- | --- |
//! | `BENCH_HNSW_SEARCH_ROWS` | 10,000 | コーパス行数（`1..=200,000`） |
//! | `BENCH_HNSW_SEARCH_DIM` | 128 | 次元（`1..=4,096`。768 が本 Issue のもう一方の規模点） |
//! | `BENCH_HNSW_SEARCH_MASK` | `none` | `none` またはマスク可視率 `1..=99`（%）。RLS 事前フィルタ統合〔Issue #409〕の `Subset` 形状を模す |
//! | `BENCH_HNSW_SEARCH_QUERIES` | 200 | クエリ数（`1..=2,000`） |
//! | `BENCH_HNSW_SEARCH_EF` | 64 | `ef_search`（`1..=MAX_EF`） |
//! | `BENCH_HNSW_SEARCH_K` | 10 | Top-k の `k`（`1..=ef`） |
//! | `BENCH_DEDICATED_ENV` | 未設定 | `1` で専有環境自己申告（出力ヘッダへ反映するのみ。挙動は変えない） |
//! | `BENCH_HNSW_SEARCH_COMMIT` | 未設定 | ビルド時（`cargo bench --no-run` 実行時）に渡すと `option_env!` で計測対象コミットとしてバイナリへ焼き込む。`git archive` で取り出した作業ツリー（`.git` を含まない）から before/after 双方をビルドする再現手順ではこの指定が必須——未指定時のフォールバック（実行時 `git rev-parse HEAD`）はカレントディレクトリの HEAD を返すため、同一ディレクトリから交互起動する before/after バイナリに同じ値が記録されてしまう（codex-review 指摘・Issue #491）。出力の `commit_source` フィールド（`build_env`／`runtime_git`／`unknown`）で由来を確認できる |
//!
//! コーパス・クエリは 2 エンジン比較ベンチと同じ理由（内積最大化とコサイン
//! 類似度最大化を一致させ、以後の距離契約を単純化する）で L2 正規化する
//! （`harness::hnsw_compare::l2_normalize_corpus` を再利用）。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::hnsw_compare::l2_normalize_corpus;
#[cfg(feature = "bench-internals")]
use harness::hnsw_search_latency::render_visited_kind_line;
use harness::hnsw_search_latency::{
    generate_corpus, generate_mask, generate_query, parse_dim, parse_ef, parse_k, parse_mask,
    parse_queries, parse_rows, parse_sparse_visited_max, refuse_under_github_actions,
    render_header_line, render_masked_short_line, render_reference_line, render_target_line,
    ArmLabel, MaskSpec,
};
use harness::protocol::{run, MeasurementConfig};

use engine::hnsw::{HnswIndex, HnswParams, HnswSearchScratch};
use engine::isa;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

fn dedicated_env() -> bool {
    std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// 計測対象コミットを決定する。優先順位:
/// 1. ビルド時に `BENCH_HNSW_SEARCH_COMMIT` を渡した場合（`option_env!` で
///    バイナリへ焼き込む。`cargo bench --no-run` 実行時に指定する）。
///    `git archive` で取り出した作業ツリー（`.git` を含まない）から
///    ビルドする再現手順（`docs/design/hnsw-search.md`「再現方法」節）は
///    この経路が前提——同じ作業ディレクトリから before/after バイナリを
///    交互起動しても、各バイナリが自分のビルド時点の値を保持する
///    （codex-review 指摘・Issue #491。実行時 `git rev-parse` はカレント
///    ディレクトリの HEAD を返すため、この用途には使えない）。
/// 2. 実行時 `git rev-parse HEAD`（`.git` があるリポジトリルートから
///    `cargo bench --bench hnsw_search_bench` を直接実行する通常経路向け
///    のフォールバック。1 と異なり「起動時のカレントディレクトリの
///    HEAD」であり、`before`/`after` を区別する保証はない）。
/// 3. いずれも得られない場合は `"unknown"`。
///
/// 戻り値は `(commit, source)`。`source` は `render_header_line` の
/// `commit_source` へそのまま渡し、値の信頼性（1: 明示指定・2: 実行時
/// フォールバック・3: 不明）を出力上区別できるようにする。
fn current_commit() -> (String, &'static str) {
    if let Some(embedded) = option_env!("BENCH_HNSW_SEARCH_COMMIT") {
        let trimmed = embedded.trim();
        if !trimmed.is_empty() {
            return (trimmed.to_string(), "build_env");
        }
    }
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| (s.trim().to_string(), "runtime_git"))
        .unwrap_or_else(|| ("unknown".to_string(), "unknown"))
}

/// クエリ本数の整数倍で protocol 下限（20）以上の最小値
/// （`hnsw_compare_bench.rs::latency_iterations_for` と同型。全クエリが均等に
/// 評価される回数にする）。
fn iterations_for(query_count: usize) -> u32 {
    const MIN_ITERATIONS: usize = 20;
    let n = query_count.max(1);
    let multiples = MIN_ITERATIONS.div_ceil(n).max(1);
    u32::try_from(n.saturating_mul(multiples)).unwrap_or(u32::MAX)
}

fn main() {
    if let Err(e) = refuse_under_github_actions(running_under_github_actions()) {
        eprintln!("hnsw_search_bench: {e}");
        std::process::exit(1);
    }

    let detected = isa::current().isa();
    let env = EnvReport::capture(format!("{detected:?}"));
    println!("{env}");

    let rows = parse_rows(std::env::var("BENCH_HNSW_SEARCH_ROWS").ok().as_deref());
    let dim = parse_dim(std::env::var("BENCH_HNSW_SEARCH_DIM").ok().as_deref());
    let mask_spec = parse_mask(std::env::var("BENCH_HNSW_SEARCH_MASK").ok().as_deref());
    let queries_count = parse_queries(std::env::var("BENCH_HNSW_SEARCH_QUERIES").ok().as_deref());
    let ef = parse_ef(std::env::var("BENCH_HNSW_SEARCH_EF").ok().as_deref());
    let k = parse_k(std::env::var("BENCH_HNSW_SEARCH_K").ok().as_deref(), ef);
    let dedicated = dedicated_env();
    let (commit, commit_source) = current_commit();

    // visited 集合切替閾値の単一ビルド A/B（Issue #498）。knob 未設定
    // （`sparse_visited_max_override == None`）なら本節は一切分岐せず、
    // 以降の探索は既存 `search_masked`（常に dense）と完全に同一のまま進む
    // （既定経路の出力不変を保つ設計。`docs/design/
    // benchmark-judgement-policy.md`「未計測の性能変更を既定にしない」方針）。
    let sparse_visited_max_raw = std::env::var("BENCH_HNSW_SEARCH_SPARSE_VISITED_MAX").ok();
    let sparse_visited_max_override =
        match parse_sparse_visited_max(sparse_visited_max_raw.as_deref(), mask_spec) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("hnsw_search_bench: {e}");
                std::process::exit(1);
            }
        };
    // 2 arm（dense=0／sparse=usize::MAX）のみを扱う（harness::hnsw_search_latency::
    // ArmLabel のドキュメンテーションコメント参照）。中間値は非 vacuous 検証
    // （全計測呼び出しが単一の期待 arm と一致するか）を単純化できないため
    // 受理しない。
    let arm = match sparse_visited_max_override {
        None => None,
        Some(0) => Some(ArmLabel::Dense),
        Some(v) if v == usize::MAX => Some(ArmLabel::Sparse),
        Some(_) => {
            eprintln!(
                "hnsw_search_bench: BENCH_HNSW_SEARCH_SPARSE_VISITED_MAX must be exactly 0 \
                 (dense arm) or {} (sparse arm); intermediate values cannot be verified \
                 non-vacuously against a single expected arm (Issue #498)",
                usize::MAX
            );
            std::process::exit(1);
        }
    };
    // `--features bench-internals` なしのビルドへ knob を渡した場合は拒否する
    // （`HnswSearchScratch::last_visited_kind_is_sparse` が存在せず visited
    // 実装の選択を検証できないため、未検証のまま計測を続けさせない）。
    if arm.is_some() && !cfg!(feature = "bench-internals") {
        eprintln!(
            "hnsw_search_bench: BENCH_HNSW_SEARCH_SPARSE_VISITED_MAX requires \
             `cargo bench --features bench-internals` (or `make bench-hnsw-search-visited`) \
             so the selected visited implementation can be verified non-vacuously \
             (Issue #498)"
        );
        std::process::exit(1);
    }

    let raw_corpus = match generate_corpus(0xC0BA_1234 ^ rows as u64, dim, rows) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_search_bench: corpus generation failed: {e}");
            std::process::exit(1);
        }
    };
    let corpus = match l2_normalize_corpus(&raw_corpus, dim) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_search_bench: corpus normalization failed: {e}");
            std::process::exit(1);
        }
    };

    let mut raw_queries: Vec<Vec<f32>> = Vec::with_capacity(queries_count);
    for i in 0..queries_count {
        raw_queries.push(generate_query(0xC0BA_9999u64.wrapping_add(i as u64), dim));
    }
    let mut queries: Vec<Vec<f32>> = Vec::with_capacity(queries_count);
    for q in &raw_queries {
        match l2_normalize_corpus(q, dim) {
            Ok(v) => queries.push(v),
            Err(e) => {
                eprintln!("hnsw_search_bench: query normalization failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let params = HnswParams::default();
    let build_start = std::time::Instant::now();
    let index = match HnswIndex::build(params, dim as u32, &corpus, 1) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("hnsw_search_bench: HNSW build failed: {e}");
            std::process::exit(1);
        }
    };
    let build_ms = build_start.elapsed().as_secs_f64() * 1e3;

    println!(
        "{}",
        render_header_line(
            rows,
            dim,
            mask_spec,
            ef,
            k,
            queries_count,
            dedicated,
            &commit,
            commit_source,
            build_ms,
        )
    );

    let mask = match mask_spec {
        MaskSpec::None => None,
        MaskSpec::VisiblePercent(p) => Some(generate_mask(0xFEED_0001 ^ rows as u64, rows, p)),
    };

    let iterations = iterations_for(queries.len());
    let config = match MeasurementConfig::new(iterations, iterations, 0xABCD_EF01) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_search_bench: measurement config: {e}");
            std::process::exit(1);
        }
    };

    // HNSW 探索本体（マスク有無を単一の search_masked 呼び出しへ統一する。
    // `None` は `HnswIndex::search` とビット同一な結果を返す契約
    // （`hnsw.rs::search_masked` ドキュメンテーションコメント参照）ため、
    // 分岐を持たずに測定できる）。
    //
    // `protocol::run` は warmup フェーズ（`config.warmup_iterations()` 回）と
    // 計測フェーズを同一クロージャで実行する（呼び出し元からは区別できない）。
    // `short_count` は「計測フェーズで k 未満しか返らなかったクエリ数」という
    // 非 vacuous 性の確認材料であり、warmup 分まで含めると表示値が実際の
    // 計測対象より過大になる（Cursor Bugbot 指摘・Issue #491）ため、
    // `call_index` で呼び出し回数を数え warmup 通過後のみ計上する。
    let mut scratch = HnswSearchScratch::default();
    let mut qi = 0usize;
    let mut short_count = 0usize;
    let mut call_index: u64 = 0;
    let warmup_iterations = u64::from(config.warmup_iterations());
    // 単一マスクを全呼び出し（warmup・計測とも）で使い回すため可視候補数
    // （`visible_count`）はラン全体で一定。Issue #498 の knob（`arm`）が
    // 設定されているときは各呼び出しの選択実装（`--features bench-internals`
    // 限定の `last_visited_kind_is_sparse`）を warmup 分も含め全数観測し、
    // どこかで期待 arm と食い違えば非 vacuous な計測とみなさず後段で
    // fail-closed に拒否する（超集合で数える方が warmup／計測の境界に依存
    // せず単純で取りこぼしがない）。
    #[cfg(feature = "bench-internals")]
    let mut observed_sparse_calls = 0usize;
    #[cfg(feature = "bench-internals")]
    let mut observed_dense_calls = 0usize;
    #[cfg(feature = "bench-internals")]
    let mut unresolved_calls = 0usize;
    let target = run(&config, || {
        let Some(query) = queries.get(qi % queries.len()) else {
            eprintln!(
                "hnsw_search_bench: query index out of bounds (unreachable with non-empty queries)"
            );
            std::process::exit(1);
        };
        qi += 1;
        let is_measured_call = call_index >= warmup_iterations;
        call_index += 1;
        let search_result = match sparse_visited_max_override {
            Some(sparse_visited_max) => index.search_masked_with(
                query,
                k,
                ef,
                mask.as_ref(),
                sparse_visited_max,
                &mut scratch,
            ),
            None => index.search_masked(query, k, ef, mask.as_ref(), &mut scratch),
        };
        #[cfg(feature = "bench-internals")]
        if arm.is_some() {
            match scratch.last_visited_kind_is_sparse() {
                Some(true) => observed_sparse_calls += 1,
                Some(false) => observed_dense_calls += 1,
                None => unresolved_calls += 1,
            }
        }
        match search_result {
            Ok(hits) => {
                if is_measured_call && hits.len() < k {
                    short_count += 1;
                }
                hits.len()
            }
            Err(e) => {
                eprintln!("hnsw_search_bench: search_masked failed: {e}");
                std::process::exit(1);
            }
        }
    });
    let target = match target {
        Ok(m) => m,
        Err(e) => {
            eprintln!("hnsw_search_bench: target measurement: {e}");
            std::process::exit(1);
        }
    };

    let min_us = target
        .samples
        .iter()
        .min()
        .map(|d| d.as_secs_f64() * 1e6)
        .unwrap_or(0.0);
    let p95 = match harness::accept::p95_from_samples(&target.samples) {
        Ok(d) => d.as_secs_f64() * 1e6,
        Err(e) => {
            eprintln!("hnsw_search_bench: p95 computation failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "{}",
        render_target_line(
            min_us,
            target.summary.median.as_secs_f64() * 1e6,
            p95,
            target.samples.len(),
        )
    );
    if matches!(mask_spec, MaskSpec::VisiblePercent(_)) {
        println!("{}", render_masked_short_line(short_count));
    }

    // visited 集合切替閾値の単一ビルド A/B（Issue #498）: `arm` が
    // 設定されているとき（`bench-internals` feature が既に前段で保証済み）、
    // ラン全体（warmup 含む）の全呼び出しが単一の期待 arm と一致したことを
    // 確認してから出力する。1 件でも `unresolved_calls`（早期 return。
    // `k==0`・受理ノードなし等）や逆 arm の観測があれば、意図した visited
    // 実装が実際には選ばれなかった可能性がある未検証計測として fail-closed
    // で拒否する（`docs/design/hnsw-search.md`「Issue #498」節）。
    #[cfg(feature = "bench-internals")]
    if let Some(arm) = arm {
        let sparse_visited_max = arm.sparse_visited_max();
        let visible_count = mask.as_ref().map(|m| m.count_ones()).unwrap_or(0);
        if unresolved_calls > 0 {
            eprintln!(
                "hnsw_search_bench: {unresolved_calls} call(s) returned no visited-kind \
                 observation (early return; likely zero visible candidates under this mask) \
                 — cannot verify arm={} non-vacuously (Issue #498)",
                arm.token()
            );
            std::process::exit(1);
        }
        let (expected_calls, other_calls, other_label) = match arm {
            ArmLabel::Dense => (observed_dense_calls, observed_sparse_calls, "sparse"),
            ArmLabel::Sparse => (observed_sparse_calls, observed_dense_calls, "dense"),
        };
        if other_calls > 0 || expected_calls == 0 {
            eprintln!(
                "hnsw_search_bench: expected every call to select the {} visited set (arm={}) \
                 but observed {other_calls} call(s) select {other_label} instead \
                 ({expected_calls} matched) — vacuous or contradictory measurement (Issue #498)",
                arm.token(),
                arm.token(),
            );
            std::process::exit(1);
        }
        println!(
            "{}",
            render_visited_kind_line(
                arm,
                sparse_visited_max,
                visible_count,
                observed_sparse_calls,
                observed_dense_calls,
                unresolved_calls,
            )
        );
    }

    // 参照区間: 変更（prefetch）を含まない brute-force Top-k。
    let ids: Vec<u64> = (0..rows as u64).collect();
    let brute = CpuScalarProvider;
    let mut ref_qi = 0usize;
    let reference = run(&config, || {
        let Some(query) = queries.get(ref_qi % queries.len()) else {
            eprintln!(
                "hnsw_search_bench: reference query index out of bounds (unreachable with non-empty queries)"
            );
            std::process::exit(1);
        };
        ref_qi += 1;
        match brute.search(SearchInput {
            ids: &ids,
            vectors: &corpus,
            dim: dim as u32,
            query,
            k,
        }) {
            Ok(hits) => hits.len(),
            Err(e) => {
                eprintln!("hnsw_search_bench: brute-force reference search failed: {e}");
                std::process::exit(1);
            }
        }
    });
    let reference = match reference {
        Ok(m) => m,
        Err(e) => {
            eprintln!("hnsw_search_bench: reference measurement: {e}");
            std::process::exit(1);
        }
    };
    let ref_min_us = reference
        .samples
        .iter()
        .min()
        .map(|d| d.as_secs_f64() * 1e6)
        .unwrap_or(0.0);
    let ref_p95 = match harness::accept::p95_from_samples(&reference.samples) {
        Ok(d) => d.as_secs_f64() * 1e6,
        Err(e) => {
            eprintln!("hnsw_search_bench: reference p95 computation failed: {e}");
            std::process::exit(1);
        }
    };
    // ノイズ帯（実測帯）はここでは算出しない。単一プロセス内の分布は
    // クエリサイクルに含まれる各クエリ間の所要時間差を含み run-to-run
    // （プロセス実行間）幅にならない（codex-review 指摘・Issue #491）ため、
    // 交互起動する運用者・シェルが複数プロセス launch の代表値
    // （`min_us`／`median_us`）を集めて `reference_band` へ渡す。
    println!(
        "{}",
        render_reference_line(
            ref_min_us,
            reference.summary.median.as_secs_f64() * 1e6,
            ref_p95,
            reference.samples.len(),
        )
    );
}
