//! ティア別レイテンシ受け入れ基準の時間非依存ヘルパ（TASK-116。ポインタ:
//! `docs/spec/05-tasks.md` TASK-116・対象ビヘイビア: `docs/spec/04-behavior/
//! query-planning.md` PLAN-4, PLAN-6, PLAN-7）。
//!
//! `benches/tier_latency_bench.rs`（実測。時間依存・`make ci` 対象外）が
//! ティア別に計測した p95（(a) クエリ展開の追加処理時間＝PLAN-4、(b) 展開込み
//! エンドツーエンド＝PLAN-6/PLAN-7）を本モジュールの判定関数へ渡す。
//! `tests/tier_latency_accept.rs` が `#[path]` で本モジュールを取り込み、実測
//! タイマー・env に依存せず `cargo test`（`make ci` 対象）で回帰検証する
//! （`harness/accept.rs`・`sql_c1_bench.rs`/`c1_bench_accept.rs` と同一パターン）。
//!
//! 数値基準（p95 上限）そのものは spec が SSOT であり本ファイルにはハードコード
//! しない（`.claude/rules/spec-confidentiality.md`）。判定関数はいずれも呼び出し元
//! （`tier_latency_bench.rs`）から env 経由で注入された閾値を受け取るのみとする
//! （`harness::accept` と同一方針）。
//!
//! # 計測質問（ティア routing 実証。設計方針 3）
//!
//! [`DIALOGUE_QUESTION`]・[`PRECISION_QUESTION`] は
//! [`engine::tiering::TieringCriteria::default`] の判定優先順（パス様トークン >
//! 手掛かり語 > 辞書シンボル名一致、`tiering.rs::classify` ドキュメンテーション
//! コメント「優先順」参照）のうち、**対象テーブルの辞書スナップショット内容に
//! 依存しない**判定経路（パス拡張子一致・手掛かり語一致）のみで意図したティアへ
//! 決定的に分類されるよう選んである。これにより計測用コーパスの内容を変更しても
//! ルーティングの実証結果が揺れない（`tiering.rs` の
//! `path_extension_match_yields_direct`／`abstraction_cue_yields_high_precision`
//! と同一の判定経路）。

use std::time::Duration;

use super::accept::{check_p95_within_limit, p95_from_samples};
use super::rng::DeterministicRng;
use super::stats::BenchError;

use engine::query_planner::{OllamaClient, OllamaConfig, PlanError};

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

/// TASK-116 の 4 判定（対話ティア／高精度ティアそれぞれの展開追加処理時間 p95・
/// e2e p95）に使う上限値。
#[derive(Debug, Clone, Copy)]
pub struct TierThresholds {
    pub dialogue_expansion_max_p95: Duration,
    pub dialogue_e2e_max_p95: Duration,
    pub precision_expansion_max_p95: Duration,
    pub precision_e2e_max_p95: Duration,
}

/// 実測サンプル（対話ティア／高精度ティアそれぞれの展開追加処理時間・e2e）と
/// routing 実証結果（設計方針 3）。
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
}

/// TASK-116 の 4 判定結果。
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
}

impl TierJudgment {
    /// 4 判定・2 routing 検証すべてが通ったか（合否の単一集約点。`tier_latency_bench.rs`
    /// はこの値のみを最終 pass/fail に使う）。
    pub fn all_passed(&self) -> bool {
        self.dialogue_expansion_ok
            && self.dialogue_e2e_ok
            && self.precision_expansion_ok
            && self.precision_e2e_ok
            && self.dialogue_routing_matched
            && self.precision_routing_matched
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
    })
}
