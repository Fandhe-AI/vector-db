//! バッチ高速化の受け入れ基準検証ベンチ（TASK-130。ポインタ: `docs/spec/05-tasks.md`
//! TASK-130・対象ビヘイビア CORE-6, CORE-7〔動的窓〕, CORE-16〔f16 常駐〕。
//! `docs/spec/04-behavior/core-engine.md` ポインタ参照）。
//!
//! `simd_bench.rs`（TASK-127）と同じ設計方針を踏襲する: `make ci` には含めず
//! `.github/workflows/bench.yml`（workflow_dispatch）から実行する時間依存ベンチであり、
//! 閾値は環境変数（Actions secrets）から注入・未設定は fail-closed で非ゼロ終了する。
//! 標準出力には pass/fail と非数値状態のみを書き、実測値・注入された閾値は出力しない
//! （`.claude/rules/spec-confidentiality.md`・Issue #279）。実測値が必要な場合のみ
//! `BENCH_VERBOSE`（非空で有効）の opt-in で追加出力するが、`GITHUB_ACTIONS` 下では
//! public ログへの漏えいを防ぐため fail-closed で拒否する（PR #224 で CORE-5 側
//! 〔`contrast_bench.rs`〕に適用済みの「真偽値のみを既定出力にする」方針を本ファイルへ
//! 横展開したもの）。
//!
//! - CORE-7（動的窓の劣化上限・アクティブなゲート。Issue #302 で測定方式を再整合）:
//!   [`run_core7_gate`] が測定する。設計の要点（詳細は `docs/design/
//!   core7-dynamic-window-gate.md` の ADR を参照。数値は書かない）:
//!   - A（対照）・B（被検）とも **同一カーネル**（`BatchEngine::batch_search`。f16
//!     常駐・CPU-SIMD）を経由する。A は事前生成済みクエリ 1 本を直接
//!     `batch_search` へ渡す経路、B は同じクエリを [`DynamicWindowAggregator`]
//!     の `push`/`drain` に通してから同じ `batch_search` を呼ぶ経路とし、差分を
//!     「窓の push/drain・所有権移動・dispatch 相当の分岐」のみに絞る。単発
//!     クエリは実運用では動的窓に入らないため（`should_aggregate_into_batch`
//!     が `false` を返す文脈）、B は「窓を通った場合に単発クエリが払いうる
//!     最大オーバーヘッド」を課す保守側の構成である。
//!   - `CORE6`/`CORE-16` と同じ合成データセット（[`build_gate_dataset`]）を
//!     再利用し、複数試行（[`CORE7_TRIALS`]）を行う。A/B のクエリは反復ごとに
//!     同一内容を複製して使い（`batch_search` の類似度計算コストがクエリ値へ
//!     左右されるぶんをノイズ源から除く。Issue #302 レビュー対応）、試行内の
//!     劣化率（%）は CORE-7 が定義する量そのまま、**経路ごとに独立算出した
//!     p95 の差分**（`degradation_pct(p95_from_samples(a), p95_from_samples(b))`）
//!     で算出する。試行間はその値の列の**中央値**を
//!     `BENCH_BATCH_MAX_DEGRADATION_PCT` と比較する（突発的な計測スパイクが
//!     単一試行だけを外れ値化しても誤 fail しないための試行間ノイズ対策。
//!     Issue #302 codex-review 対応）。
//!
//!     反復ペアの絶対差分 `b_i - a_i` の分布から p95 を取る「ペア化差分」方式
//!     （旧実装）は一度採用したが、2 回のレビューで撤回した（Issue #302
//!     Cursor Bugbot・codex-review 双方の指摘）。ペア化差分方式が構造的に
//!     抱える欠陥: `run_ab` は同一反復番号の `a_i`/`b_i` を直後に連続実行する
//!     だけであり厳密な同時計測ではないため、`delta_i = b_i - a_i` は A/B が
//!     完全に同一分布でも平均 0・分散非 0 の分布になる。その**分布の p95**
//!     （＝ 0 を中心とする対称分布の上側裾）は退行の有無に関わらず構造的に
//!     正の値を取り続けるため、A/B の分布が完全に一致していても偽陽性を生む
//!     （codex-review 指摘。旧実装が置いていた合成テストは注入ノイズ幅を
//!     閾値未満に固定していただけで、この構造的バイアスを検証できていな
//!     かった）。経路別に独立算出した p95 の差分（本方式）は A が速くも遅くも
//!     なりうる対称な統計量であり、この構造的バイアスを持たない。
//!
//!     本方式は「軽微な push/drain 退行が全走査コストの反復間ノイズへ埋もれ
//!     判別力を失う」弱点を持つ（ペア化を検討した動機そのもの）。この弱点は
//!     ペア化ではなく試行数（[`CORE7_TRIALS`]）・反復数（`run_core7_gate` が
//!     `MeasurementConfig::new` へ渡す測定反復回数）を増やし分位点推定の
//!     ノイズを下げることで緩和する対象とし、推定量自体をペア化差分へ戻さない。
//!     詳細は ADR「本ゲートの感度の限界」節参照。
//!   - ワークロードの戻り値（B の `drain()` 結果等）は計測区間内で drop すると
//!     解放コストが測定対象へ混入する（`harness::ab::run_ab` のドキュメンテー
//!     ションコメント参照）ため、各試行内で sink へ退避し `run_ab` 完了後に
//!     まとめて drop する。
//!   - 旧来の「256 本まとめて push/drain するだけの経路」比較は判別力を保つ
//!     診断として [`run_dynamic_window_push_drain_diagnostic`] に残すが、
//!     合否には数えない（`BatchEngine::batch_search` を経由しないため CORE-7
//!     の定義量そのものではない）。この旧来比較が `batch_search` を測定区間から
//!     除外していたのは PR #154 のレビュー対応（全走査コストの前で push/drain の
//!     差分が埋もれ判別力を失う、という指摘）によるものだった。本 Issue（#302）は
//!     その判断を誤りとして覆すのではなく、「判別力優先の比較」と「CORE-7 が
//!     定義する量」は両立しない設計だったと整理し、前者を診断へ、後者
//!     （[`run_core7_gate`]）をゲート本体へ切り分けて両方を保持する（詳細は
//!     ADR「PR #154 の判別力優先レビュー対応との関係」節）。
//! - CORE-6（GPU 経路 vs CPU-SIMD の p95 短縮率）・CORE-16（f16 常駐 vs f32 常駐の
//!   p95 短縮率）: `BENCH_CORE6`/`BENCH_CORE16` フラグ（opt-in。未設定・空文字のみ
//!   「対象外」とし、非空値はすべて opt-in 要求とみなす）で有効化する実測ゲート
//!   （Issue #178 で実 GPU バックエンド〔`engine::gpu_batch`〕が接続されたため、
//!   従来の「未実装のため常に pass=false」を実測経路へ置き換えた）。
//!   - CORE-6: 対照 A = CPU-SIMD バッチ経路（`BatchEngine::batch_search`。f16 常駐
//!     行列を CPU の実行時 SIMD 検出カーネルで全走査）、被検 B = GPU バックエンド
//!     （`FallbackBatchEngine::build_with_gpu`）。閾値は
//!     `BENCH_CORE6_MIN_IMPROVEMENT_PCT` から注入し未設定は fail-closed。
//!     GPU が初期化できない環境（CI の GitHub ホステッド runner 等）で opt-in された
//!     場合は「判定不能」を `pass=false` として報告する（CPU 経路同士の比較値を
//!     GPU 実測の代替として計上しない＝アサーション弱体化を避ける）。計測中に
//!     CPU 縮退（CORE-8）が起きた場合も同様に `pass=false`（縮退後の値は GPU 経路の
//!     実測ではないため）。
//!   - CORE-16（Issue #234）: **GPU 常駐コピーの f16 パック vs f32 常駐**の p95
//!     短縮率。対照 A = f32 常駐対照経路（`engine::gpu_batch::
//!     GpuF32ContrastBackend`。GPU 側で元の f32 ベクトル列をそのまま常駐させ
//!     `unpack2x16float` を経由しない内積を計算する bench/テスト専用経路）、
//!     被検 B = 本番の f16 パック常駐 GPU 経路（`engine::gpu_batch::
//!     GpuBatchBackend`）。閾値は `BENCH_CORE16_MIN_IMPROVEMENT_PCT` から注入し
//!     未設定は fail-closed。どちらかの GPU 初期化が失敗した環境（CI の
//!     GitHub ホステッド runner 等）で opt-in された場合は「判定不能」を
//!     `pass=false` として報告する（CPU 経路同士の f16/f32 比較を代替として
//!     計上しない＝アサーション弱体化を避ける。CORE-6 と同方針）。
//!
//!
//!   いずれも既定では pass/fail・非数値状態のみを標準出力へ書き、実測値・注入した
//!   閾値は出力しない（`BENCH_VERBOSE` opt-in 時のみ実測値を追加出力する）。

// `harness` の取り込み方針は `simd_bench.rs` と同一(本ファイルが実際に使う項目
// のみで、未到達の `pub` 項目は `dead_code` 警告になりうるためモジュール全体を許容する)。
#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{
    check_degradation_pct_within_limit, check_improvement_at_least, degradation_pct,
    median_degradation_pct, p95_from_samples,
};
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;

use engine::batch_fallback::BatchBackend;
use engine::batch_search::{BatchEngine, BatchQuery, DynamicWindowAggregator, ResidentMatrix};
use engine::policy::PolicyContext;
use engine::storage::Visibility;

/// CORE-7 の診断（[`run_dynamic_window_push_drain_diagnostic`]）で 1 反復あたり
/// 集約器へ通すクエリ本数。1 本単位では `push`/`drain` の所要時間が `Instant`
/// の分解能・関数呼び出しオーバーヘッドへ埋もれるため、実運用の動的窓サイズ
/// 相当の本数へ増幅してから測る（値そのものは spec の閾値ではなく本ベンチ
/// 固有の測定条件。実測の時間スケールは書かない——
/// `.claude/rules/spec-confidentiality.md`）。
const AGG_BATCH_SIZE: usize = 256;

/// 診断で集約するクエリの次元数。
const AGG_QUERY_DIM: usize = 768;

/// CORE-7 ゲート（[`run_core7_gate`]）の試行回数（Issue #302）。hosted runner での
/// 突発的な単発スパイクが 1 試行だけを外れ値化しても、中央値採用により判定全体を
/// 誤 fail させないための本ベンチ固有の測定条件（spec の閾値ではない）。中央値が
/// 意味を持つには外れ値以外の「クリーンな」試行が過半数必要であり、2 コア共有の
/// hosted runner はローカル専有環境よりスパイク頻度が高いと想定されるため、
/// ローカル実測（`docs/design/core7-dynamic-window-gate.md` 参照）で確認できた
/// 安定性より余裕を持たせた値を採る。
const CORE7_TRIALS: usize = 9;

/// CORE-6/CORE-16/CORE-7 ゲートの実測に使う合成データセット規模（本ベンチ固有の測定条件で
/// あり spec の閾値ではない）。GPU 転送・dispatch の固定コストを償却できる程度の
/// 行数・次元にしつつ、GPU 非搭載環境でも CORE-16 側（CPU 上の f16/f32 比較）が
/// 現実的な時間で完走する規模に留める。
const GPU_GATE_ROW_COUNT: usize = 20_000;
const GPU_GATE_DIM: usize = 256;
const GPU_GATE_TOP_K: usize = 10;
/// 1 反復あたりのバッチ本数（GPU 経路はクエリ単位に dispatch するため、1 本だと
/// 転送・同期の固定コストが支配的になり経路差が現れない）。CORE-6/CORE-16 のみが
/// 使う（CORE-7 は単発クエリの p95 を測るため 1 本固定）。
const GPU_GATE_BATCH_SIZE: usize = 8;

/// CORE-7 ゲートの計測に使うテナント ID（本ベンチ専用の合成データ。実データではない）。
const BENCH_TENANT: &str = "task-130-bench-tenant";

/// `BENCH_BATCH_MAX_DEGRADATION_PCT` 環境変数（パーセント・浮動小数点）を読み取り、
/// CORE-7 の劣化率上限として使う値を得る。未設定・非数値・負値は fail-closed で
/// 判定不能として扱う(数値そのものは spec が SSOT。本ファイルにはデフォルト値を
/// 持たない——`.claude/rules/spec-confidentiality.md`)。
fn max_degradation_pct_from_env() -> Result<f64, String> {
    let raw = std::env::var("BENCH_BATCH_MAX_DEGRADATION_PCT").map_err(|_| {
        "BENCH_BATCH_MAX_DEGRADATION_PCT is not set (see .github/workflows/bench.yml vars)"
            .to_string()
    })?;
    let value: f64 = raw.trim().parse().map_err(|_| {
        "BENCH_BATCH_MAX_DEGRADATION_PCT must be a floating-point number".to_string()
    })?;
    if !value.is_finite() || value < 0.0 {
        return Err(
            "BENCH_BATCH_MAX_DEGRADATION_PCT must be a finite, non-negative value".to_string(),
        );
    }
    Ok(value)
}

/// `BENCH_CORE6`/`BENCH_CORE16` 環境変数を読み取り、実 GPU 未接続の判定不能ゲートを
/// opt-in で有効化するかを返す。値が未設定・空文字のときのみ「対象外」（`false`）とし、
/// それ以外の非空値はすべて opt-in 要求とみなす（レビュー指摘対応: 以前は `"1"` 完全
/// 一致のみを有効とみなしており、`"true"`/`"yes"` 等の non-"1" な truthy 値を設定しても
/// サイレントに「未設定」と同じ fail-open 側の「対象外」経路に落ちていた。opt-in
/// ゲートの趣旨は「明示的な値が設定されていれば fail-closed 側で判定する」ことなので、
/// 値の有無で判定し内容の解釈は行わない）。
fn opt_in_requested_from_env(var: &str) -> bool {
    std::env::var(var)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// `BENCH_VERBOSE` 環境変数（非空で有効。[`opt_in_requested_from_env`] と同じ
/// 「値の有無のみで判定し内容は解釈しない」規約）を読み取り、実測値（p95・median）を
/// 標準出力へ追加するかを返す。実測値は public リポの Actions ログへ出すと注入済みの
/// 非公開閾値を逆算されうるため（Issue #279）、`GITHUB_ACTIONS` が設定された実行環境
/// （GitHub Actions ランナーは常にこの変数を設定する）では opt-in 自体を拒否する
/// （fail-closed。`.github/workflows/bench.yml` は `BENCH_VERBOSE` を注入しない運用と
/// 二重化することで、誤注入時にも public ログへ実測値が漏れないようにする）。
fn verbose_requested_from_env() -> Result<bool, String> {
    let requested = opt_in_requested_from_env("BENCH_VERBOSE");
    if requested && std::env::var("GITHUB_ACTIONS").is_ok() {
        return Err(
            "BENCH_VERBOSE is refused under GitHub Actions (public log; Issue #279)".to_string(),
        );
    }
    Ok(requested)
}

/// `BENCH_CORE6_MIN_IMPROVEMENT_PCT` を読み取り、
/// p95 短縮率の下限として使う値を得る。opt-in されているのに閾値が未設定・非数値・
/// 非正値の場合は「判定不能」として fail-closed に `Err` を返す
/// （[`max_degradation_pct_from_env`] と同一方針。数値そのものは spec が SSOT であり
/// 本ファイルにデフォルト値を持たない——`.claude/rules/spec-confidentiality.md`）。
fn min_improvement_pct_from_env(var: &str) -> Result<f64, String> {
    let raw = std::env::var(var)
        .map_err(|_| format!("{var} is not set (see .github/workflows/bench.yml vars)"))?;
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{var} must be a floating-point number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("{var} must be a finite, positive value"));
    }
    Ok(value)
}

/// CORE-6/CORE-16/CORE-7 ゲート用の合成データセット（本ベンチ専用。実データではない）。
/// 単一テナント・全 `Public` の素直な配置にし、テナントマスクの分岐差が経路間の
/// 比較へ混ざらないようにする。
struct GateDataset {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    vectors: Vec<f32>,
    queries: Vec<Vec<f32>>,
}

fn build_gate_dataset(rng: &mut DeterministicRng) -> GateDataset {
    let ids: Vec<u64> = (0..GPU_GATE_ROW_COUNT as u64).collect();
    let tenant_ids: Vec<String> =
        std::iter::repeat_n(BENCH_TENANT.to_string(), GPU_GATE_ROW_COUNT).collect();
    let visibilities: Vec<Visibility> =
        std::iter::repeat_n(Visibility::Public, GPU_GATE_ROW_COUNT).collect();
    let mut vectors = Vec::with_capacity(GPU_GATE_ROW_COUNT * GPU_GATE_DIM);
    for _ in 0..GPU_GATE_ROW_COUNT {
        vectors.extend(rng.next_vector(GPU_GATE_DIM));
    }
    let queries: Vec<Vec<f32>> = (0..GPU_GATE_BATCH_SIZE)
        .map(|_| rng.next_vector(GPU_GATE_DIM))
        .collect();
    GateDataset {
        ids,
        tenant_ids,
        visibilities,
        vectors,
        queries,
    }
}

/// CORE-8 の縮退イベント件数だけを数える observer（GPU ゲートの測定妥当性判定用）。
/// 構築時・計測中に 1 件でも縮退が起きていれば、その測定値は GPU 経路のものでは
/// ないため「判定不能（`pass=false`）」に倒す。
struct CountingObserver(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl engine::batch_fallback::FallbackObserver for CountingObserver {
    fn on_fallback(&self, event: engine::batch_fallback::FallbackEvent) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        eprintln!("batch_bench: fallback observed during gpu gate: {event}");
    }
}

/// `run_ab` の 1 反復分として与えるバッチクエリ列を組み立てる（測定区間外での
/// 参照の組み立てはできないため、各ワークロード内で `BatchQuery` を作る。
/// クエリベクトル本体は事前生成済みで確保・コピーは発生しない）。
fn gate_batch_queries<'a>(queries: &'a [Vec<f32>], ctx: &'a PolicyContext) -> Vec<BatchQuery<'a>> {
    queries
        .iter()
        .map(|q| BatchQuery {
            vector: q.as_slice(),
            k: GPU_GATE_TOP_K,
            ctx,
        })
        .collect()
}

/// CORE-7 ゲート（動的窓集約を経由する単発クエリ経路の p95 劣化率上限。Issue #302 で
/// 測定方式を再整合。設計の詳細は `docs/design/core7-dynamic-window-gate.md` の
/// ADR とモジュール冒頭コメントを参照）。
///
/// A（対照）・B（被検）とも `dataset`/`ctx` から構築した同一の CPU-SIMD
/// `BatchEngine` を経由する。差分は「窓の push/drain・所有権移動」のみに絞り、
/// `CORE7_TRIALS` 回の試行から劣化率（%）の中央値を算出して
/// `max_degradation_pct` と突き合わせる。`batch_search` が 1 件でもエラーを
/// 返した場合は判定不能として `pass=false`（CORE-6/CORE-16 と同一の fail-closed
/// 方針。エラー経路は通常大幅に軽量なため、これを計測サンプルへ計上すると
/// 誤って劣化なしと判定しうる）。
fn run_core7_gate(
    dataset: &GateDataset,
    ctx: &PolicyContext,
    max_degradation_pct: f64,
    verbose: bool,
) -> Result<bool, String> {
    const LABEL: &str = "dynamic_window_degradation";

    let matrix = ResidentMatrix::build(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
    )
    .map_err(|err| format!("{LABEL}: resident matrix build failed: {err}"))?;
    let engine = BatchEngine::new(matrix);

    let error_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut trial_pcts: Vec<f64> = Vec::with_capacity(CORE7_TRIALS);

    for trial in 0..CORE7_TRIALS {
        let seed = (trial as u64)
            .checked_add(1)
            .expect("CORE7_TRIALS is a small constant far below u64::MAX");
        let config = MeasurementConfig::new(20, 50, seed).expect("protocol minimums satisfied");
        let total_iterations = (config.warmup_iterations() as usize)
            .checked_add(config.measured_iterations() as usize)
            .expect("MeasurementConfig::new bounds iteration counts within usize");

        // pool の対称化・クエリ内容の一致（Issue #302 レビュー対応）: A/B で
        // 使うクエリは反復ごとに 1 本だけ生成し、同一内容を複製して両プールへ
        // 積む（`build_query_pool_pair` の診断側と同一方針）。別々の RNG 引きで
        // 生成すると A/B のクエリ内容が異なり、`batch_search` の類似度計算・
        // 上位 k 候補更新コストがクエリ値に左右されるぶんが測定対象（動的窓
        // 集約の push/drain オーバーヘッド）より大きいノイズ・系統差になりうる。
        // heap 配置も「1 つのループで交互に確保」する（順次確保〔pool_a を
        // 全確保したのち pool_b を全確保〕だと heap 配置がテスト間で非対称に
        // なりうる。モジュール冒頭コメント・ADR 参照）。
        let mut trial_rng = DeterministicRng::new(seed);
        let mut pool_a: Vec<Vec<f32>> = Vec::with_capacity(total_iterations);
        let mut pool_b: Vec<Vec<f32>> = Vec::with_capacity(total_iterations);
        for _ in 0..total_iterations {
            let query = trial_rng.next_vector(GPU_GATE_DIM);
            pool_a.push(query.clone());
            pool_b.push(query);
        }

        // 解放コストの測定区間外化（`harness::ab::run_ab` の drop 契約参照）。
        // A は消費したクエリ（`Vec<f32>`）を、B は `drain()` の戻り値
        // （`Vec<Vec<f32>>`）を測定完了後まとめて drop する。
        let mut sink_a: Vec<Vec<f32>> = Vec::with_capacity(total_iterations);
        let mut sink_b: Vec<Vec<Vec<f32>>> = Vec::with_capacity(total_iterations);

        let error_count_a = std::sync::Arc::clone(&error_count);
        let error_count_b = std::sync::Arc::clone(&error_count);

        let workload_a = || -> usize {
            let query = pool_a
                .pop()
                .expect("pool sized to warmup + measured iteration count");
            let batch_queries = [BatchQuery {
                vector: query.as_slice(),
                k: GPU_GATE_TOP_K,
                ctx,
            }];
            let count = match engine.batch_search(&batch_queries) {
                Ok(hits) => hits.len(),
                Err(err) => {
                    error_count_a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    eprintln!(
                        "batch_bench: {LABEL}: direct batch_search returned an error \
                         during measurement: {err}"
                    );
                    0
                }
            };
            sink_a.push(query);
            count
        };

        let workload_b = || -> usize {
            let query = pool_b
                .pop()
                .expect("pool sized to warmup + measured iteration count");
            let mut window = DynamicWindowAggregator::new();
            window
                .push(query)
                .expect("well-formed synthetic query must satisfy window limits");
            let drained = window.drain();
            let count = match drained.first() {
                Some(first) => {
                    let batch_queries = [BatchQuery {
                        vector: first.as_slice(),
                        k: GPU_GATE_TOP_K,
                        ctx,
                    }];
                    match engine.batch_search(&batch_queries) {
                        Ok(hits) => hits.len(),
                        Err(err) => {
                            error_count_b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            eprintln!(
                                "batch_bench: {LABEL}: windowed batch_search returned an \
                                 error during measurement: {err}"
                            );
                            0
                        }
                    }
                }
                None => {
                    // 直前に必ず 1 件 push しているため到達しないはずだが、
                    // untrusted な状態遷移を仮定せず fail-closed に倒す。
                    error_count_b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    eprintln!(
                        "batch_bench: {LABEL}: drained window unexpectedly empty during \
                         measurement"
                    );
                    0
                }
            };
            sink_b.push(drained);
            count
        };

        let ab = run_ab(&config, workload_a, workload_b)
            .map_err(|err| format!("{LABEL}: A/B measurement failed (trial {trial}): {err}"))?;

        if error_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            println!(
                "{LABEL}: not measurable (batch_search returned an error during \
                 measurement; see stderr) rows={GPU_GATE_ROW_COUNT} dim={GPU_GATE_DIM} \
                 k={GPU_GATE_TOP_K} trials={CORE7_TRIALS} pass=false"
            );
            return Ok(false);
        }

        // CORE-7 が定義する量（B の p95 と A の p95 の差）をそのまま算出する
        // （Issue #302 codex-review 指摘対応）。ペア化差分
        // `paired_p95_degradation_pct`（反復ごとの絶対差分 `b_i - a_i` の
        // 分布から p95 を取る旧実装）は、`run_ab` が同一反復番号の `a_i`/`b_i`
        // を直後に連続実行するだけで厳密な同時計測ではないため、A/B が完全に
        // 同一分布でも `delta_i = b_i - a_i` は平均 0・分散非 0 の分布になり、
        // その**分布の p95**（0 を中心とする対称分布の上側裾）は退行の有無に
        // 関わらず構造的に正の値を取り続け偽陽性を生む（codex-review 指摘。
        // 旧実装の合成テストは注入ノイズ幅を閾値未満に固定していただけで、この
        // 構造的バイアスを検証できていなかった）。経路ごとに独立算出した p95 の
        // 差分（[`degradation_pct`]）は A が速くも遅くもなりうる対称な統計量で
        // あり、この構造的バイアスを持たない（`docs/design/
        // core7-dynamic-window-gate.md` 参照）。
        //
        // 本方式は「軽微な push/drain 退行が全走査コストの反復間ノイズへ埋もれ
        // 判別力を失う」弱点を持つ（ペア化を検討した動機そのもの）。この弱点は
        // ペア化ではなく試行数・反復数（分位点推定のノイズを下げる）で緩和する
        // 対象とする。
        //
        // 試行間のスパイク耐性は次段（`trial_pcts` の `median_degradation_pct`）が
        // 別途担う（単一試行内のばらつきと複数試行間の突発スパイクは別種のノイズ
        // であり、試行内側まで中央値化すると契約が定める p95 劣化そのものを
        // 隠してしまうため、試行内は p95 のまま・試行間だけ中央値を使う設計は
        // 維持する）。
        let baseline_p95 = p95_from_samples(&ab.a.samples)
            .map_err(|err| format!("{LABEL}: baseline p95 failed (trial {trial}): {err}"))?;
        let candidate_p95 = p95_from_samples(&ab.b.samples)
            .map_err(|err| format!("{LABEL}: candidate p95 failed (trial {trial}): {err}"))?;
        let pct = degradation_pct(baseline_p95, candidate_p95)
            .map_err(|err| format!("{LABEL}: p95 degradation failed (trial {trial}): {err}"))?;
        trial_pcts.push(pct);
    }

    let median_pct = median_degradation_pct(&trial_pcts)
        .map_err(|err| format!("{LABEL}: median_degradation_pct failed: {err}"))?;
    let pass = check_degradation_pct_within_limit(median_pct, max_degradation_pct)
        .map_err(|err| format!("{LABEL}: degradation check failed: {err}"))?;

    // limit（BENCH_BATCH_MAX_DEGRADATION_PCT）・実測値（試行別劣化率・中央値）は
    // 意図的にログへ出力しない（閾値は spec が SSOT・実測値は public リポの
    // Actions ログから閾値を逆算されうるため出さない。Issue #279 参照）。
    println!(
        "{LABEL}: rows={GPU_GATE_ROW_COUNT} dim={GPU_GATE_DIM} k={GPU_GATE_TOP_K} \
         trials={CORE7_TRIALS} pass={pass}"
    );
    if verbose {
        println!(
            "verbose({LABEL}): trial_degradation_pct={trial_pcts:?} median_pct={median_pct:?}"
        );
    }
    Ok(pass)
}

/// CORE-6 ゲート（GPU 経路 vs CPU-SIMD 経路の p95 短縮率）。
/// opt-in されていなければ「対象外」を出力して合否に数えない（silent skip にしない）。
/// opt-in 時に GPU が初期化できない・計測中に CPU 縮退した場合は `pass=false`
/// （fail-closed。CPU 同士の比較値を GPU 実測の代替として計上しない）。
fn run_core6_gate(
    dataset: &GateDataset,
    ctx: &PolicyContext,
    verbose: bool,
) -> Result<bool, String> {
    const LABEL: &str = "gpu_vs_cpu_simd_p95";
    if !opt_in_requested_from_env("BENCH_CORE6") {
        println!(
            "{LABEL}: out of scope for this run (not counted toward pass/fail; \
             set BENCH_CORE6=1 with BENCH_CORE6_MIN_IMPROVEMENT_PCT to enable) requested=false"
        );
        return Ok(true);
    }
    let min_improvement_pct = min_improvement_pct_from_env("BENCH_CORE6_MIN_IMPROVEMENT_PCT")?;

    let fallback_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gpu_engine = engine::batch_fallback::FallbackBatchEngine::build_with_gpu(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
        Box::new(CountingObserver(std::sync::Arc::clone(&fallback_count))),
    )
    .map_err(|err| format!("{LABEL}: gpu-backed engine build failed: {err}"))?;
    if fallback_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        println!(
            "{LABEL}: not measurable in this environment (gpu backend unavailable; \
             cpu fallback engaged) requested=true pass=false"
        );
        return Ok(false);
    }

    let cpu_matrix = ResidentMatrix::build(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
    )
    .map_err(|err| format!("{LABEL}: resident matrix build failed: {err}"))?;
    let cpu_engine = BatchEngine::new(cpu_matrix);

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let workload_a = || {
        let queries = gate_batch_queries(&dataset.queries, ctx);
        cpu_engine.batch_search(&queries).map(|hits| hits.len())
    };
    let workload_b = || {
        let queries = gate_batch_queries(&dataset.queries, ctx);
        gpu_engine.batch_search(&queries).map(|hits| hits.len())
    };
    let ab = run_ab(&config, workload_a, workload_b)
        .map_err(|err| format!("{LABEL}: A/B measurement failed: {err}"))?;
    if fallback_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        println!(
            "{LABEL}: not measurable in this environment (cpu fallback engaged during \
             measurement) requested=true pass=false"
        );
        return Ok(false);
    }

    let p95_a = p95_from_samples(&ab.a.samples)
        .map_err(|err| format!("{LABEL}: p95 of A samples unavailable: {err}"))?;
    let p95_b = p95_from_samples(&ab.b.samples)
        .map_err(|err| format!("{LABEL}: p95 of B samples unavailable: {err}"))?;
    let pass = check_improvement_at_least(p95_a, p95_b, min_improvement_pct)
        .map_err(|err| format!("{LABEL}: improvement check failed: {err}"))?;
    println!(
        "{LABEL}: rows={GPU_GATE_ROW_COUNT} dim={GPU_GATE_DIM} batch={GPU_GATE_BATCH_SIZE} \
         requested=true pass={pass}"
    );
    if verbose {
        println!("verbose({LABEL}): cpu_simd_p95={p95_a:?} gpu_p95={p95_b:?}");
    }
    Ok(pass)
}

/// CORE-16 ゲート（GPU 常駐コピーの f16 パック vs f32 常駐の p95 短縮率。
/// Issue #234）。
///
/// 本 ID の A/B は **GPU バッチ経路上**の常駐形式比較であり（ポインタ:
/// `docs/spec/04-behavior/core-engine.md` CORE-16。CPU-SIMD 経路への f16 適用は
/// 本 ID の対象外）、対照 A = `engine::gpu_batch::GpuF32ContrastBackend`
/// （f32 常駐。bench/テスト専用の対照経路）、被検 B = 本番の f16 パック常駐
/// `engine::gpu_batch::GpuBatchBackend` を**どちらも GPU 経路で直接構築**して
/// 比較する（`FallbackBatchEngine` は経由しない。CPU 縮退が発生すると GPU 経路
/// 同士の比較にならないため）。opt-in されていなければ「対象外」を出力して
/// 合否に数えない（silent skip にしない）。opt-in 時にどちらかの GPU 初期化が
/// 失敗した場合は `pass=false`（fail-closed。CPU 比較を GPU 実測の代替として
/// 計上しない＝アサーション弱体化を避ける。CORE-6 と同方針）。
fn run_core16_gate(
    dataset: &GateDataset,
    ctx: &PolicyContext,
    verbose: bool,
) -> Result<bool, String> {
    const LABEL: &str = "f16_resident_vs_f32_resident_p95";
    if !opt_in_requested_from_env("BENCH_CORE16") {
        println!(
            "{LABEL}: out of scope for this run (not counted toward pass/fail; \
             set BENCH_CORE16=1 with BENCH_CORE16_MIN_IMPROVEMENT_PCT to enable) requested=false"
        );
        return Ok(true);
    }
    let min_improvement_pct = min_improvement_pct_from_env("BENCH_CORE16_MIN_IMPROVEMENT_PCT")?;

    let f32_backend = match engine::gpu_batch::GpuF32ContrastBackend::try_new(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
    ) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "{LABEL}: not measurable in this environment (f32-resident gpu contrast \
                 backend unavailable: {e}) requested=true pass=false"
            );
            return Ok(false);
        }
    };
    let f16_matrix = ResidentMatrix::build(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
    )
    .map_err(|err| format!("{LABEL}: resident matrix build failed: {err}"))?;
    let f16_backend = match engine::gpu_batch::GpuBatchBackend::try_new(f16_matrix) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "{LABEL}: not measurable in this environment (f16-resident gpu backend \
                 unavailable: {e}) requested=true pass=false"
            );
            return Ok(false);
        }
    };

    // レビュー指摘対応（PR #245）: `gate_batch_queries` の `Vec<BatchQuery>` 確保・
    // 構築は測定区間の外（`run_ab` 呼び出し前）で 1 回だけ行い、両ワークロードから
    // 同じ参照を再利用する。`BatchQuery` はクエリ本体・ctx への借用のみを保持し
    // `batch_search` も `&[BatchQuery]` を読み取るだけで消費しないため、反復間で
    // 使い回しても計測対象（f16/f32 常駐形式の差）を歪めない。以前は各反復の
    // 測定区間内で構築しており、その共通コストが f16/f32 間の短縮率を薄めていた
    // （CORE-7 と同じ「入力生成・確保は測定区間外」契約に反していた）。
    let queries = gate_batch_queries(&dataset.queries, ctx);

    // レビュー指摘対応（PR #245・cursor[bot]）: `batch_search` の `Result` を
    // 検査せず捨てていたため、`try_new` 成功後にランタイムで GPU 失敗が起きても
    // エラー経路（通常大幅に軽量）がそのまま計測サンプルへ計上され、
    // `check_improvement_at_least` が誤って `pass=true` を返しうる状態だった
    // （CORE-6 の縮退カウントに相当する fail-closed チェックが本ゲートに無かった）。
    // 両ワークロードのエラー発生を計測中も観測し、1 件でもあれば「判定不能」として
    // `pass=false` に倒す。
    let error_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let error_count_a = std::sync::Arc::clone(&error_count);
    let error_count_b = std::sync::Arc::clone(&error_count);

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let workload_a = || match f32_backend.batch_search(&queries) {
        Ok(hits) => hits.len(),
        Err(err) => {
            error_count_a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "batch_bench: {LABEL}: f32-resident batch_search returned an error \
                 during measurement: {err}"
            );
            0
        }
    };
    let workload_b = || match f16_backend.batch_search(&queries) {
        Ok(hits) => hits.len(),
        Err(err) => {
            error_count_b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "batch_bench: {LABEL}: f16-resident batch_search returned an error \
                 during measurement: {err}"
            );
            0
        }
    };
    let ab = run_ab(&config, workload_a, workload_b)
        .map_err(|err| format!("{LABEL}: A/B measurement failed: {err}"))?;
    if error_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        println!(
            "{LABEL}: not measurable in this environment (batch_search returned an \
             error during measurement; see stderr) requested=true pass=false"
        );
        return Ok(false);
    }

    let p95_a = p95_from_samples(&ab.a.samples)
        .map_err(|err| format!("{LABEL}: p95 of A samples unavailable: {err}"))?;
    let p95_b = p95_from_samples(&ab.b.samples)
        .map_err(|err| format!("{LABEL}: p95 of B samples unavailable: {err}"))?;
    let pass = check_improvement_at_least(p95_a, p95_b, min_improvement_pct)
        .map_err(|err| format!("{LABEL}: improvement check failed: {err}"))?;
    println!(
        "{LABEL}: rows={GPU_GATE_ROW_COUNT} dim={GPU_GATE_DIM} batch={GPU_GATE_BATCH_SIZE} \
         requested=true pass={pass}"
    );
    if verbose {
        println!("verbose({LABEL}): f32_resident_p95={p95_a:?} f16_resident_p95={p95_b:?}");
    }
    Ok(pass)
}

/// 診断用の事前生成プール 1 本（`total_iterations` バッチ分。各バッチ
/// [`AGG_BATCH_SIZE`] 本のクエリベクトルを持つ）。[`build_query_pool_pair`] の
/// 戻り値の型（clippy::type_complexity 対応）。
type DiagnosticQueryPool = Vec<Vec<Vec<f32>>>;

/// 診断（[`run_dynamic_window_push_drain_diagnostic`]）の A/B 計測区間からクエリ
/// 確保・コピーを追い出すための事前生成プール。`total_iterations` バッチ分
/// （各バッチ [`AGG_BATCH_SIZE`] 本）を A/B 交互に確保し（heap 配置の系統差を
/// 避ける。CORE-7 ゲート本体〔[`run_core7_gate`]〕と同じ「pool の対称化」方針）、
/// 測定区間側は `Vec::pop` で取り出すだけにする。
fn build_query_pool_pair(
    rng: &mut DeterministicRng,
    total_iterations: usize,
) -> (DiagnosticQueryPool, DiagnosticQueryPool) {
    let base = rng.next_vector(AGG_QUERY_DIM);
    let mut pool_a: Vec<Vec<Vec<f32>>> = Vec::with_capacity(total_iterations);
    let mut pool_b: Vec<Vec<Vec<f32>>> = Vec::with_capacity(total_iterations);
    for _ in 0..total_iterations {
        let mut batch_a: Vec<Vec<f32>> = Vec::with_capacity(AGG_BATCH_SIZE);
        let mut batch_b: Vec<Vec<f32>> = Vec::with_capacity(AGG_BATCH_SIZE);
        for _ in 0..AGG_BATCH_SIZE {
            batch_a.push(base.clone());
            batch_b.push(base.clone());
        }
        pool_a.push(batch_a);
        pool_b.push(batch_b);
    }
    (pool_a, pool_b)
}

/// 診断: [`DynamicWindowAggregator`] の `push`/`drain` それ自体のオーバーヘッドを
/// （256 本まとめて集約する場合について）検証なしの `Vec::push` と比較する。
/// **合否には数えない**（`simd_bench.rs::diagnostic_ab` と同型。[`run_core7_gate`]
/// が CORE-7 の定義量〔単発クエリ p95〕を担うため、本関数は集約器実装の退行を
/// 可視化する判別力の高い参考値としてのみ残す。詳細は
/// `docs/design/core7-dynamic-window-gate.md` の ADR 参照）。
///
/// `run_core7_gate` と同じく、A（対照。検証なしの `Vec::push`）・B（被検。
/// `DynamicWindowAggregator::push`/`drain`）双方の戻り値を測定区間外の sink へ
/// 退避してから drop する（`harness::ab::run_ab` の drop 契約参照）。
fn run_dynamic_window_push_drain_diagnostic(rng: &mut DeterministicRng, verbose: bool) {
    const LABEL: &str = "diagnostic_dynamic_window_push_drain";
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    let total_iterations = (config.warmup_iterations() as usize)
        .checked_add(config.measured_iterations() as usize)
        .expect("MeasurementConfig::new bounds iteration counts within usize");
    let (mut pool_a, mut pool_b) = build_query_pool_pair(rng, total_iterations);

    let mut sink_a: Vec<Vec<Vec<f32>>> = Vec::with_capacity(total_iterations);
    let mut sink_b: Vec<Vec<Vec<f32>>> = Vec::with_capacity(total_iterations);

    let workload_a = move || -> usize {
        let batch = pool_a
            .pop()
            .expect("query pool sized to warmup + measured iteration count");
        let mut queries: Vec<Vec<f32>> = Vec::with_capacity(AGG_BATCH_SIZE);
        for query in batch {
            queries.push(query);
        }
        let len = queries.len();
        sink_a.push(queries);
        len
    };

    let workload_b = move || -> usize {
        let batch = pool_b
            .pop()
            .expect("query pool sized to warmup + measured iteration count");
        let mut window = DynamicWindowAggregator::new();
        for query in batch {
            window
                .push(query)
                .expect("well-formed synthetic query must satisfy window limits");
        }
        let drained = window.drain();
        let len = drained.len();
        sink_b.push(drained);
        len
    };

    match run_ab(&config, workload_a, workload_b) {
        Ok(ab) => match (
            p95_from_samples(&ab.a.samples),
            p95_from_samples(&ab.b.samples),
        ) {
            (Ok(p95_a), Ok(p95_b)) => {
                println!(
                    "{LABEL}: batch_size={AGG_BATCH_SIZE} dim={AGG_QUERY_DIM} \
                     measured=true (not counted toward pass/fail)"
                );
                if verbose {
                    println!(
                        "verbose({LABEL}): direct_median={:?} direct_p95={p95_a:?} \
                         windowed_median={:?} windowed_p95={p95_b:?}",
                        ab.a.summary.median, ab.b.summary.median,
                    );
                }
            }
            _ => {
                println!("{LABEL}: p95 unavailable (not counted toward pass/fail)");
            }
        },
        Err(e) => {
            println!("{LABEL}: measurement unavailable ({e}) (not counted toward pass/fail)");
        }
    }
}

fn main() {
    let max_degradation_pct = match max_degradation_pct_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };
    // 実測値の既定非出力（Issue #279）の opt-in ゲート。CI（`GITHUB_ACTIONS`）下では
    // `verbose_requested_from_env` 自体が fail-closed で拒否するため、ここで検証不能な
    // 経路を通す前に必ず判定する。
    let verbose = match verbose_requested_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };

    let mut rng = DeterministicRng::new(1);
    let mut passed = true;

    // CORE-6/CORE-16/CORE-7 で共有する合成データセットを先に構築する（Issue #302:
    // CORE-7 も CORE-6/16 と同じ `BatchEngine::batch_search` 経由の実測ゲートへ
    // 再整合したため、同一データセットを流用する）。
    let gate_ctx = PolicyContext::new(BENCH_TENANT).expect("valid tenant");
    let gate_dataset = build_gate_dataset(&mut rng);

    // --- CORE-7: 動的窓集約を経由する単発クエリ経路の p95 劣化上限
    // （アクティブなゲート。モジュール冒頭コメント・[`run_core7_gate`] 参照）---
    let core7_ok = match run_core7_gate(&gate_dataset, &gate_ctx, max_degradation_pct, verbose) {
        Ok(ok) => ok,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };
    passed &= core7_ok;

    // --- 診断: push/drain 単体のオーバーヘッド（合否には数えない）---
    run_dynamic_window_push_drain_diagnostic(&mut rng, verbose);

    // --- CORE-6 / CORE-16: opt-in の実測ゲート（Issue #178 で実 GPU バックエンドへ
    // 接続済み。未 opt-in なら「対象外」を出力するだけで合否に数えない）---
    let core6_ok = match run_core6_gate(&gate_dataset, &gate_ctx, verbose) {
        Ok(ok) => ok,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };
    let core16_ok = match run_core16_gate(&gate_dataset, &gate_ctx, verbose) {
        Ok(ok) => ok,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };
    passed &= core6_ok;
    passed &= core16_ok;

    if !passed {
        eprintln!("batch_bench: acceptance criteria not met (TASK-130 CORE-6/CORE-7/CORE-16)");
        std::process::exit(1);
    }
}
