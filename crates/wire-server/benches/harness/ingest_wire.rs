//! 単文 `INSERT` の wire 往復内訳ベンチ（Issue #484。親 Issue #483・
//! `crates/engine/benches/ingest_profile_bench.rs`（`BENCH_INGEST_PROFILE_MODE=
//! single`）の engine 内部段〔P0/E0/S0/I1〜I8〕を補い、wire プロトコル層自体の
//! 寄与を切り分ける）の時間非依存ロジック。
//!
//! `ingest_wire_profile_bench.rs`（実測本体・時間依存）と
//! `tests/ingest_wire_profile_accept.rs`（`make ci` 対象の回帰テスト）の双方から
//! `#[path]` で取り込む。統計量（min-of-N・median-of-N・帯判定）は
//! `harness::knn_wire`（Issue #463）の純関数をそのまま再利用し、本モジュールは
//! 本ベンチ固有の env パース（rows・rounds・可分性検証）と rows/s 換算のみを持つ
//! （重複実装しない）。

/// 本モジュールのエラー型。`harness::knn_wire::KnnWireError` とは対象の env が
/// 異なるため独立させる（`ingest_profile.rs::IngestProfileError` と同一の
/// 分離方針）。
#[derive(Debug, Clone, PartialEq)]
pub enum IngestWireError {
    /// `GITHUB_ACTIONS` 実行環境下での起動を拒否した。
    RefusedUnderGitHubActions,
    /// env 変数が数値として解釈できない、または範囲外・可分性条件を満たさない。
    InvalidEnv { name: &'static str, reason: String },
    /// rows/s 換算の分母（統計文数・所要時間）が 0 だった。
    ZeroRows,
}

impl std::fmt::Display for IngestWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestWireError::RefusedUnderGitHubActions => write!(
                f,
                "ingest_wire_profile_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set)"
            ),
            IngestWireError::InvalidEnv { name, reason } => {
                write!(f, "invalid env {name}: {reason}")
            }
            IngestWireError::ZeroRows => write!(f, "cannot compute rows_per_sec for zero rows/elapsed"),
        }
    }
}

impl std::error::Error for IngestWireError {}

/// `GITHUB_ACTIONS` 環境下での起動を拒否する（`harness::knn_wire` と同一方針）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), IngestWireError> {
    if under_github_actions {
        Err(IngestWireError::RefusedUnderGitHubActions)
    } else {
        Ok(())
    }
}

/// `BENCH_INGEST_WIRE_ROWS` の既定・下限・上限（wire 経由の投入総数。
/// crossdb ベンチの既定行数 25,000 と揃える）。
pub const DEFAULT_ROWS: usize = 25_000;
pub const MIN_ROWS: usize = 5_000;
pub const MAX_ROWS: usize = 100_000;

/// `BENCH_INGEST_WIRE_ROUNDS` の既定・下限・上限
/// （`docs/design/benchmark-judgement-policy.md` §3「交互実行 min-of-N（N≥5）」）。
pub const DEFAULT_ROUNDS: u32 = 5;
pub const MIN_ROUNDS: u32 = 5;
pub const MAX_ROUNDS: u32 = 50;

/// `BENCH_INGEST_WIRE_ROWS` を fail-closed にパースする。未設定は
/// [`DEFAULT_ROWS`]。数値変換不能・範囲外は `Err`
/// （黙って既定へフォールバックしない。coding-rust.md「untrusted 入力の扱い」）。
pub fn parse_rows(raw: Option<&str>) -> Result<usize, IngestWireError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_ROWS);
    };
    let value: usize = raw
        .trim()
        .parse()
        .map_err(|_| IngestWireError::InvalidEnv {
            name: "BENCH_INGEST_WIRE_ROWS",
            reason: format!("{raw:?} is not a valid usize"),
        })?;
    if !(MIN_ROWS..=MAX_ROWS).contains(&value) {
        return Err(IngestWireError::InvalidEnv {
            name: "BENCH_INGEST_WIRE_ROWS",
            reason: format!("{value} out of range [{MIN_ROWS}, {MAX_ROWS}]"),
        });
    }
    Ok(value)
}

/// `BENCH_INGEST_WIRE_ROUNDS` を fail-closed にパースする。未設定は
/// [`DEFAULT_ROUNDS`]。
pub fn parse_rounds(raw: Option<&str>) -> Result<u32, IngestWireError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_ROUNDS);
    };
    let value: u32 = raw
        .trim()
        .parse()
        .map_err(|_| IngestWireError::InvalidEnv {
            name: "BENCH_INGEST_WIRE_ROUNDS",
            reason: format!("{raw:?} is not a valid u32"),
        })?;
    if !(MIN_ROUNDS..=MAX_ROUNDS).contains(&value) {
        return Err(IngestWireError::InvalidEnv {
            name: "BENCH_INGEST_WIRE_ROUNDS",
            reason: format!("{value} out of range [{MIN_ROUNDS}, {MAX_ROUNDS}]"),
        });
    }
    Ok(value)
}

/// `rows` が `rounds` で割り切れることを検証し、1 ラウンドあたりの文数を返す
/// （各ラウンドを同一形状で計測するための可分性契約。黙って切り捨てない）。
pub fn rows_per_round(rows: usize, rounds: u32) -> Result<usize, IngestWireError> {
    let rounds_usize = rounds as usize;
    if rounds_usize == 0 || !rows.is_multiple_of(rounds_usize) {
        return Err(IngestWireError::InvalidEnv {
            name: "BENCH_INGEST_WIRE_ROWS",
            reason: format!("{rows} is not evenly divisible by BENCH_INGEST_WIRE_ROUNDS={rounds}"),
        });
    }
    Ok(rows / rounds_usize)
}

/// 投入文数と所要時間から集計 rows/s を算出する
/// （`ingest_profile.rs::rows_per_sec` と同一契約の独立コピー。crossdb の
/// `ingest_single_stmt` と同じ単位で並記する）。
pub fn rows_per_sec(stmts: usize, elapsed: std::time::Duration) -> Result<f64, IngestWireError> {
    if stmts == 0 {
        return Err(IngestWireError::ZeroRows);
    }
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return Err(IngestWireError::ZeroRows);
    }
    Ok(stmts as f64 / secs)
}
