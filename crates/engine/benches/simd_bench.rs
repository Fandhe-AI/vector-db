//! 性能・Recall 受け入れ基準の回帰ベンチ（TASK-127。ポインタ: `docs/spec/05-tasks.md`
//! TASK-127・対象ビヘイビア CORE-3, CORE-4, SEARCH-4）。CORE-5（対照エンジン比較）は
//! `contrast_bench.rs`（同じく TASK-127・Issue #176）が独立バイナリとして判定する
//! （failure domain 分離。同ファイル冒頭コメント参照）。
//!
//! 実測対象は `ParallelSearchProvider`（TASK-126・スレッド並列）が `kernel::dot` 経由で
//! 呼び出す TASK-156（CORE-14・Issue #109・PR #202）の実行時検出 SIMD カーネル
//! （AVX2+FMA・AVX-512・NEON。`isa::current().dot` へ委譲）であり、スレッド並列化と
//! SIMD 化の双方が乗った実測になる（`parallel_search.rs`・`isa.rs` モジュール冒頭の
//! コメント参照）。ファイル名は当初 `simd_bench.rs` だったが、PR #143（TASK-127）
//! 導入時点では SIMD カーネルが未実装だったため実態に合わせて `parallel_bench.rs` へ
//! 改名していた。TASK-156 で SIMD カーネルが入ったことを受け、本 Issue（#177）で
//! `simd_bench.rs` へ戻し、検出 ISA の記録・CORE-4 参照の真のスカラー逐次和への
//! 差し替えを行う（詳細は各セクション参照）。
//!
//! `parallel_smoke.rs`（TASK-126 の手動計測スモーク）と異なり、本ベンチは数値基準との
//! 突き合わせまで行い、基準未達なら非ゼロ終了する回帰ゲートとして機能する
//! （`harness/accept.rs` の判定ヘルパを利用）。`.github/workflows/bench.yml`
//! （週次 schedule + workflow_dispatch による実行）から実行される想定で、`make ci` の
//! 対象にはしない（時間依存の測定値を CI アサーションへ混ぜない既存方針。
//! `parallel_smoke.rs` と同一）。
//!
//! - 冒頭で検出 ISA を [`EnvReport`] へ記録する（`sql_c1_bench.rs` と同一パターン）。
//!   `isa::current().isa()` が [`engine::isa::DetectedIsa::Scalar`] を返す環境
//!   （SIMD 拡張なし）では SIMD 経路が測定不能＝判定不能として fail-closed で
//!   非ゼロ終了する（GitHub ホステッド x86_64 runner は AVX2+FMA 以上・aarch64 は
//!   常に NEON を検出するため実運用で恒常 red にはならない。ISA を上書きする
//!   環境変数・フラグは意図的に設けない——`isa.rs`「CORE-12 との整合」節参照）。
//! - CORE-3・SEARCH-4: p95 レイテンシが上限以下であること（[`max_p95_from_env`]）。
//!   出力行に検出 ISA を併記する。
//! - CORE-4: `ParallelSearchProvider`（SIMD+並列・実測対象）と
//!   `harness::scalar_reference::top_k_ids_scalar`（`engine::isa::dot_scalar` による
//!   真のスカラー逐次和の総当たり参照）の Top-k 一致率が下限以上であること
//!   （[`min_recall_from_env`]）。旧実装は参照に `CpuScalarProvider` を使っていたが、
//!   同 provider も TASK-156 以降 `kernel::dot` 経由で SIMD カーネルを使うため
//!   「SIMD+並列 vs SIMD+逐次」（スレッド分割の整合確認。`tests/parallel_search.rs`
//!   側で別途担保済み）にしかならず、SIMD 演算順序（レーン分割・FMA）による丸め差を
//!   含む「SIMD 経路 vs 真のスカラー逐次和」という CORE-4 本来の判定になっていな
//!   かった（`harness::scalar_reference` モジュール冒頭コメント参照）。
//! - CORE-5（対照エンジン接続）は本ファイルでは判定しない。`contrast_bench.rs` が
//!   `contrast-bench` feature 限定の独立バイナリとして実測・判定する（Issue #176。
//!   対照エンジン側の C++ FFI ビルド障害を CORE-3/CORE-4 のゲートへ波及させない
//!   failure domain 分離。同ファイル冒頭コメント参照）。
//! - 診断 A/B（[`harness::ab::run_ab`]）: `ParallelSearchProvider`（SIMD+並列）と
//!   単一スレッドの真のスカラー参照（総当たり）を比較する。**この比較は SIMD 単独の
//!   効果を分離したものではない**（codex-review 指摘対応）。A 側はスレッド並列化・
//!   `TopKSelector`（`kernel.rs`）を経由し、B 側は単一スレッド・独立実装の
//!   Top-k 選出（`harness::scalar_reference`）を経由するため、`median_ratio` には
//!   SIMD カーネル差に加えて並列化効果・Top-k 選出アルゴリズム差・割り当て/ソート量の
//!   差も乗る。ここで可視化しているのは「エンドツーエンドの SIMD 並列経路 vs
//!   スカラー参照経路」の実測比であり、内積カーネルのみを差し替えた SIMD 単独効果の
//!   測定ではない点に注意する。合否には数えない（`sql_c1_bench.rs::diagnostic_ab`
//!   と同型）。CORE-3/CORE-4 とは別に `DIAG_AB_ROW_COUNT`（小さい専用件数）・
//!   プロトコル下限ちょうどの反復回数（warmup 20 回・計測 20 回）を使う
//!   （Issue #177 codex-review 指摘: `ROW_COUNT` 全件・warmup 20 回＋計測 50 回を
//!   合否に数えない診断でも流用すると、B 側の逐次内積＋全件 `sort_by` が
//!   `.github/workflows/bench.yml` の `timeout-minutes: 30` を圧迫し得たため）。
//!
//! 数値基準（p95 上限・Recall 下限）・測定条件は spec（TASK-127）が SSOT。本ファイルには
//! 数値そのものをハードコードせず、実行時に環境変数（`BENCH_MAX_P95_MS`・
//! `BENCH_MIN_RECALL`）から注入する（`.claude/rules/spec-confidentiality.md`:
//! spec 本文・数値基準を public 資産へ転記しない。`harness/accept.rs` の判定関数が
//! 閾値を引数で受け取る設計と整合させる）。値は `.github/workflows/bench.yml` が
//! リポジトリの Actions variables（`vars.*`）から渡す想定。未設定・不正値の場合は
//! fail-closed で非ゼロ終了する（本ファイルにデフォルト値を持たない）。
//!
//! 標準出力には pass/fail・非数値の状態のみを記録し、注入された閾値・実測値（median・
//! p95・recall_min・診断 A/B の a_median/b_median/median_ratio）は出力しない（本
//! workflow は public リポの Actions ログとして残るため。実測値からも数値基準を
//! 逆算されうる。Issue #277。`contrast_bench.rs`（CORE-5・Issue #224）と同方針。
//! `.claude/rules/spec-confidentiality.md`）。実測値がローカルで必要な場合は
//! `--verbose`（[`verbose_requested`]）で opt-in できるが、`GITHUB_ACTIONS` 環境下では
//! public ログ混入防止のため fail-closed で拒否する（[`running_under_github_actions`]）。

// `harness` は `benches/measurement.rs`・`benches/parallel_smoke.rs` と同様、独立した
// コンパイル単位（cargo bench バイナリ）から取り込まれる共有ソース。本ファイルが
// 実際に使う項目のみで、未到達の `pub` 項目は `dead_code` 警告になりうるため
// モジュール全体を対象に許容する（`parallel_smoke.rs` と同一方針。`harness/mod.rs`
// 自体は変更しない）。
#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{
    check_p95_within_limit, check_recall_within_limit, p95_from_samples, recall_at_k, worst_recall,
};
use harness::env_report::EnvReport;
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::scalar_reference::top_k_ids_scalar;

use engine::isa::DetectedIsa;
use engine::kernel::{SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use std::time::Duration;

/// 測定条件（行数・次元・k）。TASK-127・`parallel_smoke.rs` と同一値を用いる
/// （測定条件そのものは spec の SSOT だが、既存ベンチ〔TASK-126〕がすでに同じ値を
/// 公開コードへ含んでいるため新規の漏えいではない）。
const ROW_COUNT: usize = 100_000;
const DIM: usize = 768;
const TOP_K: usize = 20;

/// Recall@k 判定に使うクエリ本数（複数クエリの最小値〔worst-query〕で判定し、
/// 単一クエリでの偶然の完全一致に判定全体が引きずられないようにする。
/// 本数自体は本ベンチ独自の実装選択で spec 由来の値ではない）。
const RECALL_QUERY_COUNT: usize = 20;

/// 診断 A/B（合否に数えない `run_ab` 計測）専用の行数（Issue #177 codex-review
/// 指摘対応）。CORE-3/CORE-4 は `ROW_COUNT`（100,000 件）の全件を判定根拠として
/// 使う必要があるため変更しないが、診断 A/B は合否に数えない参考値であり、B 側
/// （`top_k_ids_scalar`）が `ROW_COUNT` 全件の逐次内積＋`sort_by` を warmup 20 回＋
/// 計測 50 回（`DIAG_AB_CONFIG` 参照）実行すると、GitHub ホステッド runner・
/// `.github/workflows/bench.yml` の `timeout-minutes: 30` 制約下で恒常的な
/// タイムアウト要因になり得た（約 54 億要素分の逐次演算＋70 回の O(n log n) ソート）。
/// 診断が示したいのは「SIMD 並列経路 vs スカラー参照経路」の比率であり、比率の
/// 意味は行数に依存しないため、CORE-3/CORE-4 用データセットとは別に小さい専用
/// データセットを生成して用いる。
///
/// ただし縮小しすぎると A 側（`ParallelSearchProvider`）が実際にはスレッド分割
/// されず、ログの `end_to_end_simd_parallel_path` ラベルと実態（シングルスレッド
/// SIMD パス）が食い違う（Cursor Bugbot 指摘・PR #222）。`parallel_search.rs`
/// `thread_count_for` はスレッド数を `row_count / MIN_ROWS_PER_THREAD`
/// （`parallel_search.rs` 内 `MIN_ROWS_PER_THREAD = 1024`）でクランプするため、
/// 少なくとも 2 スレッドに分割されるには行数が `MIN_ROWS_PER_THREAD` の 2 倍
/// （2,048）以上必要。旧値 2,000 はこれを下回っており（`MIN_ROWS_PER_THREAD`
/// × 1 未満）常にシングルスレッド経路だった。`MIN_ROWS_PER_THREAD` の 4 倍
/// （実分割後も 1 スレッドあたり `MIN_ROWS_PER_THREAD` を十分上回る）を確保
/// しつつ、タイムアウト回避の目的（行数を小さく保つ）は維持する値として選定した。
///
/// ただし `thread_count_for` は行数だけでなく実行時の
/// `std::thread::available_parallelism()` にもスレッド数を制限される
/// （`parallel_search.rs` 参照）。並列度 1 の runner・コンテナ（CPU 制限環境）や
/// ローカル環境では、この行数条件を満たしていても A 側が単一スレッドのままに
/// なり得る（codex-review 指摘・Issue #177／PR #222）。行数だけで「2 スレッド
/// 以上に分割される」と決め打たず、`engine::parallel_search::expected_thread_count`
/// で実行時に見込みスレッド数を取得し、2 未満ならログラベルを
/// `end_to_end_simd_single_thread_path_vs_scalar_reference_path` へ切り替えて
/// 実態と食い違わせない（`main` 関数内 `diag_label` 参照）。
const DIAG_AB_ROW_COUNT: usize = 4_096;

/// `BENCH_MAX_P95_MS` 環境変数（ミリ秒・整数）を読み取り、CORE-3・SEARCH-4 の
/// p95 上限として使う `Duration` を得る。未設定・非数値・0 以下は fail-closed で
/// 判定不能として扱う（数値そのものは spec が SSOT。本ファイルにはデフォルト値を
/// 持たない——`.claude/rules/spec-confidentiality.md`）。
///
/// GitHub Actions では未設定の repo variable（`vars.*`）は空文字列に解決されるため
/// （`.github/workflows/bench.yml` 参照）、CI での未設定時に実際にたどるのは
/// `std::env::var` の `Err`（"is not set"）ではなく、空文字列パース失敗の
/// "must be a positive integer" エラーである。いずれの経路でも fail-closed で
/// 非ゼロ終了する点は変わらない。
fn max_p95_from_env() -> Result<Duration, String> {
    let raw = std::env::var("BENCH_MAX_P95_MS")
        .map_err(|_| "BENCH_MAX_P95_MS is not set (see .github/workflows/bench.yml vars)")?;
    let millis: u64 = raw
        .trim()
        .parse()
        .map_err(|_| "BENCH_MAX_P95_MS must be a positive integer (milliseconds)".to_string())?;
    if millis == 0 {
        return Err("BENCH_MAX_P95_MS must be greater than 0".to_string());
    }
    Ok(Duration::from_millis(millis))
}

/// `BENCH_MIN_RECALL` 環境変数（`(0.0, 1.0]` の浮動小数点）を読み取り、CORE-4 の
/// Recall@k 下限として使う値を得る。範囲外・未設定・非数値は fail-closed で
/// 判定不能として扱う（`harness::accept::check_recall_within_limit` も同じ範囲を
/// 再検証するが、ここでは早期に人間可読なエラーを出すために先立って検証する）。
/// `0.0` は「どんな recall 値でも pass」となり CORE-4 のゲートを実質的に
/// 無効化するため、`max_p95_from_env` が `0` を拒否するのと対称に下限からも除外する。
///
/// GitHub Actions では未設定の repo variable（`vars.*`）は空文字列に解決されるため
/// （`.github/workflows/bench.yml` 参照）、実運用で未設定時にたどるのは主に
/// 数値パース失敗のエラーメッセージであり、`std::env::var` の `Err`（環境変数自体が
/// 存在しない場合）は主にローカル実行時の経路になる。いずれの経路でも fail-closed
/// で非ゼロ終了する点は変わらない。
fn min_recall_from_env() -> Result<f64, String> {
    let raw = std::env::var("BENCH_MIN_RECALL")
        .map_err(|_| "BENCH_MIN_RECALL is not set (see .github/workflows/bench.yml vars)")?;
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| "BENCH_MIN_RECALL must be a floating-point number".to_string())?;
    if !(value > 0.0 && value <= 1.0) {
        return Err("BENCH_MIN_RECALL must be within (0.0, 1.0]".to_string());
    }
    Ok(value)
}

/// `--verbose` 指定の有無を判定する。`[[bench]] simd_bench` は `harness = false` /
/// `test = false`（`Cargo.toml`）のため cargo 標準のベンチ引数パーサを経由できず、
/// 自前で `std::env::args()` を走査する（`tier_latency_bench.rs::help_requested` と
/// 同一パターン）。cargo が付与しうる未知の引数（`--bench` 等）は無視し、完全一致する
/// `--verbose` の有無のみを見る。
fn verbose_requested() -> bool {
    std::env::args().skip(1).any(|arg| arg == "--verbose")
}

/// GitHub Actions 上で実行されているかを判定する。`GITHUB_ACTIONS` は Actions が
/// ランナーへ自動設定する環境変数であり、値の中身は見ず存在有無のみを見る
/// （untrusted な値のパースを避ける）。`--verbose` と併用された場合に実測値の
/// public ログ混入を防ぐための defense-in-depth（モジュール冒頭コメント参照）。
fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

fn main() {
    let verbose = verbose_requested();
    if verbose && running_under_github_actions() {
        eprintln!(
            "simd_bench: --verbose (numeric output) is not permitted under GitHub Actions because workflow logs are public; run locally without GITHUB_ACTIONS to see measured values"
        );
        std::process::exit(1);
    }

    let max_p95 = match max_p95_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("simd_bench: {msg}");
            std::process::exit(1);
        }
    };
    let min_recall = match min_recall_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("simd_bench: {msg}");
            std::process::exit(1);
        }
    };

    // 検出 ISA を記録する（`sql_c1_bench.rs` と同一パターン）。標準出力の env 行から
    // 「SIMD 経路を実際に測ったか」を後から確認できるようにする。
    let isa = engine::isa::current().isa();
    println!("{}", EnvReport::capture(format!("{isa:?}")));

    // Scalar 検出時は SIMD 経路が測定不能＝判定不能として fail-closed（モジュール
    // 冒頭コメント参照）。ISA を上書きする入口は意図的に設けない
    // （`isa.rs`「CORE-12 との整合」節と同じ方針）。
    if isa == DetectedIsa::Scalar {
        eprintln!(
            "simd_bench: no SIMD instruction set detected at runtime (isa=Scalar); SIMD path is not measurable on this host"
        );
        std::process::exit(1);
    }

    let mut rng = DeterministicRng::new(1);
    let ids: Vec<u64> = (0..ROW_COUNT as u64).collect();
    let mut vectors = Vec::with_capacity(ROW_COUNT * DIM);
    for _ in 0..ROW_COUNT {
        vectors.extend(rng.next_vector(DIM));
    }

    let mut passed = true;

    // --- CORE-3 / SEARCH-4: p95 レイテンシ ---
    let provider = ParallelSearchProvider;
    let latency_query = rng.next_vector(DIM);
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    let measurement = run(&config, || {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &latency_query,
            k: TOP_K,
        };
        provider
            .search(input)
            .expect("search must succeed for well-formed synthetic input")
    })
    .expect("measurement must satisfy protocol minimums");

    let p95 = p95_from_samples(&measurement.samples).expect("non-empty samples must yield a p95");
    let p95_ok = check_p95_within_limit(p95, max_p95);
    passed &= p95_ok;
    // limit（BENCH_MAX_P95_MS）・実測値（median・p95）は意図的にログへ出力しない。
    // 閾値・実測値のいずれも public リポの Actions ログへ能動的に書き出さない
    // （モジュール冒頭コメント参照）。
    println!("p95_latency: rows={ROW_COUNT} dim={DIM} k={TOP_K} isa={isa:?} pass={p95_ok}");
    if verbose {
        println!(
            "verbose_p95_latency: median={:?} p95={p95:?}",
            measurement.summary.median,
        );
    }

    // --- CORE-4: ParallelSearchProvider（SIMD+並列） vs 真のスカラー逐次和の Top-k 一致率 ---
    // 参照実装は `harness::scalar_reference::top_k_ids_scalar`（`engine::isa::dot_scalar`
    // による総当たり）を使う。旧実装は `CpuScalarProvider` を参照にしていたが、同
    // provider も TASK-156（CORE-14）以降 `kernel::dot` 経由で SIMD カーネルを使うため
    // 「SIMD+並列 vs SIMD+逐次」（スレッド分割の整合確認）にしかならなかった
    // （`tests/parallel_search.rs::multi_thread_path_matches_scalar_reference_at_scale`
    // 等が別途 `make ci` で担保）。本判定は SIMD レーン分割・FMA による丸め差を含む
    // 「SIMD 経路 vs 真のスカラー逐次和」の比較であり、近似 ANN provider 導入前でも
    // SIMD 経路の正しさゲートとして機能する（spec ポインタ: TASK-127 CORE-4）。
    // クエリ間の平均ではなく worst-query（最小値）で判定する
    // （`harness::accept::worst_recall` のドキュメント参照）。
    let mut recalls = Vec::with_capacity(RECALL_QUERY_COUNT);
    for _ in 0..RECALL_QUERY_COUNT {
        let query = rng.next_vector(DIM);

        let expected = top_k_ids_scalar(&ids, &vectors, DIM, &query, TOP_K)
            .expect("well-formed synthetic input yields a scalar reference top-k");
        let actual: Vec<u64> = provider
            .search(SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: DIM as u32,
                query: &query,
                k: TOP_K,
            })
            .expect("candidate search must succeed for well-formed synthetic input")
            .into_iter()
            .map(|hit| hit.id)
            .collect();

        recalls.push(recall_at_k(&expected, &actual).expect("non-empty reference top-k"));
    }
    let recall_min =
        worst_recall(&recalls).expect("RECALL_QUERY_COUNT queries yield a non-empty recall list");
    let recall_ok = check_recall_within_limit(recall_min, min_recall)
        .expect("min_recall validated by min_recall_from_env");
    passed &= recall_ok;
    // limit（BENCH_MIN_RECALL）・実測値（recall_min）は意図的にログへ出力しない
    // （p95_latency と同一方針。モジュール冒頭コメント参照）。
    println!(
        "topk_consistency(simd_parallel_vs_scalar_reference): k={TOP_K} queries={RECALL_QUERY_COUNT} pass={recall_ok}"
    );
    if verbose {
        println!("verbose_topk_consistency: recall_min={recall_min:.6}");
    }

    // CORE-5（対照エンジン比較）は `contrast_bench.rs` が独立バイナリとして判定する
    // （モジュール冒頭コメント参照）。

    // --- 診断 A/B: ParallelSearchProvider（SIMD+並列） vs 単一スレッド真のスカラー参照 ---
    // 合否には数えない（`sql_c1_bench.rs::diagnostic_ab` と同型）。**SIMD 単独効果の
    // 測定ではない**（モジュール冒頭コメント参照。codex-review 指摘対応）: A 側は
    // 並列化・`TopKSelector` を経由し、B 側は単一スレッド・独立実装の Top-k 選出を
    // 経由するため、`median_ratio` には並列化効果・Top-k 選出アルゴリズム差・
    // 割り当て/ソート量の差も乗る。ここで可視化するのは「エンドツーエンドの SIMD
    // 並列経路 vs スカラー参照経路」の実測比である。
    //
    // CORE-3/CORE-4 と同じ `ids`/`vectors`（`ROW_COUNT` 件）・`config`（warmup 20 回・
    // 計測 50 回）を流用すると、B 側の `top_k_ids_scalar` が呼び出しのたびに
    // `ROW_COUNT × DIM` 要素の逐次内積と `ROW_COUNT` 件全体の `sort_by` を行うため、
    // 合否に数えない診断のためだけに大きな計算コストが warmup+計測の 70 回分
    // 乗ってしまう（`.github/workflows/bench.yml` の `timeout-minutes: 30` を
    // 圧迫し得る。Issue #177 codex-review 指摘）。診断が示したいのは比率であって
    // 絶対値ではないため、専用の小さいデータセット（`DIAG_AB_ROW_COUNT` 件）と
    // プロトコル下限ちょうどの反復回数（warmup 20 回・計測 20 回）で行う。
    let mut diag_ids: Vec<u64> = Vec::with_capacity(DIAG_AB_ROW_COUNT);
    let mut diag_vectors = Vec::with_capacity(DIAG_AB_ROW_COUNT * DIM);
    for i in 0..DIAG_AB_ROW_COUNT {
        diag_ids.push(i as u64);
        diag_vectors.extend(rng.next_vector(DIM));
    }
    let diag_config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let ab_query = rng.next_vector(DIM);

    // A 側が実際に複数スレッドへ分割されるかは `DIAG_AB_ROW_COUNT` だけでなく
    // 実行環境の `std::thread::available_parallelism()` にも依存する
    // （`parallel_search.rs::thread_count_for`）。並列度 1 の runner・コンテナ
    // （CPU 制限環境）では `DIAG_AB_ROW_COUNT` をいくら増やしても A 側は単一
    // スレッドのままになり、常に "parallel_path" とラベル付けすると診断値の
    // 意味を誤認させる（codex-review 指摘・Issue #177／PR #222）。
    // `engine::parallel_search::expected_thread_count` で実際に使われる見込みの
    // スレッド数を事前取得し、ラベル・出力へ実測反映する（2 未満なら
    // 「並列」を名乗らない）。
    let diag_expected_threads = engine::parallel_search::expected_thread_count(DIAG_AB_ROW_COUNT);
    let diag_label = if diag_expected_threads >= 2 {
        "end_to_end_simd_parallel_path_vs_scalar_reference_path"
    } else {
        "end_to_end_simd_single_thread_path_vs_scalar_reference_path"
    };
    match run_ab(
        &diag_config,
        || -> Vec<u64> {
            provider
                .search(SearchInput {
                    ids: &diag_ids,
                    vectors: &diag_vectors,
                    dim: DIM as u32,
                    query: &ab_query,
                    k: TOP_K,
                })
                .expect("candidate search must succeed for well-formed synthetic input")
                .into_iter()
                .map(|hit| hit.id)
                .collect()
        },
        || -> Vec<u64> {
            top_k_ids_scalar(&diag_ids, &diag_vectors, DIM, &ab_query, TOP_K)
                .expect("well-formed synthetic input yields a scalar reference top-k")
        },
    ) {
        Ok(ab) => {
            println!(
                "diagnostic_ab({diag_label}, not isolated to SIMD kernel alone): rows={DIAG_AB_ROW_COUNT} dim={DIM} k={TOP_K} expected_threads={diag_expected_threads} measured=true (not counted toward pass/fail; small dedicated dataset, not comparable to p95_latency's rows={ROW_COUNT} figure)"
            );
            if verbose {
                println!(
                    "verbose_diagnostic_ab: a_median={:?} b_median={:?} median_ratio={:.4}",
                    ab.a.summary.median, ab.b.summary.median, ab.median_ratio
                );
            }
        }
        Err(e) => {
            // A/B は診断情報であり合否に含めないため、算出不能（分母 0 等）でも
            // 本体の pass/fail には影響させない。ただし silent にはせず記録する。
            println!(
                "diagnostic_ab({diag_label}, not isolated to SIMD kernel alone): expected_threads={diag_expected_threads} unavailable ({e}) (not counted toward pass/fail)"
            );
        }
    }

    if !passed {
        eprintln!("simd_bench: acceptance criteria not met (TASK-127 CORE-3/CORE-4/SEARCH-4)");
        std::process::exit(1);
    }
}
