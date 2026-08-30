//! ティア別レイテンシ受け入れ基準の時間非依存ヘルパ（TASK-116。ポインタ:
//! `docs/spec/05-tasks.md` TASK-116・対象ビヘイビア: `docs/spec/04-behavior/
//! query-planning.md` PLAN-4, PLAN-6, PLAN-7。判定内容・測定段階・数値基準は
//! spec 側が SSOT であり本ファイルには転記しない
//! （`.claude/rules/spec-confidentiality.md`）。
//!
//! `benches/tier_latency_bench.rs`（実測。時間依存・`make ci` 対象外）が計測した
//! サンプルを本モジュールの判定関数へ渡す。`tests/tier_latency_accept.rs` が
//! `#[path]` で本モジュールを取り込み、実測タイマー・env に依存せず
//! `cargo test`（`make ci` 対象）で回帰検証する（`harness/accept.rs`・
//! `sql_c1_bench.rs`/`c1_bench_accept.rs` と同一パターン）。
//!
//! 判定関数はいずれも呼び出し元（`tier_latency_bench.rs`）から env 経由で注入
//! された閾値を受け取るのみとする（`harness::accept` と同一方針）。
//!
//! # 計測質問（ティア routing 実証）
//!
//! [`DIALOGUE_QUESTION`]・[`PRECISION_QUESTION`] は、**対象テーブルの辞書
//! スナップショット内容に依存しない**判定経路のみで意図したティアへ決定的に
//! 分類されるよう選んである（`tiering.rs` の
//! `path_extension_match_yields_direct`／`abstraction_cue_yields_high_precision`
//! と同一の判定経路）。これにより計測用コーパスの内容を変更してもルーティング
//! の実証結果が揺れない。判定優先順の詳細は `tiering.rs::classify` を参照。
//!
//! # LLM 不正応答（`PlanError::InvalidResponse`）試行の扱い（TASK-116・Issue #316）
//!
//! 常駐 Ollama の応答形式は非決定的であり、`plan_query_with_classification`／
//! `execute_sql`（`USING PLAN`）の各試行が `PlanError::InvalidResponse`（または
//! それを丸め込んだ `SqlSurfaceError::Internal`）で失敗しうる。本リポでは
//! **除外対象は `InvalidResponse` のみ**とし、[`classify_core_error`]・
//! [`classify_sql_error`] で分類のうえ [`super::protocol::run_fallible`] へ渡す。
//! `Timeout`・`Unavailable` 等の他エラーは除外せず致命扱いとする（除外すると
//! p95 判定が fail-open になるため）。除外試行は有効サンプル数に達するまで
//! 追加試行で埋め合わせ、段ごとの除外数上限（[`DEFAULT_MAX_INVALID_RESPONSE_TRIALS`]・
//! `BENCH_TIER_MAX_INVALID_RESPONSE_TRIALS` で上書き可能）を超えた場合は
//! `run_fallible` が `BenchError::ExcludedTrialsExceeded` で打ち切る。この既定値・
//! 上限方式はいずれも spec 由来の受け入れ基準ではなく、本リポ独自の実装既定値
//! （`docs/design/query-tiering-criteria.md` の判定基準と同じ位置づけ。
//! `docs/design/tier-latency-acceptance.md`「不正応答試行の扱い」節参照）。

use std::time::Duration;

use super::accept::{check_p95_within_limit, p95_from_samples};
use super::protocol::TrialFailure;
use super::rng::DeterministicRng;
use super::stats::BenchError;

use engine::core::CoreError;
use engine::query_planner::{OllamaClient, OllamaConfig, PlanError};
use engine::sql::allowlist::SqlSurfaceError;

/// 対話ティア（[`engine::tiering::Tier::Dialogue`]）へ決定的に分類される質問。
/// パス拡張子（`.rs`）一致による [`engine::tiering::ClassificationSignal::PathMatch`]
/// を用いる（モジュールドキュメント参照）。
pub const DIALOGUE_QUESTION: &str = "open src/module.rs and check it";

/// 高精度ティア（[`engine::tiering::Tier::HighPrecision`]）へ決定的に分類される質問。
/// 手掛かり語（`explain`／`architecture`）一致による
/// [`engine::tiering::ClassificationSignal::AbstractionCue`] を用いる。
pub const PRECISION_QUESTION: &str = "explain the overall architecture";

/// `USING PLAN('<question>')` の一意ディスパッチ（TASK-77・SQL-5）を叩く `SELECT`
/// 文を組み立てる（`harness::sql_c1::c1_statement` と同じく、bench 本体
/// （`tier_latency_bench.rs`）とテスト（`tests/tier_latency_accept.rs`）の双方が
/// 同一の文字列生成ロジックを共有し、実測経路とテスト経路の SQL がドリフトしない
/// ようにする）。`question` は本モジュールが定義する固定定数
/// （[`DIALOGUE_QUESTION`]/[`PRECISION_QUESTION`]）のみを渡す前提で、単純結合で
/// 足りる（`'`／`\` を含まない。untrusted 入力の組み立てには使わない）。
pub fn using_plan_statement(table: &str, question: &str, top_k: usize) -> String {
    format!("SELECT id FROM {table} USING PLAN('{question}') LIMIT {top_k}")
}

/// 計測対象テーブルの 1 行分（`path`/`body`/`embedding` 列。TASK-120 のファイル形
/// `INSERT` 規約と同じ列名を使う）。
pub struct CorpusRow {
    pub id: u64,
    pub path: String,
    pub body: String,
    pub embedding: Vec<f32>,
}

/// `count` 行・`dim` 次元の決定的な合成コーパスを生成する（シード固定
/// [`DeterministicRng`]。`query_planning_recall.rs` の合成コーパス生成と同じ
/// 「決定的・語彙上限あり」方針を踏襲するが、本モジュールは routing の実証に
/// [`DIALOGUE_QUESTION`]/[`PRECISION_QUESTION`] を使わない設計（モジュール
/// ドキュメント参照）のため、コーパス自体の語彙内容は計測結果へ影響しない
/// 単純な合成データで足りる）。
pub fn build_corpus(count: usize, dim: usize, seed: u64) -> Vec<CorpusRow> {
    let mut rng = DeterministicRng::new(seed);
    (0..count)
        .map(|i| {
            let embedding = rng.next_vector(dim);
            CorpusRow {
                id: i as u64,
                path: format!("corpus/doc_{i}.txt"),
                body: format!("synthetic document body {i} for tier latency measurement"),
                embedding,
            }
        })
        .collect()
}

/// `BENCH_TIER` env の opt-in ゲートを有効化するかを返す。値が未設定・
/// 空文字のときのみ「対象外」（`false`）とし、それ以外の非空値はすべて opt-in
/// 要求とみなす（`batch_bench.rs::opt_in_requested_from_env` と同一方針。
/// TASK-130・CORE-6/16 opt-in ゲートを踏襲）。常駐 Ollama 前提の実測は CI
/// 経路を持たず（README「ティア別レイテンシ受け入れ基準の実測手順」参照）、
/// `make bench-tier` をローカル・外部計測環境で運用者が明示指定した run での
/// み実測ゲートを評価する。
pub fn opt_in_requested(raw: Option<&str>) -> bool {
    raw.map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// p95 上限（ミリ秒）の生文字列を解析する。正の整数以外・0 は `Err`（fail-closed。
/// `sql_c1_bench.rs::max_p95_from_env` と同一方針）。`var_name` はエラーメッセージ
/// にのみ使う（呼び出し元が env 変数名を識別できるようにする）。
pub fn parse_max_p95_ms(raw: &str, var_name: &str) -> Result<Duration, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{var_name} must not be empty"));
    }
    let millis: u64 = trimmed
        .parse()
        .map_err(|_| format!("{var_name} must be a positive integer (milliseconds)"))?;
    if millis == 0 {
        return Err(format!("{var_name} must be greater than 0"));
    }
    Ok(Duration::from_millis(millis))
}

/// 接続ポートの生文字列を解析する（`u16` 範囲。fail-closed）。
pub fn parse_port(raw: &str, var_name: &str) -> Result<u16, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{var_name} must not be empty"));
    }
    trimmed
        .parse::<u16>()
        .map_err(|_| format!("{var_name} must be a valid port number (0-65535)"))
}

/// モデル名の生文字列を検証する（空文字は fail-closed で拒否するのみ。値の妥当性
/// 自体は Ollama 接続時のエラーに委ねる）。
pub fn parse_model_name(raw: &str, var_name: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{var_name} must not be empty"));
    }
    Ok(trimmed.to_string())
}

/// 接続ホストの生文字列を検証する（空文字は fail-closed で拒否するのみ）。
///
/// 未設定の env は空文字列として渡ってくる場合がある（`sql_c1_bench.rs::
/// max_p95_from_env` 等と同じ前提）ため、`OllamaConfig::with_host` に空文字列を
/// そのまま渡さない（空文字列は IP リテラルとして解釈できずホスト名として素通り
/// してしまい、未設定にもかかわらず接続を試みて分かりにくい接続エラーになる。
/// README「ティア別レイテンシ受け入れ基準の実測手順」が約束する fail-closed
/// 契約を保つため、他の接続・閾値パラメータと同じ「空文字は明示エラー」に揃える）。
/// ホスト名としての妥当性・ループバック制約自体は [`build_ollama_client`]
/// （`OllamaConfig::with_host`）が引き続き検証する。
pub fn parse_host(raw: &str, var_name: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{var_name} must not be empty"));
    }
    Ok(trimmed.to_string())
}

/// ティア別接続設定から [`OllamaClient`] を構築する（対話ティア・高精度ティアで
/// host/port は共有し、モデル名のみ分ける想定。`OllamaConfig::with_host` の
/// ループバック検証をそのまま経由する——本モジュールでは接続先の安全性検証を
/// 複製しない。`query_planner.rs` モジュールドキュメント「SSRF 対策」参照）。
pub fn build_ollama_client(host: &str, port: u16, model: &str) -> Result<OllamaClient, PlanError> {
    let config = OllamaConfig::new(model).with_host(host)?.with_port(port);
    Ok(OllamaClient::new(config))
}

/// LLM 不正応答（`PlanError::InvalidResponse`）試行の除外数上限の既定値
/// （TASK-116・Issue #316「設計判断」節。spec 由来の数値基準ではなく、本リポ
/// 独自の実装既定値——`query-tiering-criteria.md` の判定基準と同じ位置づけ）。
/// 計測回数（`MeasurementConfig::new` へ渡す固定値 30。`tier_latency_bench.rs`
/// 参照）の 10% を採用する。
pub const DEFAULT_MAX_INVALID_RESPONSE_TRIALS: u32 = 3;

/// `BENCH_TIER_MAX_INVALID_RESPONSE_TRIALS`（任意 env）の生文字列を解析する。
/// 未設定時は呼び出し元が [`DEFAULT_MAX_INVALID_RESPONSE_TRIALS`] を使う（本関数
/// へは非 `None` の値のみを渡す想定）。空文字・非整数・負は `Err`（fail-closed）。
/// `0` は「除外を許容しない」設定として明示的に許容する。
pub fn parse_max_invalid_response_trials(raw: &str, var_name: &str) -> Result<u32, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{var_name} must not be empty"));
    }
    trimmed
        .parse::<u32>()
        .map_err(|_| format!("{var_name} must be a non-negative integer"))
}

/// [`engine::core::CoreError`] を [`TrialFailure`] へ分類する（TASK-116・
/// Issue #316）。`plan_query_with_classification`（ティア別展開の追加処理時間
/// 計測、PLAN-4）が返すエラーの分類に使う。
///
/// 除外対象は `QueryPlanning(PlanError::InvalidResponse)` のみ。`Timeout`・
/// `Unavailable` 等の他 `PlanError` variant・他 `CoreError` variant はすべて
/// `Fatal`（除外すると p95 判定が fail-open になるため。設計判断 1 参照）。
pub fn classify_core_error(err: &CoreError) -> TrialFailure {
    match err {
        CoreError::QueryPlanning(PlanError::InvalidResponse) => TrialFailure::Excluded,
        _ => TrialFailure::Fatal,
    }
}

/// [`engine::sql::allowlist::SqlSurfaceError`] を [`TrialFailure`] へ分類する
/// （TASK-116・Issue #316）。`execute_sql`（`USING PLAN` 経由のティア別
/// エンドツーエンド計測、PLAN-6/7）が返すエラーの分類に使う。
///
/// `USING PLAN` 経路の LLM 呼び出し失敗は `core.rs::plan_using_plan_expansion`
/// が `SqlSurfaceError::Internal { detail: format!("USING PLAN query expansion
/// failed: {CoreError}") }` へ丸め込む（`sql/using_plan.rs` モジュール
/// ドキュメント「プランナー未注入・埋め込み未注入・LLM 応答異常は呼び出し元が
/// 既存分類（`XX000`・`SqlSurfaceError::Internal`）のみで拒否」）ため、新規
/// `wire_code` 分類は追加せず、`detail` 文字列に `PlanError::InvalidResponse`
/// の `Display` 文言が部分文字列として含まれるかで判定する（ハードコード
/// せず `PlanError::InvalidResponse.to_string()` から動的に構成する。
/// `EmbedError::InvalidResponse` の `Display` 文言とは重ならないため、
/// 埋め込みサービス側の不正応答を誤って除外することはない）。
pub fn classify_sql_error(err: &SqlSurfaceError) -> TrialFailure {
    if let SqlSurfaceError::Internal { detail } = err {
        let needle = PlanError::InvalidResponse.to_string();
        if detail.contains(&needle) {
            return TrialFailure::Excluded;
        }
    }
    TrialFailure::Fatal
}

/// TASK-116 の受け入れ判定（対象ビヘイビア PLAN-4, PLAN-6, PLAN-7）に使う上限値。
#[derive(Debug, Clone, Copy)]
pub struct TierThresholds {
    pub dialogue_expansion_max_p95: Duration,
    pub dialogue_e2e_max_p95: Duration,
    pub precision_expansion_max_p95: Duration,
    pub precision_e2e_max_p95: Duration,
    /// LLM 不正応答試行の除外数上限（段ごと共通。TASK-116・Issue #316）。
    /// 実測経路では [`super::protocol::run_fallible`] が計測時点で上限超過を
    /// 先に検知し打ち切るが、`judge` 単独呼び出し経路の fail-closed 性も
    /// この値が担う（[`TierJudgment::invalid_response_ok`] のドキュメント参照）。
    pub max_invalid_response_trials: u32,
}

/// TASK-116 の実測サンプルと routing 実証結果。
pub struct TierSamples {
    pub dialogue_expansion: Vec<Duration>,
    pub dialogue_e2e: Vec<Duration>,
    pub precision_expansion: Vec<Duration>,
    pub precision_e2e: Vec<Duration>,
    /// [`DIALOGUE_QUESTION`] の実測ティアが [`engine::tiering::Tier::Dialogue`] と
    /// 一致したか（誤ったティアのモデルで基準判定する false green/red を防ぐ。
    /// 計画「設計方針 3」参照）。
    pub dialogue_routing_matched: bool,
    /// [`PRECISION_QUESTION`] の実測ティアが [`engine::tiering::Tier::HighPrecision`]
    /// と一致したか。
    pub precision_routing_matched: bool,
    /// 4 段それぞれの計測フェーズで `Excluded`（LLM 不正応答）と分類された
    /// 試行数（`super::protocol::FallibleMeasurement::measured_excluded`）。
    /// TASK-116・Issue #316。
    pub dialogue_expansion_invalid_responses: u32,
    pub dialogue_e2e_invalid_responses: u32,
    pub precision_expansion_invalid_responses: u32,
    pub precision_e2e_invalid_responses: u32,
}

/// TASK-116 の受け入れ判定結果。
#[derive(Debug, Clone, Copy)]
pub struct TierJudgment {
    pub dialogue_expansion_p95: Duration,
    pub dialogue_expansion_ok: bool,
    pub dialogue_e2e_p95: Duration,
    pub dialogue_e2e_ok: bool,
    pub precision_expansion_p95: Duration,
    pub precision_expansion_ok: bool,
    pub precision_e2e_p95: Duration,
    pub precision_e2e_ok: bool,
    pub dialogue_routing_matched: bool,
    pub precision_routing_matched: bool,
    /// 4 段の除外数がいずれも [`TierThresholds::max_invalid_response_trials`]
    /// 以内だったか（TASK-116・Issue #316）。実測経路（`tier_latency_bench.rs`）
    /// では `super::protocol::run_fallible` が計測時点で上限超過を検知し
    /// `BenchError::ExcludedTrialsExceeded` として判定到達前に打ち切るため、
    /// `judge` に到達する時点では常に `true` になる。一方 `judge` は
    /// `TierSamples` を直接構築して単独で呼び出す経路（本モジュールのテスト等）
    /// でも成立する契約であり、判定 API 自体を fail-closed に保つため
    /// `all_passed()` の AND 条件に含める（PR #329 codex-review P2 指摘）。
    pub invalid_response_ok: bool,
}

impl TierJudgment {
    /// すべての判定・routing 検証・除外数上限チェックが通ったか（合否の単一
    /// 集約点。`tier_latency_bench.rs` はこの値のみを最終 pass/fail に使う）。
    ///
    /// `invalid_response_ok` を含める（TASK-116・Issue #316）。実測経路では
    /// `super::protocol::run_fallible` が計測時点で上限超過を先に検知し打ち切る
    /// ため冗長になるが、`judge` は `TierSamples` を直接構築して呼び出すことも
    /// できる公開 API であり、`all_passed()` から除外すると呼び出し元によっては
    /// 除外上限超過を見逃す fail-open な判定になる（PR #329 codex-review P2 指摘）。
    pub fn all_passed(&self) -> bool {
        self.dialogue_expansion_ok
            && self.dialogue_e2e_ok
            && self.precision_expansion_ok
            && self.precision_e2e_ok
            && self.dialogue_routing_matched
            && self.precision_routing_matched
            && self.invalid_response_ok
    }
}

/// [`TierSamples`]・[`TierThresholds`] から [`TierJudgment`] を算出する（時間非依存。
/// p95 抽出・上限判定は [`super::accept`] の既存関数をそのまま再利用し複製しない）。
/// いずれかのサンプル列が空なら `Err`（fail-closed。`accept::p95_from_samples` と
/// 同一契約）。
pub fn judge(
    samples: &TierSamples,
    thresholds: &TierThresholds,
) -> Result<TierJudgment, BenchError> {
    let dialogue_expansion_p95 = p95_from_samples(&samples.dialogue_expansion)?;
    let dialogue_e2e_p95 = p95_from_samples(&samples.dialogue_e2e)?;
    let precision_expansion_p95 = p95_from_samples(&samples.precision_expansion)?;
    let precision_e2e_p95 = p95_from_samples(&samples.precision_e2e)?;

    let invalid_response_ok = samples.dialogue_expansion_invalid_responses
        <= thresholds.max_invalid_response_trials
        && samples.dialogue_e2e_invalid_responses <= thresholds.max_invalid_response_trials
        && samples.precision_expansion_invalid_responses <= thresholds.max_invalid_response_trials
        && samples.precision_e2e_invalid_responses <= thresholds.max_invalid_response_trials;

    Ok(TierJudgment {
        dialogue_expansion_p95,
        dialogue_expansion_ok: check_p95_within_limit(
            dialogue_expansion_p95,
            thresholds.dialogue_expansion_max_p95,
        ),
        dialogue_e2e_p95,
        dialogue_e2e_ok: check_p95_within_limit(dialogue_e2e_p95, thresholds.dialogue_e2e_max_p95),
        precision_expansion_p95,
        precision_expansion_ok: check_p95_within_limit(
            precision_expansion_p95,
            thresholds.precision_expansion_max_p95,
        ),
        precision_e2e_p95,
        precision_e2e_ok: check_p95_within_limit(
            precision_e2e_p95,
            thresholds.precision_e2e_max_p95,
        ),
        dialogue_routing_matched: samples.dialogue_routing_matched,
        precision_routing_matched: samples.precision_routing_matched,
        invalid_response_ok,
    })
}
