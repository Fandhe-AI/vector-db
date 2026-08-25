//! 性能・Recall 受け入れ基準の回帰ベンチ（TASK-127。ポインタ: `docs/spec/05-tasks.md`
//! TASK-127・対象ビヘイビア CORE-3, CORE-4, CORE-5, SEARCH-4）。
//!
//! 実測対象は `ParallelSearchProvider`（TASK-126）であり、`kernel::dot` の
//! スレッド並列化のみを行う（ベクトル化＝SIMD 演算は実装していない。
//! `parallel_search.rs` モジュール冒頭のコメント参照）。SIMD カーネルが
//! 未導入の現時点では本ベンチも SIMD の性能回帰を検出しない——ファイル名・
//! ドキュメントは「スカラー並列実装の回帰ベンチ」である実態に合わせている。
//! SIMD provider/カーネルが導入された際は、本ベンチの測定対象をそちらへ
//! 差し替える（ファイル名の再検討も含む）。
//!
//! `parallel_smoke.rs`（TASK-126 の手動計測スモーク）と異なり、本ベンチは数値基準との
//! 突き合わせまで行い、基準未達なら非ゼロ終了する回帰ゲートとして機能する
//! （`harness/accept.rs` の判定ヘルパを利用）。`.github/workflows/bench.yml`
//! （週次 schedule + workflow_dispatch による実行。CORE-5 は Issue #176 まで
//! opt-in のまま。同ファイル冒頭コメント参照）から実行される想定で、`make ci` の対象にはしない
//! （時間依存の測定値を CI アサーションへ混ぜない既存方針。`parallel_smoke.rs`
//! と同一）。
//!
//! - CORE-3・SEARCH-4: p95 レイテンシが上限以下であること（[`max_p95_from_env`]）
//! - CORE-4: `ParallelSearchProvider`（TASK-126・実測対象）と `CpuScalarProvider`
//!   （厳密最近傍の参照実装。`kernel.rs` 既存）の Top-k 一致率が下限以上であること
//!   （[`min_recall_from_env`]。両 provider とも厳密最近傍のため、本質的には
//!   並列実装の Top-k 一致を確認する回帰チェック。詳細は本文の CORE-4 セクション参照）
//! - CORE-5: 対照エンジンとの中央値比較。対照エンジンクレートの導入がユーザー承認必須
//!   （`.claude/rules/dependency-policy.md`）のため本 PR では未接続。CORE-5 の判定は
//!   `BENCH_CORE5=1` が設定された場合のみ opt-in で有効化する（[`core5_requested_from_env`]）。
//!   既定（未設定）では CORE-3/CORE-4 のみで合否を返し、CORE-5 は「対象外（未接続・
//!   Issue #176 で追跡中）」であることを標準出力へ明示する（silent skip にしない）。
//!   `BENCH_CORE5=1` を指定した場合は、未接続＝判定不能を fail-closed として扱う
//!   （判定関数 [`harness::accept::check_contrast_ratio_within_limit`] のみ先行実装済み。
//!   実測接続後は同フラグの下で自動的に実測ベースの判定へ切り替わる）。
//!   opt-in にしたのは、フラグ無指定時まで恒常的に fail させると CORE-3/CORE-4 の
//!   合否判定ゲートとして機能しなくなるため（codex-review 継続指摘）。
//!   `Cargo.toml`・PR 本文の「対象外・承認事項」参照）
//!
//! 数値基準（p95 上限・Recall 下限）・測定条件は spec（TASK-127）が SSOT。本ファイルには
//! 数値そのものをハードコードせず、実行時に環境変数（`BENCH_MAX_P95_MS`・
//! `BENCH_MIN_RECALL`）から注入する（`.claude/rules/spec-confidentiality.md`:
//! spec 本文・数値基準を public 資産へ転記しない。`harness/accept.rs` の判定関数が
//! 閾値を引数で受け取る設計と整合させる）。値は `.github/workflows/bench.yml` が
//! リポジトリの Actions variables（`vars.*`）から渡す想定。未設定・不正値の場合は
//! fail-closed で非ゼロ終了する（本ファイルにデフォルト値を持たない）。
//!
//! 標準出力には実測値と pass/fail のみを記録し、注入された閾値そのものは出力しない
//! （本 workflow は public リポの Actions ログとして残るため。閾値は spec が SSOT
//! であり、能動的にログへ書き出さない運用とする。`.claude/rules/spec-confidentiality.md`）。

// `harness` は `benches/measurement.rs`・`benches/parallel_smoke.rs` と同様、独立した
// コンパイル単位（cargo bench バイナリ）から取り込まれる共有ソース。本ファイルが
// 実際に使う項目のみで、未到達の `pub` 項目は `dead_code` 警告になりうるため
// モジュール全体を対象に許容する（`parallel_smoke.rs` と同一方針。`harness/mod.rs`
// 自体は変更しない）。
#[allow(dead_code)]
mod harness;

use harness::accept::{
    check_p95_within_limit, check_recall_within_limit, p95_from_samples, recall_at_k, worst_recall,
};
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;

use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
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

/// `BENCH_CORE5` 環境変数を読み取り、CORE-5（対照エンジン比較）判定を opt-in で
/// 有効化するかを返す。`"1"` のときのみ有効（未設定・それ以外の値はすべて無効）。
///
/// CORE-5 は対照エンジンクレートの導入がユーザー承認必須のため未接続であり
/// （`.claude/rules/dependency-policy.md`）、フラグ無指定の既定状態では CORE-5 を
/// 判定対象から明示的に除外する（`main` 側で「対象外」を出力する。silent skip には
/// しない）。フラグを常時 fail-closed にすると CORE-3/CORE-4 が基準を満たしても
/// 本ベンチが成功ケースを持たなくなり、合否ゲートとして機能しなくなるため
/// （codex-review 継続指摘）、既定では対象外・`BENCH_CORE5=1` 指定時のみ
/// fail-closed という opt-in 方式にする。
fn core5_requested_from_env() -> bool {
    std::env::var("BENCH_CORE5")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

fn main() {
    let max_p95 = match max_p95_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("parallel_bench: {msg}");
            std::process::exit(1);
        }
    };
    let min_recall = match min_recall_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("parallel_bench: {msg}");
            std::process::exit(1);
        }
    };

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
    // limit（BENCH_MAX_P95_MS の実測値）は意図的にログへ出力しない。閾値は spec が
    // SSOT であり、public リポの Actions ログへ能動的に書き出さない
    // （モジュール冒頭コメント参照）。
    println!(
        "p95_latency: rows={ROW_COUNT} dim={DIM} k={TOP_K} median={:?} p95={p95:?} pass={p95_ok}",
        measurement.summary.median,
    );

    // --- CORE-4: ParallelSearchProvider vs CpuScalarProvider の Top-k 一致率 ---
    // 両 provider はいずれも総当たり（厳密最近傍）実装であり（`parallel_search.rs:79`
    // のドキュメント参照）、`TopKSelector` の選出規約・同点順序を共有する
    // （`kernel.rs:146` 参照）。したがって本測定は近似 ANN の Recall 品質ゲートでは
    // なく、並列実装が参照実装と Top-k 集合で食い違わないことを bench 規模（本ファイル
    // の ROW_COUNT・DIM）で確認する回帰チェックである（同種の集合一致は
    // `parallel_search.rs` の単体テストでも小規模に検証済み）。近似 provider が
    // 導入された時点で本チェックが実質的な Recall 受け入れゲートとして機能する
    // （spec ポインタ: TASK-127 CORE-4）。
    // クエリ間の平均ではなく worst-query（最小値）で判定する
    // （`harness::accept::worst_recall` のドキュメント参照）。
    let reference = CpuScalarProvider;
    let mut recalls = Vec::with_capacity(RECALL_QUERY_COUNT);
    for _ in 0..RECALL_QUERY_COUNT {
        let query = rng.next_vector(DIM);

        let expected: Vec<u64> = reference
            .search(SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: DIM as u32,
                query: &query,
                k: TOP_K,
            })
            .expect("reference search must succeed for well-formed synthetic input")
            .into_iter()
            .map(|hit| hit.id)
            .collect();
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
    // limit（BENCH_MIN_RECALL の実測値）は意図的にログへ出力しない（p95_latency と
    // 同一方針。モジュール冒頭コメント参照）。
    println!(
        "topk_consistency(parallel_vs_scalar_exhaustive): k={TOP_K} queries={RECALL_QUERY_COUNT} recall_min={recall_min:.6} pass={recall_ok}"
    );

    // --- CORE-5: 対照エンジン比較（本 PR では未接続。BENCH_CORE5=1 で opt-in） ---
    // 対照エンジンクレートの導入がユーザー承認必須のため実測できない
    // （`.claude/rules/dependency-policy.md`）。`core5_requested_from_env` のドキュメント
    // 参照。フラグ未指定（既定）では CORE-5 を判定対象から明示的に除外し、その旨を
    // 標準出力へ書く（silent skip にしない。Issue #176 で追跡中）。フラグ指定時のみ
    // 「未測定＝判定不能」を fail-closed として受理判定に反映する。
    let core5_requested = core5_requested_from_env();
    if core5_requested {
        let core5_ok = false;
        passed &= core5_ok;
        println!(
            "contrast_ratio: not measured in this run (contrast engine dependency pending user approval; see harness::accept::check_contrast_ratio_within_limit) requested=true pass={core5_ok}"
        );
    } else {
        println!(
            "contrast_ratio: out of scope for this run (contrast engine dependency pending user approval; not counted toward pass/fail; set BENCH_CORE5=1 to opt in once connected; see harness::accept::check_contrast_ratio_within_limit and issue #35) requested=false"
        );
    }

    if !passed {
        let core5_suffix = if core5_requested { "CORE-5/" } else { "" };
        eprintln!(
            "parallel_bench: acceptance criteria not met (TASK-127 CORE-3/CORE-4/{core5_suffix}SEARCH-4)"
        );
        std::process::exit(1);
    }
}
