//! `feature_bench.rs`（examples）・`knn_profile_bench.rs`（benches）が ANN opt-in
//! （Issue #403 B 案・`search_engine::hnsw_kind`）の有無・規模を測定条件として
//! 選べるようにする、依存を持たない純パース関数群（Issue #413）。
//!
//! `crates/engine/tests/fixtures/recall_engine.rs::RecallEngine::parse`（Recall
//! ゲート層 B の ANN opt-in・Issue #412）と同じ「未設定・空・既定トークンは既定へ、
//! それ以外の未知値は fail-closed で拒否」の語彙・判定方針を、計測 example・bench
//! 側の env 変数（`BENCH_FEATURE_ENGINE`／`BENCH_KNN_PROFILE_ENGINE`・
//! `BENCH_FEATURE_SCALE`・`BENCH_FEATURE_DIM`／`BENCH_KNN_PROFILE_DIM`〔Issue #466。
//! dim=768／1536 が採否の判別変数になり得るため横断ベンチの規模点へ dim を追加〕）
//! 向けに複製したもの。`ingest_profile_bench.rs::
//! read_env_var` と同じ理由（`VarError::NotUnicode` を「未設定」へ黙って合流させ
//! ない）で env 読み取りも本モジュールへ集約する。
//!
//! `super::` を参照しない純関数のみで構成する。`examples/feature_bench.rs` からは
//! `#[path = "../benches/harness/bench_engine.rs"] mod bench_engine;` で単一ファイル
//! を取り込み（`harness::` モジュールツリー全体は取り込まない）、
//! `benches/knn_profile_bench.rs` からは既存の `harness::bench_engine` 経由で使う。
//! `tests/bench_engine_accept.rs`（`cargo test`・`make ci` 対象）が本ファイルを
//! 独立に取り込み、パース関数の回帰を時間非依存に検証する。
//!
//! 単体テストは本ファイルへインラインで置かない（`harness/ab.rs::median_ratio`
//! ドキュメンテーションコメント参照。bench ターゲット〔`harness = false`〕の
//! コンパイルは `#[test]` 属性のみ除去され `#[cfg(test)]` ブロック自体は
//! コンパイルされてしまうため、`#[cfg(test)] mod tests { use super::*; ... }`
//! を置くと bench ビルド時に `use super::*` が unused import になる）。
//! テストは `tests/bench_engine_accept.rs` に集約する。

use std::env::VarError;

/// ベンチが構築する `EngineCore` の検索エンジン種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchEngine {
    /// 既定（`search_engine::default_engine()`）。
    BruteForce,
    /// ANN opt-in（`search_engine::hnsw_kind(HnswParams::default())`）。
    Hnsw,
    /// ANN opt-in・索引ノード f16 常駐（Issue #514・`hnsw::ResidentPrecision::F16`。
    /// `ValidatedHnswParams::new(HnswParams::default())?.with_resident_precision(F16)`
    /// で構築する。Issue #516 が f32 常駐（[`Self::Hnsw`]）との前後比較・常駐
    /// メモリ実測の対象として追加した。`tests/fixtures/recall_engine.rs::
    /// RecallEngine::HnswF16` と同じ構築経路・トークン語彙を踏襲する）。
    HnswF16,
}

impl BenchEngine {
    /// ゲート出力・JSON `meta` へ書き出すトークン（数値を含まない。
    /// `.claude/rules/spec-confidentiality.md` 準拠）。
    pub fn token(self) -> &'static str {
        match self {
            Self::BruteForce => "brute_force",
            Self::Hnsw => "hnsw",
            Self::HnswF16 => "hnsw_f16",
        }
    }
}

/// env 変数値の取得エラー。`fmt::Display` を実装し呼び出し元は `eprintln!` へ
/// そのまま渡して非 0 終了する（`feature_bench.rs::fail_bench`／
/// `knn_profile_bench.rs::fail_closed` と同じ fail-closed の入口を経由する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchEngineError {
    message: String,
}

impl std::fmt::Display for BenchEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BenchEngineError {}

fn err(message: impl Into<String>) -> BenchEngineError {
    BenchEngineError {
        message: message.into(),
    }
}

/// `std::env::var` を fail-closed に読む（`ingest_profile_bench.rs::read_env_var`
/// と同型）。未設定（`NotPresent`）は `Ok(None)`、非 UTF-8（`NotUnicode`）は
/// 明示的に `Err` とし、typo・環境破損を黙って既定値へ合流させない。
pub fn read_env_var(name: &'static str) -> Result<Option<String>, BenchEngineError> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(err(format!("{name} value is not valid UTF-8"))),
    }
}

/// `raw`（`read_env_var` が返した値。前後の空白は許容: GitHub Actions の
/// variable 展開が末尾改行を持ち込む経路への対応。`recall_engine.rs` と同方針）
/// から [`BenchEngine`] を解決する。未設定・空文字列・`"brute_force"` は
/// [`BenchEngine::BruteForce`]、`"hnsw"` は [`BenchEngine::Hnsw`]。それ以外は
/// fail-closed で拒否する（黙って既定へ倒すと、typo で ANN 測定が静かに
/// スキップされる事故を防げない）。
pub fn parse_engine(raw: Option<&str>) -> Result<BenchEngine, BenchEngineError> {
    match raw.map(str::trim) {
        None | Some("") | Some("brute_force") => Ok(BenchEngine::BruteForce),
        Some("hnsw") => Ok(BenchEngine::Hnsw),
        Some("hnsw_f16") => Ok(BenchEngine::HnswF16),
        Some(other) => Err(err(format!(
            "must be unset, \"brute_force\", \"hnsw\", or \"hnsw_f16\" (got {other:?})"
        ))),
    }
}

/// 汎用の fail-closed 真偽値パーサ（Issue #516。`knn_profile_bench.rs` の
/// `BENCH_KNN_PROFILE_HOT_ONLY`／`BENCH_KNN_PROFILE_INDEX_MEMORY` が使う）。
/// 未設定・空文字列・`"0"` は `false`、`"1"` は `true`。他の値（`"true"`・
/// `"yes"` 等）は typo が黙って既定へ倒れる事故を防ぐため拒否する
/// （`parse_engine`・`recall_engine.rs::RecallEngine::parse` と同じ「未知値は
/// fail-closed」方針）。
pub fn parse_flag(raw: Option<&str>) -> Result<bool, BenchEngineError> {
    match raw.map(str::trim) {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(err(format!(
            "must be unset, \"0\", or \"1\" (got {other:?})"
        ))),
    }
}

/// 行数スケール倍率の上限。`max_nodes / rows_per_scale_unit` の呼び出し元計算
/// （`feature_bench.rs`: `ROWS_A + ROWS_B` = 25,000 行/単位）から
/// `hnsw::MAX_HNSW_NODES`（1,000,000）を超えない最大倍率を渡す契約とし、本関数
/// 自体は汎用の bound 付き正整数パーサとする。
pub fn parse_scale(raw: Option<&str>, max: u64) -> Result<u64, BenchEngineError> {
    let trimmed = raw.map(str::trim);
    let value: u64 = match trimmed {
        None | Some("") => 1,
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| err(format!("must be a positive integer (got {s:?})")))?,
    };
    if value == 0 {
        return Err(err("must be >= 1 (got 0)"));
    }
    if value > max {
        return Err(err(format!("must be <= {max} (got {value})")));
    }
    Ok(value)
}

/// `feature_bench.rs`／`knn_profile_bench.rs` の既定次元数（Issue #466）。
/// `BENCH_FEATURE_DIM`／`BENCH_KNN_PROFILE_DIM` 未設定時の後方互換値。
pub const DEFAULT_BENCH_DIM: u32 = 128;

/// `parse_dim` が許容する次元数の上限（Issue #466）。`ingest_profile_bench.rs::
/// parse_bounded_env`（`BENCH_INGEST_PROFILE_DIM`・`1..=4096`）と同値を踏襲する。
/// `feature_bench` の最大規模（scale=4・`ROWS_A + ROWS_B` = 25,000 行/単位 × 4 ≒
/// 100,000 行）× dim 4,096 × 4 バイト（f32）≒ 1.6 GiB の arena 確保が上限の目安。
/// `storage::MAX_EMBEDDING_DIM`（65,536）まで許すと無制限確保に近い規模になり
/// ベンチ実行環境の OOM を招きうるため、計測用途として現実的な範囲に絞る。
pub const MAX_BENCH_DIM: u32 = 4_096;

/// `raw`（`read_env_var` が返した値）から次元数を解決する（Issue #466。
/// `parse_scale` と同じ契約: 前後の空白を許容し、未設定・空文字列は `default`、
/// それ以外は `1..=max` の範囲の正整数のみを受理する fail-closed パーサ）。
/// dim=768／1536 が Issue #365 の複数アキュムレータ化検討で採否の判別変数と
/// 判明したため、`feature_bench`・`knn_profile_bench` の dim を可変化する
/// 注入点として使う。
pub fn parse_dim(raw: Option<&str>, default: u32, max: u32) -> Result<u32, BenchEngineError> {
    let trimmed = raw.map(str::trim);
    let value: u32 = match trimmed {
        None | Some("") => return Ok(default),
        Some(s) => s
            .parse::<u32>()
            .map_err(|_| err(format!("must be a positive integer (got {s:?})")))?,
    };
    if value == 0 {
        return Err(err("must be >= 1 (got 0)"));
    }
    if value > max {
        return Err(err(format!("must be <= {max} (got {value})")));
    }
    Ok(value)
}

/// `knn_profile_bench.rs` の可視比率 × 行数スイープ（Issue #487）が
/// `BENCH_KNN_PROFILE_VISIBLE_RATIO` から読む可視比率の分母。`None` はスイープ
/// モード無効（既定の非スイープ経路）を表す。分子は常に 1 に固定する
/// （Issue #487 が測る「可視 1/N」の形状のみを対象とし、`2/5` のような任意比は
/// 受理しない——受理形状を絞ることで env の入力空間を単純にし、
/// `expected_arm`／`sql_c1::c1_where_statement` 側の `bucket` 列挙生成
/// （`b0..b{N-1}`）とも 1 対 1 に対応させる）。
pub fn parse_visible_ratio(
    raw: Option<&str>,
    max_denominator: u32,
) -> Result<Option<u32>, BenchEngineError> {
    let trimmed = raw.map(str::trim);
    let denominator: u32 = match trimmed {
        None | Some("") => return Ok(None),
        Some(s) => {
            let rest = s.strip_prefix("1/").ok_or_else(|| {
                err(format!(
                    "must be \"1/<N>\" with N a positive integer (got {s:?})"
                ))
            })?;
            rest.parse::<u32>().map_err(|_| {
                err(format!(
                    "must be \"1/<N>\" with N a positive integer (got {s:?})"
                ))
            })?
        }
    };
    if denominator == 0 {
        return Err(err("denominator must be >= 1 (got 0)"));
    }
    if denominator > max_denominator {
        return Err(err(format!(
            "denominator must be <= {max_denominator} (got {denominator})"
        )));
    }
    Ok(Some(denominator))
}

/// `BENCH_KNN_PROFILE_FULL_SCAN_RATIO` から `crate::hnsw::ValidatedHnswParams::
/// with_full_scan_ratio` へ渡す `(numerator, denominator)` を読む（Issue #487）。
/// `None` は既定値（`crate::hnsw::DEFAULT_FULL_SCAN_RATIO` = 1/10）を使うことを
/// 表す。受理形状は `<num>/<den>`（`den >= 1`・`num <= den`）——本モジュールは
/// `engine::` を import しない契約（モジュール冒頭コメント）のため、検証は
/// `ValidatedHnswParams::with_full_scan_ratio` と同じ不変条件をタプルの範囲で
/// 複製するに留め、実際の `Ratio` 構築・最終検証は呼び出し元（`knn_profile_bench.rs`。
/// `engine::` を import できる）に委ねる。
pub fn parse_full_scan_ratio(raw: Option<&str>) -> Result<Option<(u32, u32)>, BenchEngineError> {
    let trimmed = raw.map(str::trim);
    let s = match trimmed {
        None | Some("") => return Ok(None),
        Some(s) => s,
    };
    let (num_str, den_str) = s
        .split_once('/')
        .ok_or_else(|| err(format!("must be \"<num>/<den>\" (got {s:?})")))?;
    let numerator: u32 = num_str.parse().map_err(|_| {
        err(format!(
            "numerator must be a non-negative integer (got {num_str:?})"
        ))
    })?;
    let denominator: u32 = den_str.parse().map_err(|_| {
        err(format!(
            "denominator must be a positive integer (got {den_str:?})"
        ))
    })?;
    if denominator == 0 {
        return Err(err("denominator must be >= 1 (got 0)"));
    }
    if numerator > denominator {
        return Err(err(format!(
            "numerator must not exceed denominator (got {numerator}/{denominator})"
        )));
    }
    Ok(Some((numerator, denominator)))
}

/// `knn_profile_bench.rs` のスイープが、`sql::hnsw_cache::search_with_overlay`
/// の整数比較（`visible * den < index_len * num` なら plain scan）を、計測前に
/// 「この (可視行数, 索引ノード数, full_scan_ratio) では ANN と plain scan の
/// どちらが選ばれるはずか」を予測するラベル付け専用の複製（Issue #487）。
///
/// 実行時に実際に選ばれた経路は `HnswIndexCacheStats`（`subset_searches`・
/// `plain_scans`・`mask_splits_graph`・`masked_short` 等）からのみ確定できる
/// （マスク分断・結果不足等、比較だけでは分からない縮退経路があるため）。
/// 本関数は doc 表の「予測 arm」列のラベル付けに使い、実測の「観測 arm」列とは
/// 独立に扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedArm {
    /// `visible * den >= index_len * num`: マスク付き ANN 探索が選ばれるはず。
    AnnMasked,
    /// `visible * den < index_len * num`: 可視カーディナリティ比が閾値未満で
    /// plain scan が選ばれるはず。
    PlainScanRatio,
}

pub fn expected_arm(
    visible: u64,
    index_len: u64,
    full_scan_ratio: (u32, u32),
) -> Result<ExpectedArm, BenchEngineError> {
    let (num, den) = full_scan_ratio;
    if den == 0 {
        return Err(err("full_scan_ratio denominator must be >= 1 (got 0)"));
    }
    let lhs = visible
        .checked_mul(den as u64)
        .ok_or_else(|| err("overflow computing visible * full_scan_ratio.denominator"))?;
    let rhs = index_len
        .checked_mul(num as u64)
        .ok_or_else(|| err("overflow computing index_len * full_scan_ratio.numerator"))?;
    if lhs < rhs {
        Ok(ExpectedArm::PlainScanRatio)
    } else {
        Ok(ExpectedArm::AnnMasked)
    }
}
