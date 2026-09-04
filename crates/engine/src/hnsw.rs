//! HNSW（Hierarchical Navigable Small World）グラフの構築（TASK-132・対象ビヘイビア:
//! CORE-9・CORE-10。ポインタ: `docs/design/ann-index-adoption.md`「実装ガイド（B 案）」節）。
//!
//! 本モジュールの範囲は**グラフ構築（Algorithm 1〜4 相当）＋ ef-探索 top-k 検索
//! （Algorithm 5 相当。[`HnswIndex::search`]。#405 で追加）＋ 並列構築（#406）＋
//! `search_engine.rs` への `SearchEngineKind::Hnsw` 結線（[`provider`] サブモジュール、
//! #407）**。世代整合キャッシュ・RLS 統合・永続化はいずれも別タスク（#408〜#409）の
//! 担当であり、本モジュールは触れない（`provider` は本タスク時点、索引を保持せず
//! 全件 brute-force フォールバックする——詳細は `provider` モジュールドキュメント
//! 参照）。ADR（`docs/design/ann-index-adoption.md`）の「非契約的な実装詳細」区分に
//! 基づき、spec 側の確定を待たずに着手している。
//!
//! # ベクトルの所有方針（codex-review PR #430 P1 指摘への対応で変更）
//!
//! [`HnswIndex::build`] は `&[f32]`（`arena.rs::VectorArena` 等が所有する
//! row-major 連続バッファ）を借用してグラフ（隣接リスト・レベル・
//! エントリポイント）を構築するが、構築完了時にその内容を `Arc<[f32]>`
//! として 1 回だけコピーし [`HnswIndex`] 自身に**所有**させる。
//! [`HnswIndex::search`] はもはや `vectors` を引数に取らず、常にこの
//! 内部スナップショットを参照する——旧設計（呼び出し元がビルド後も
//! バッファを所有し続け、`search` へ毎回 `&[f32]` で貸し出す）では、
//! 呼び出し元が構築後にバッファを書き換える・行順を入れ替える・別の
//! バッファへ差し替えるといった事故を `search` 側が検出できるかは
//! 長さ照合とサンプリングに依存し、サンプリング対象外の位置への
//! 書き換えは正常入力として静かに受理されグラフと `vectors` の対応が
//! 崩れたまま誤った top-k を返しかねなかった（初出時の対応:
//! サンプリング・フィンガープリント照合。指摘: そのサンプリング自体が
//! 検出漏れの余地を残す）。`HnswIndex` が唯一の正本を所有する設計へ
//! 変更したことで、`search` に「別バッファが渡される」という入力の
//! クラス自体が存在しなくなり、照合ロジックなしに構造的に防げる。
//!
//! この設計変更により `build` 呼び出し 1 回あたり `n * dim * 4` バイトの
//! 追加コピーが恒久的に発生する（768 次元 × 100 万行で 3 GB 級。旧設計が
//! 避けていたコスト）。トレードオフとして受け入れた判断であり、#408 の
//! 世代整合キャッシュ・#406 の並列構築で `VectorArena` 側が最初から
//! `Arc<[f32]>` を持つ構成に変えられれば、このコピーは `Arc::clone`
//! （参照カウントの増分のみ）に縮退できる——`arena.rs::VectorArena` は
//! 本 Issue 時点で `vectors: Vec<f32>` のまま（`Arc` 化していない）ため、
//! この縮退は #408 側の設計課題として申し送る。
//!
//! # 距離カーネル
//!
//! `kernel::dot`（`isa.rs` の実行時検出 SIMD カーネルへの唯一の委譲経路）を使う。
//! スコアは内積で「大きいほど近い」（`kernel.rs::CandidateHit` と同じ規約。cosine
//! 距離は呼び出し元が正規化済みベクトルを渡すことで内積に一致させる既存契約を
//! 踏襲する）。ヒューリスティックの `d(e, q) < d(e, r)` は本モジュール内では
//! `dot(e, q) > dot(e, r)` と読み替える。
//!
//! # 順序規約
//!
//! 候補ヒープの同点タイブレークは `kernel.rs::MinHeapItem` と同じ「スコア
//! `total_cmp` 降順・同点は id（ノード番号）昇順」を踏襲する。`f32` は全順序を
//! 持たないため `total_cmp` を使い、非有限値は構築入力の段で拒否する
//! （[`HnswError::NonFiniteVector`]）ため探索段では常に有限値のみを比較する。
//! ソートは安定ソート（`sort_by`）のみを使う（`sort_unstable_by` 系は
//! `scripts/check_sort_determinism.sh` が禁止する）。入力ベクトルが有限
//! （`NonFiniteVector` 検証済み）でも、有限な大きな `f32` 同士の積・総和は
//! オーバーフロー（`Inf`）や `NaN` になり得るため、`dot` の呼び出し直後にも
//! 結果が有限か検証し、非有限なら `HnswError::NonFiniteScore` として拒否する
//! （codex-review #423 P1 指摘）。
//!
//! # レベル割当の決定性・非暗号 PRNG
//!
//! `assign_level` は本モジュール専用の xorshift64*（`benches/harness/rng.rs::
//! DeterministicRng` と同アルゴリズム。`src/` からは bench harness を参照できない
//! ため小さく複製する）を `seed` で初期化し、同一 `seed`・同一入力なら
//! 完全に同一のグラフを構築する。**非暗号 PRNG であり、鍵・トークン等の
//! セキュリティ用途に転用してはならない**（OWASP A02）。
//!
//! # 上限・untrusted 入力の扱い（coding-rust.md・security.md）
//!
//! `dim`・`vectors.len()` の整合、[`MAX_HNSW_NODES`]・[`MAX_LEVEL`] による上限、
//! `HnswParams::validate` による `m`・`ef_construction`・`ef_search` の上限
//! （[`MAX_M`]・[`MAX_EF`]）を構築前に検証する。オフセット計算は `checked_*`、
//! ノード id 変換は `u32::try_from` を使い、スライス添字は `get()` のみで
//! `unwrap`／`expect`／`[]` を使わない。`unsafe` は使わない。環境変数・feature flag
//! による経路上書きは設けない（CORE-12 踏襲）。

use std::collections::{BinaryHeap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use crate::kernel::dot;

mod parallel_build;
/// `kernel.rs::SearchProvider` への結線（Issue #407・`search_engine.rs::
/// SearchEngineKind::Hnsw` の構築先）。本タスク時点は全件 brute-force
/// フォールバック（詳細は `provider` モジュールドキュメント参照）。
pub mod provider;
pub use provider::HnswSearchProvider;

/// 次数上限の安全上限（DoS 防止。`HnswParams::validate` が `m` をこの値以下に
/// 制限する）。
pub const MAX_M: usize = 128;

/// `ef_construction`／`ef_search` の上限（`core.rs::MAX_SEARCH_K` と同値。
/// untrusted な呼び出し元がここを起点に無制限の候補集合を要求できないようにする）。
pub const MAX_EF: usize = 10_000;

/// 構築可能なノード数の上限（`arena::MAX_ARENA_ROWS` と同値。ノード id を `u32` で
/// 表現できることの裏付けでもある）。
pub const MAX_HNSW_NODES: usize = 1_000_000;

/// 層数の絶対上限（`assign_level` の結果をこの値でクランプする。理論上は対数
/// オーダーで極小確率の外れ値しか生じないが、上限を設けないと未検証の外れ値が
/// `Vec` のネストを無制限に増やしうるため fail-closed に固定する）。
pub const MAX_LEVEL: usize = 32;

/// 並列構築（[`HnswIndex::build_with_threads`]・[`HnswIndex::build_parallel`]。
/// Issue #406）で逐次に構築する先頭ノード数（qdrant 方式。孤立成分の発生を
/// 防ぐ。本リポ採用値・非規範）。`parallel_build` モジュールの並列フェーズは
/// この件数を超えるノードのみを対象にする。
pub const SEQUENTIAL_PREFIX_NODES: usize = 256;

/// 並列構築のスレッド数上限（`parallel_search::MAX_THREADS_PER_QUERY` と同値。
/// [`HnswIndex::build_parallel`] が決める並列度もこの上限でクランプされる）。
pub const MAX_BUILD_THREADS: usize = 16;

/// 整数比（`u32/u32`）。`f32` は `HnswParams` の `Copy + PartialEq + Eq` derive と
/// 両立しない（`f32` は `Eq` を実装しない）ため、Issue #401 の `REBUILD_DELTA_RATIO`
/// （`(u64, u64)` タプル）と同じ発想で構造体化した（Issue #409。`sql::hnsw_cache`
/// の可視カーディナリティ切替閾値 `ValidatedHnswParams::full_scan_ratio` に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    pub numerator: u32,
    pub denominator: u32,
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

/// HNSW 構築パラメータ。
///
/// 既定値（`M=16`／`ef_construction=100`／`ef_search=64`）は ADR 起票 Issue #403
/// に記載の本リポ採用値（非規範的な実装既定値。spec 側の確定値ではない）。
///
/// `#[non_exhaustive]` は付与しない（codex-review P1 指摘・Issue #409・PR #435）:
/// 既に公開済みの本構造体へ後付けで `#[non_exhaustive]` を付けると、外部クレートが
/// 既存フィールドで構築する構造体リテラル（構造体更新構文を使わないもの含む）が
/// コンパイル不能になり、それ自体が破壊的変更になる（`docs/design/
/// error-enum-non-exhaustive-policy.md` と同じ判断）。可視カーディナリティ切替の
/// 閾値比（Issue #409）は本構造体へフィールド追加せず、検証済みラッパー
/// [`ValidatedHnswParams`] の private フィールド（`full_scan_ratio`）として持たせる
/// ことで、本構造体の公開フィールド集合は変更しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    /// 挿入後に各ノードが層 1 以上で保持する隣接数の目安（層 0 は `2*m` まで許容する。
    /// Malkov & Yashunin 2016 の記法と同じ）。
    pub m: usize,
    /// 挿入時の貪欲探索の候補幅。
    pub ef_construction: usize,
    /// 探索時の候補幅（本モジュールでは構築後のパラメータ保持のみ。実際の探索は #405）。
    pub ef_search: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 100,
            ef_search: 64,
        }
    }
}

impl HnswParams {
    /// パラメータの範囲検証。`m < 2`（`m=1` だとレベル割当の `mL = 1/ln(m)` が
    /// 定義できない・次数上限が実用にならない）・`m > MAX_M`・`ef_construction == 0`・
    /// `ef_construction > MAX_EF`・`ef_search == 0`・`ef_search > MAX_EF` を拒否する。
    pub fn validate(&self) -> Result<(), HnswError> {
        if self.m < 2 {
            return Err(HnswError::InvalidParams {
                reason: "m must be >= 2",
            });
        }
        if self.m > MAX_M {
            return Err(HnswError::InvalidParams {
                reason: "m exceeds MAX_M",
            });
        }
        if self.ef_construction == 0 {
            return Err(HnswError::InvalidParams {
                reason: "ef_construction must be >= 1",
            });
        }
        if self.ef_construction > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef_construction exceeds MAX_EF",
            });
        }
        if self.ef_search == 0 {
            return Err(HnswError::InvalidParams {
                reason: "ef_search must be >= 1",
            });
        }
        if self.ef_search > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef_search exceeds MAX_EF",
            });
        }
        Ok(())
    }

    /// `m` だけを差し替えたコピーを返す（`HnswParams::default()` と組み合わせて
    /// 使うビルダー用アクセサ。構造体リテラル `..HnswParams::default()` と等価だが
    /// 呼び出し元の記述を短くする）。
    pub fn with_m(mut self, m: usize) -> Self {
        self.m = m;
        self
    }

    /// `ef_construction` だけを差し替えたコピーを返す（[`Self::with_m`] 参照）。
    pub fn with_ef_construction(mut self, ef_construction: usize) -> Self {
        self.ef_construction = ef_construction;
        self
    }

    /// `ef_search` だけを差し替えたコピーを返す（[`Self::with_m`] 参照）。
    pub fn with_ef_search(mut self, ef_search: usize) -> Self {
        self.ef_search = ef_search;
        self
    }
}

/// [`HnswParams::validate`] を通過済みであることを型で保証するラッパー
/// （codex-review P1 指摘・Issue #407・PR #433 追記）。
///
/// フィールドは private のため、[`Self::new`]（[`HnswParams::validate`] を必ず経由する）
/// 以外の経路では構築できない。`crate::search_engine::SearchEngineKind::Hnsw` の
/// payload をこの型にすることで、不正な `HnswParams` を保持した `SearchEngineKind`・
/// [`crate::hnsw::provider::HnswSearchProvider`] がそもそも型として存在しえなくなる
/// （実行時エラー分類の流用・偽装ではなく、型システムで到達不能にする）。
///
/// 可視カーディナリティ切替の閾値比 `full_scan_ratio`（Issue #409。既定 1/10。
/// `sql::hnsw_cache::HnswIndexCache` が「可視候補数 ÷ 索引ノード数」の比が
/// この値未満なら plain scan、以上ならマスク付き ANN 探索を選ぶ判定に使う）は
/// 本ラッパーの private フィールドとして持つ（codex-review P1 指摘・Issue #409・
/// PR #435 是正）。[`HnswParams`] へ直接フィールド追加すると、既に公開済みの
/// 同構造体を使う外部クレートの構造体リテラルを破壊する（`#[non_exhaustive]`
/// 後付けも同様に破壊的。`docs/design/error-enum-non-exhaustive-policy.md` と
/// 同じ判断）ため、`ValidatedHnswParams` は元々 [`Self::new`] 経由でしか構築
/// できない private フィールドの型であることを利用し、フィールド追加を非破壊に
/// 収める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedHnswParams {
    params: HnswParams,
    full_scan_ratio: Ratio,
}

/// [`ValidatedHnswParams`] の `full_scan_ratio` 既定値（1/10。Issue #409）。
const DEFAULT_FULL_SCAN_RATIO: Ratio = Ratio {
    numerator: 1,
    denominator: 10,
};

impl ValidatedHnswParams {
    /// `params` を [`HnswParams::validate`] で検証し、通過した場合のみ構築する。
    /// `full_scan_ratio` は既定値（[`DEFAULT_FULL_SCAN_RATIO`]）で初期化される。
    /// 差し替えたい場合は [`Self::with_full_scan_ratio`] を使う。
    pub fn new(params: HnswParams) -> Result<Self, HnswError> {
        params.validate()?;
        Ok(Self {
            params,
            full_scan_ratio: DEFAULT_FULL_SCAN_RATIO,
        })
    }

    /// 検証済みの内部値を返す（`m`／`ef_construction`／`ef_search` フィールドへの
    /// 読み取りアクセス用。書き込みは許さない＝再検証なしに値を変更できない）。
    pub fn get(&self) -> HnswParams {
        self.params
    }

    /// `full_scan_ratio` を返す（`sql::hnsw_cache::search_with_overlay` が可視
    /// カーディナリティ切替の判定に使う。Issue #409）。
    pub fn full_scan_ratio(&self) -> Ratio {
        self.full_scan_ratio
    }

    /// `full_scan_ratio` だけを差し替えたコピーを返す。`ratio.denominator == 0`・
    /// `ratio.numerator > ratio.denominator` は [`HnswError::InvalidParams`] として
    /// 拒否する（元 `HnswParams::validate` が担っていた検証をここへ移設。
    /// Issue #409・codex-review P1 是正・PR #435）。
    pub fn with_full_scan_ratio(mut self, ratio: Ratio) -> Result<Self, HnswError> {
        if ratio.denominator == 0 {
            return Err(HnswError::InvalidParams {
                reason: "full_scan_ratio denominator must be >= 1",
            });
        }
        if ratio.numerator > ratio.denominator {
            return Err(HnswError::InvalidParams {
                reason: "full_scan_ratio numerator must not exceed denominator",
            });
        }
        self.full_scan_ratio = ratio;
        Ok(self)
    }
}

impl std::ops::Deref for ValidatedHnswParams {
    type Target = HnswParams;
    fn deref(&self) -> &HnswParams {
        &self.params
    }
}

impl Default for ValidatedHnswParams {
    /// `HnswParams::default()` は本モジュールのテスト
    /// （`hnsw_default_params_pass_validation`／`search_engine.rs::
    /// hnsw_default_params_pass_validation`）で常に検証を通過することを固定済みの
    /// 定数のため、untrusted 入力経路ではなく `.expect` の使用が
    /// `coding-rust.md` の禁止規約（受信データ経路での `unwrap`/`expect` 禁止）に
    /// 抵触しない。
    fn default() -> Self {
        ValidatedHnswParams::new(HnswParams::default())
            .expect("HnswParams::default() is a fixed constant known to pass validate()")
    }
}

/// [`HnswIndex::build`] の失敗要因。`Display`／`std::error::Error` を実装し
/// ライブラリコードとして panic せず `Result` で契約する（coding-rust.md）。
/// ベクトル値そのものは含めない（テナント情報・行データを含まない索引という
/// 本モジュールの契約を、エラー経路でも壊さないため）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HnswError {
    /// `HnswParams::validate` が拒否した。
    InvalidParams { reason: &'static str },
    /// `dim == 0`、または `vectors.len()` が `dim` の整数倍でない。
    DimMismatch { dim: u32, len: usize },
    /// ノード数が [`MAX_HNSW_NODES`] を超える。
    TooManyNodes { nodes: usize },
    /// 入力ベクトルに NaN／Inf が含まれる（構築後に順序を壊す経路を作らないため
    /// 構築段で拒否する）。
    NonFiniteVector { node: usize },
    /// `dot` の計算結果が非有限（NaN／Inf）だった。各入力要素は
    /// `NonFiniteVector` 検証で有限であることを確認済みでも、有限な大きな
    /// `f32` 同士の積・その総和は `Inf`（オーバーフロー）や `NaN`（正負の
    /// `Inf` の加算）になり得る。この非有限スコアが `ScoredNode::cmp`・
    /// ヒープ・近傍選択へそのまま入ると「探索段では常に有限値のみを比較
    /// する」契約が破れ、順序が壊れたグラフを正常結果として返してしまう
    /// ため、`dot` を呼ぶ全経路（[`HnswIndex::score`]・
    /// [`HnswIndex::repair_reachability`]・
    /// [`HnswIndex::select_neighbors_heuristic`]・[`HnswIndex::shrink_links`]）
    /// で計算直後に検証し fail-closed で拒否する。
    NonFiniteScore { node: u32 },
    /// オフセット・容量計算が `usize`／`u32` の範囲を超えた。
    CapacityOverflow,
    /// [`HnswIndex::search`]（#405）のクエリベクトルの次元が索引の次元
    /// （[`HnswIndex::dim`]）と一致しない。
    QueryDimMismatch { expected: u32, found: usize },
    /// [`HnswIndex::search`] のクエリベクトルに NaN／Inf が含まれる。
    /// `kernel.rs::KernelError::NonFiniteQuery` と同じ理由（wire 経由の
    /// untrusted 入力を `total_cmp` の順序に委ねず事前拒否する）で、探索段の
    /// 入口で検証する。
    NonFiniteQuery,
    /// [`HnswIndex::build_with_threads`]／[`HnswIndex::build_parallel`]
    /// （Issue #406）の構築ワーカーが panic した、またはノード単位ロック
    /// （`parallel_build::BuildGraph::links`）・エントリポイントロック
    /// （`parallel_build::BuildGraph::entry`）が poison した。全ハンドルを
    /// join したうえで fail-closed に拒否し、部分的に結線された索引を
    /// `Ok` で返さない（モジュール冒頭「失敗契約」参照）。
    WorkerPanicked,
}

impl fmt::Display for HnswError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HnswError::InvalidParams { reason } => write!(f, "invalid HNSW params: {reason}"),
            HnswError::DimMismatch { dim, len } => {
                write!(
                    f,
                    "vector buffer length {len} is not a multiple of dim {dim}"
                )
            }
            HnswError::TooManyNodes { nodes } => {
                write!(
                    f,
                    "node count {nodes} exceeds MAX_HNSW_NODES ({MAX_HNSW_NODES})"
                )
            }
            HnswError::NonFiniteVector { node } => {
                write!(f, "vector for node {node} contains a non-finite value")
            }
            HnswError::NonFiniteScore { node } => {
                write!(f, "dot product score for node {node} is non-finite")
            }
            HnswError::CapacityOverflow => write!(f, "capacity computation overflowed"),
            HnswError::QueryDimMismatch { expected, found } => write!(
                f,
                "hnsw search query dim mismatch: expected={expected} found={found}"
            ),
            HnswError::NonFiniteQuery => write!(f, "hnsw search query contains non-finite value"),
            HnswError::WorkerPanicked => {
                write!(
                    f,
                    "hnsw parallel build worker panicked or a lock was poisoned"
                )
            }
        }
    }
}

impl std::error::Error for HnswError {}

/// 決定的シードの xorshift64* PRNG（`benches/harness/rng.rs::DeterministicRng` と
/// 同アルゴリズム。`src/` から bench harness を参照できないため独立に複製する）。
///
/// 非暗号 PRNG。レベル割当専用でありセキュリティ用途に転用しない（OWASP A02）。
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `(0.0, 1.0]` の単位区間に収まる f64 疑似乱数値を返す（`0` を除外するのは
    /// `assign_level` が `-ln(u)` を取るため。`0` だと `ln(0) = -inf` になり
    /// レベルが発散する）。
    fn next_open01(&mut self) -> f64 {
        // 53bit の仮数部精度で一様分布を作る（f64 の仮数部ビット数に合わせる）。
        let bits = self.next_u64() >> 11; // 53bit
        let u = (bits as f64) / (1u64 << 53) as f64;
        // u ∈ [0, 1) を (0, 1] へシフトする（1.0 - u なら u=0 のとき 1.0 になる）。
        1.0 - u
    }
}

/// レベル割当（Malkov & Yashunin 2016 Algorithm 1 の `l = floor(-ln(unif(0,1)) * mL)`）。
/// `mL = 1 / ln(m)`。結果は [`MAX_LEVEL`] でクランプする（`HnswParams::validate` が
/// `m >= 2` を保証するため `ln(m)` は必ず正の有限値になる）。
fn assign_level(rng: &mut DeterministicRng, m: usize) -> usize {
    let m_l = 1.0 / (m as f64).ln();
    let u = rng.next_open01();
    let level = (-u.ln() * m_l).floor();
    if !level.is_finite() || level <= 0.0 {
        0
    } else {
        (level as usize).min(MAX_LEVEL)
    }
}

/// 候補ヒープの要素。スコア降順・同点は id 昇順を「強い」とする順序を `Ord` に
/// 埋め込む（`kernel.rs::MinHeapItem` と同じ規約。構築入力は事前に非有限値を
/// 拒否済みのため `total_cmp` の呼び出しは常に有限値同士になる）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScoredNode {
    node: u32,
    score: f32,
}

impl Eq for ScoredNode {}

impl PartialOrd for ScoredNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then(other.node.cmp(&self.node))
    }
}

/// 1 ノード分のグラフ状態。`links[l]` が層 `l` の隣接リスト（`l` は `0..=level`）。
/// ベクトル本体は持たない（モジュール冒頭「ベクトルの所有方針」参照）。
#[derive(Debug)]
struct Node {
    level: usize,
    links: Vec<Vec<u32>>,
}

/// 構築済み HNSW グラフ。`build` 完了時に渡された `vectors` の内容を
/// `Arc<[f32]>` として所有する（モジュール冒頭「ベクトルの所有方針」節・
/// codex-review PR #430 P1 指摘対応）。`search` は呼び出し元からベクトルを
/// 受け取らず、常にこの不変スナップショットのみを参照するため、長さ・
/// 内容が食い違うバッファが渡されるという事故のクラス自体が存在しない。
#[derive(Debug)]
pub struct HnswIndex {
    params: HnswParams,
    dim: u32,
    nodes: Vec<Node>,
    entry_point: Option<u32>,
    /// `build` 時点の `vectors`（row-major・`len() == nodes.len() * dim`）の
    /// 不変スナップショット。`search` はこれを `node_vector` で参照する。
    vectors: Arc<[f32]>,
}

/// `search_layer` が呼び出しをまたいで再利用する visited 集合（世代カウンタ
/// 方式）。挿入ごとに新規の `Vec<bool>` を割り当てて毎回ゼロ初期化すると、
/// `build` は各挿入で少なくとも層 0 の `search_layer` を 1 回呼ぶため
/// Σ_{i=1..N} O(i) = O(N^2) の初期化コストが積み上がる（codex-review #423
/// P1 指摘）。本構造体は `epoch` 配列を挿入間で使い回し、リセットを
/// カウンタのインクリメントだけの O(1) にすることでこれを避ける。
/// `current` は `u64` とし、`build` 1 回あたりの呼び出し回数
/// （高々 `N * (MAX_LEVEL+1)` 程度）に対して十分な余裕を持たせ、桁あふれ
/// 処理そのものを不要にする（到達しない分岐を残さない）。
#[derive(Debug, Default)]
pub(crate) struct VisitedScratch {
    epoch: Vec<u64>,
    current: u64,
}

impl VisitedScratch {
    /// 次の呼び出しに備えてリセットする。`len` は呼び出し時点の
    /// `self.nodes.len()`（構築中は挿入のたびに増加するため、呼び出し
    /// ごとに現在値を渡す。使い回すバッファは伸長のみで縮めない）。
    fn reset(&mut self, len: usize) {
        if self.epoch.len() < len {
            self.epoch.resize(len, 0);
        }
        self.current += 1;
    }

    /// `id` を訪問済みとして記録する。戻り値は「今回のリセット以降で
    /// 既に訪問済みだったか」（`true`＝既訪問なのでスキップ、`false`＝
    /// 新規訪問なので処理を続行）。範囲外の `id` は `None`
    /// （呼び出し元は untrusted 添字アクセスをせず `continue` する）。
    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        let slot = self.epoch.get_mut(id)?;
        let already = *slot == self.current;
        *slot = self.current;
        Some(already)
    }
}

/// [`search_layer`](HnswIndex::search_layer) が visited 集合として要求する
/// 最小インターフェース（#405）。構築経路（[`VisitedScratch`]。世代カウンタ
/// 方式で挿入ごとの O(N) 初期化コストを避ける）と探索経路（[`VisitedBitmap`]。
/// 1 ノード 1 bit でクエリ間の長期保持スクラッチに適する）の 2 実装を同じ
/// `search_layer` から共有するための境界。選定理由の詳細は
/// `docs/design/hnsw-search.md` 参照。
pub(crate) trait VisitedSet {
    /// 呼び出しに先立ちリセットする。`len` は索引の現在のノード数。
    fn reset(&mut self, len: usize);
    /// `id` を訪問済みとして記録する。戻り値・範囲外時の扱いは各実装の
    /// `mark_visited` に合わせる（`Some(既訪問か)`／範囲外は `None`）。
    fn mark_visited(&mut self, id: usize) -> Option<bool>;
}

impl VisitedSet for VisitedScratch {
    fn reset(&mut self, len: usize) {
        VisitedScratch::reset(self, len);
    }

    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        VisitedScratch::mark_visited(self, id)
    }
}

/// [`HnswIndex::search`]（#405）専用の visited 集合（1 ノード 1 bit の
/// ビットマップ方式）。[`VisitedScratch`] の世代カウンタ方式は構築経路の
/// O(N^2) 初期化回避を目的に導入されたものだが、探索経路はクエリごとに
/// 呼ばれ [`HnswSearchScratch`] としてスレッドごとに長期保持される想定のため、
/// メモリ効率（epoch 方式の 8 分の 1）を優先してビットマップを採用する
/// （選定理由の詳細は `docs/design/hnsw-search.md` 参照）。
#[derive(Debug, Default)]
struct VisitedBitmap {
    words: Vec<u64>,
}

impl VisitedBitmap {
    /// `len` ノード分を保持できるよう語数を伸長したうえで全ビットをクリア
    /// する（縮小はしない。呼び出し元が同一スクラッチを異なる索引規模へ
    /// 使い回す想定のため、再確保コストより多少の未使用メモリを許容する）。
    fn reset(&mut self, len: usize) {
        let words_needed = len.div_ceil(64);
        if self.words.len() < words_needed {
            self.words.resize(words_needed, 0);
        }
        for w in self.words.iter_mut() {
            *w = 0;
        }
    }

    /// `id` を訪問済みとして記録する。範囲外の `id` は `None`（呼び出し元は
    /// untrusted 添字アクセスをせず `continue` する。coding-rust.md）。
    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        let word_idx = id / 64;
        let bit_idx = id % 64;
        let word = self.words.get_mut(word_idx)?;
        let mask = 1u64 << bit_idx;
        let already = (*word & mask) != 0;
        *word |= mask;
        Some(already)
    }
}

impl VisitedSet for VisitedBitmap {
    fn reset(&mut self, len: usize) {
        VisitedBitmap::reset(self, len);
    }

    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        VisitedBitmap::mark_visited(self, id)
    }
}

/// [`HnswIndex::search_masked`]（Issue #409。Issue #431 是正で意味を拡張）が
/// 受け取る候補マスク。索引ノード（`build` 時に割り当てたノード番号）のうち、
/// 探索経路（貪欲降下・ビーム探索の候補集合展開）へ使ってよいもの＝結果集合へ
/// 含めてよいものを 1 ビット 1 ノードで表す（`docs/design/ann-index-adoption.md`
/// 「RLS／フィルタとの相互作用と折衷案」節の「非可視ノードを探索経路として通過
/// させる設計は不採用」という P0 安全条件により、候補・結果の受理は同一マスクで
/// 統一する）。テナント境界そのものではなく、`sql::hnsw_cache` がクエリ時点の
/// 候補集合（アリーナ）と索引ノードの差分を表現する装置。
///
/// `VisitedBitmap` と同型だが役割が異なる（訪問済み管理 ≠ 受理可否）ため
/// 別の型として持つ。
#[derive(Debug, Clone)]
pub struct NodeMask {
    words: Vec<u64>,
    len: usize,
}

impl NodeMask {
    /// `len` ノード分（すべて false）で初期化する。
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0u64; len.div_ceil(64)],
            len,
        }
    }

    /// `node` を受理対象に加える。範囲外は無視する（呼び出し元が索引の
    /// `len()` 以内の値のみを渡す契約。fail-closed に「何も起きない」側へ倒す）。
    pub fn set(&mut self, node: u32) {
        let idx = node as usize;
        if idx >= self.len {
            return;
        }
        let word_idx = idx / 64;
        let bit_idx = idx % 64;
        if let Some(word) = self.words.get_mut(word_idx) {
            *word |= 1u64 << bit_idx;
        }
    }

    /// `node` が受理対象か（範囲外は `false`。`unwrap`/`[]` を使わない）。
    pub fn get(&self, node: u32) -> bool {
        let idx = node as usize;
        if idx >= self.len {
            return false;
        }
        let word_idx = idx / 64;
        let bit_idx = idx % 64;
        match self.words.get(word_idx) {
            Some(word) => (*word & (1u64 << bit_idx)) != 0,
            None => false,
        }
    }

    /// マスクが表すノード総数（構築時の索引ノード数と一致する契約。
    /// [`HnswIndex::search_masked`] がこの値と `self.len()` の不一致を
    /// `HnswError::InvalidParams` として拒否する）。
    pub fn len(&self) -> usize {
        self.len
    }

    /// マスクが空（`len == 0`）か。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 受理対象に設定されているノード数。
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }
}

/// [`HnswIndex::search`]（#405）の呼び出しをまたいで再利用するスクラッチ。
/// 呼び出し元（将来の provider）がスレッドごとに 1 つ所有し、クエリごとに
/// 使い回す想定（モジュール冒頭「ベクトルの所有方針」節と同じ、確保コストを
/// 呼び出し元へ償却させる方針）。`Default` から始めれば初回呼び出しで索引
/// 規模に応じて自動的に伸長する。
#[derive(Debug, Default)]
pub struct HnswSearchScratch {
    visited: VisitedBitmap,
}

/// 層 `level` におけるノードの隣接リスト最大次数を返す（層 0 は `2*m`、
/// 層 1 以上は `m`。[`HnswIndex::max_degree`] の内部実装から共有する）。
fn max_degree_for(params: &HnswParams, level: usize) -> usize {
    if level == 0 {
        params.m.saturating_mul(2)
    } else {
        params.m
    }
}

impl HnswIndex {
    /// row-major 連続バッファ（`VectorArena::vectors()` と同レイアウト。
    /// `vectors[node * dim .. node * dim + dim]` が `node` 番目のベクトル）から
    /// 単一スレッドで構築する。
    ///
    /// 検証順序: パラメータ → 次元整合 → ノード数上限 → 非有限値。空入力
    /// （`vectors` が空）は空索引を返す（エラーにしない。呼び出し元が未挿入の
    /// テーブルへ構築を試みる自然なケースのため）。
    pub fn build(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        let dim_usize = dim as usize;
        let n = validate_build_input(&params, dim, vectors)?;

        // `vectors` の不変スナップショットを取り、以降 `search` はこれのみを
        // 参照する（モジュール冒頭「ベクトルの所有方針」節・codex-review PR
        // #430 P1 指摘対応。呼び出し元が構築後に借用元バッファを書き換えても
        // この Arc の中身は変化しない）。
        let owned_vectors: Arc<[f32]> = Arc::from(vectors);
        let mut index = HnswIndex {
            params,
            dim,
            nodes: Vec::with_capacity(n),
            entry_point: None,
            vectors: owned_vectors,
        };
        if n == 0 {
            return Ok(index);
        }

        let mut rng = DeterministicRng::new(seed);
        // 全挿入をまたいで使い回す visited スクラッチ（`VisitedScratch` 参照。
        // 挿入ごとに新規確保しないことで search_layer の初期化コストを
        // O(N^2) から O(N) 相当へ落とす）。
        let mut visited = VisitedScratch::default();
        // 挿入順はノード番号昇順に固定する（呼び出し元の入力順＝挿入順。決定性の
        // 唯一の自由度は `seed` によるレベル割当だけにする）。
        for node_idx in 0..n {
            let level = assign_level(&mut rng, params.m);
            let node_id = node_idx as u32; // n <= MAX_HNSW_NODES であることを上で検証済み
            index.insert_node(node_id, level, dim_usize, vectors, &mut visited)?;
        }

        index.repair_reachability(dim_usize, vectors)?;

        Ok(index)
    }

    /// [`build`](Self::build) と同じグラフを、要素単位ロック（ノードごとの
    /// `RwLock`）とエントリポイント更新のみの排他で並列構築する（Issue #406・
    /// 親 #402。pgvector `hnswbuild.c` のロック粒度設計・qdrant の逐次
    /// プレフィックス方式を参考にした。手法名のみ参照でコード転記はしない）。
    ///
    /// `threads == 1` または `n <= `[`SEQUENTIAL_PREFIX_NODES`] の場合は
    /// [`build`](Self::build) と完全に同一のグラフを返す（内部実装は
    /// `parallel_build::build_parallel_graph` に委譲せず [`build`](Self::build)
    /// をそのまま呼ぶ）。`threads >= 2` かつ `n > `[`SEQUENTIAL_PREFIX_NODES`]
    /// の場合、先頭 [`SEQUENTIAL_PREFIX_NODES`] 件は逐次挿入し、残りを
    /// `AtomicUsize` によるワークスティール方式で並列挿入する——挿入順が
    /// 非決定的になるため、構築されるグラフの**形状**は同一 `seed` でも
    /// run-to-run で変わり得る（レベル割当は並列フェーズ開始前に `seed` から
    /// 逐次確定するため不変。[`HnswIndex::search`] の決定性契約「同一索引・
    /// 同一クエリで再現」自体は不変。詳細は `docs/design/hnsw-parallel-build.md`
    /// 参照）。
    ///
    /// # エラー
    ///
    /// `threads == 0` または `threads > `[`MAX_BUILD_THREADS`] は
    /// [`HnswError::InvalidParams`]。構築ワーカーの panic・ロック poison は
    /// [`HnswError::WorkerPanicked`]（fail-closed。部分的に結線された索引を
    /// `Ok` で返さない）。
    pub fn build_with_threads(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
        threads: usize,
    ) -> Result<Self, HnswError> {
        if threads == 0 || threads > MAX_BUILD_THREADS {
            return Err(HnswError::InvalidParams {
                reason: "threads must be in 1..=MAX_BUILD_THREADS",
            });
        }
        let n = validate_build_input(&params, dim, vectors)?;
        if threads == 1 || n <= SEQUENTIAL_PREFIX_NODES {
            return Self::build(params, dim, vectors, seed);
        }
        parallel_build::build_parallel_graph(params, dim, vectors, seed, threads, n)
    }

    /// [`build_with_threads`](Self::build_with_threads) のスレッド数を
    /// [`crate::parallel_search::ParallelSearchProvider`] と同じ決定方法
    /// （`thread_count_for` による行数依存の並列度算出・プロセス全体の
    /// `WorkerBudgetGuard` による同時実行間の調停）で自動的に決める（Issue
    /// #406 要件 5。#407／#408 の既定結線先はこちら）。決定された並列度が
    /// 1 以下、またはグローバル予算を確保できなかった場合は
    /// [`build`](Self::build)（逐次・完全決定的）へ縮退する（`ParallelSearchProvider`
    /// と同じ「並列度を落とすだけで失敗させない」縮退規則）。
    ///
    /// `thread_count_for` は検索側の `MIN_ROWS_PER_THREAD`（1,024）を
    /// 1 スレッドあたりの担当行数の下限として使う（`available_parallelism`
    /// と `row_count / MIN_ROWS_PER_THREAD` の小さい方）ため、実質的な並列化
    /// 閾値は `MIN_ROWS_PER_THREAD * 2`（2,048。`available_parallelism >= 2`
    /// の環境で `desired >= 2` になる最小の `n`）であり、本メソッドが別途
    /// 課す [`SEQUENTIAL_PREFIX_NODES`]（256）より大きい。したがって
    /// `n` が 257..2047 の範囲では [`build_with_threads`](Self::
    /// build_with_threads) に明示的なスレッド数を渡せば並列化されるが、
    /// 本メソッドは検索側の閾値をそのまま流用する設計判断により逐次へ
    /// 縮退する（構築 1 ノードあたりのコストは検索 1 クエリの `dot` 計算
    /// より大幅に重いため、この閾値が構築にとって保守的すぎる可能性は
    /// 残るが、#407／#408 が実運用で結線する際に単一の決定方法を共有する
    /// 利点を優先した。見直しが必要になれば構築専用の閾値を別途持たせる）。
    pub fn build_parallel(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        let n = validate_build_input(&params, dim, vectors)?;
        let desired = crate::parallel_search::thread_count_for(n).min(MAX_BUILD_THREADS);
        if desired <= 1 || n <= SEQUENTIAL_PREFIX_NODES {
            return Self::build(params, dim, vectors, seed);
        }
        let guard = crate::parallel_search::WorkerBudgetGuard::acquire(desired);
        let granted = guard.granted();
        if granted <= 1 {
            drop(guard);
            return Self::build(params, dim, vectors, seed);
        }
        let result = Self::build_with_threads(params, dim, vectors, seed, granted);
        drop(guard);
        result
    }

    /// 全ノード挿入後の決定的な後始末パス。`insert_node`／`shrink_links` の
    /// `protect` 引数（呼び出し時点のみの保護）だけでは、後続ノードの挿入が
    /// 同じ近傍を再度枝刈りして到達不能ノードを生む残差ケースを閉じきれない
    /// （`docs/design/hnsw-graph-construction.md`「逆方向リンクの到達性保証」
    /// 節参照）。各層でエントリポイントから BFS
    /// し、到達できないノードが残っていれば、その層の到達済み集合中で最も
    /// 近い（`dot` が最大の）ノードへ双方向リンクを追加して修復する。
    ///
    /// # 2 フェーズ構成（計算量の上限。codex-review #423 P1 指摘）
    ///
    /// 検証の過程で、`shrink_links` によるヒューリスティック再選択を修復
    /// バッチとして複数ノードへ一括適用すると、ある未到達ノードを直すための
    /// 枝刈りが**無関係な別の**既存ノードの唯一の到達経路を巻き込んで壊し、
    /// 新たな到達不能ノードを生む whack-a-mole が起こり得ることが分かった。
    /// そのためフェーズ 1 は 1 ノードずつ確定的に修復し、直後に BFS を
    /// やり直して次の未到達ノード（新たに生まれたものを含む）を選ぶ
    /// ワークリスト方式を取る（`shrink_links`・`protect` つきで次数上限を
    /// 維持する厳密な修復）。この「全体 BFS ＋ 到達済み全ノードとの `dot`
    /// 計算」を伴う反復は、旧実装では上限を `member_count` の定数倍として
    /// おり、残差の多い入力（重複ベクトルが多い adversarial な入力等）では
    /// 反復回数・1 反復あたりのコストの双方が入力規模に比例して膨らみ、
    /// 少なくとも O(N^2) 相当となって `MAX_HNSW_NODES`（100 万）まで受理
    /// する構築 API 全体を計算量 DoS にさらしていた。フェーズ 1 の反復回数は
    /// 入力規模に依存しない小さな絶対上限 [`PRECISE_REPAIR_CAP`] に固定し、
    /// それを超えて残る未到達ノードはフェーズ 2 が閉じる。
    ///
    /// フェーズ 2 は残存ノードを id 昇順の**片方向チェーン**（`entry ->
    /// remaining[0] -> remaining[1] -> ...`）として連結するだけで残りを
    /// 閉じる。旧実装（全残存ノードをエントリポイントへ直結）は
    /// (1) `connect` の重複検査（`Vec::contains`）を経てエントリポイントの
    /// 隣接リストが残存ノード数に比例して伸び続け二次関数的コストになる
    /// （Bugbot 指摘）、(2) `shrink_links` を一切呼ばないため次数が
    /// `max_degree` を大幅に超え得る（codex-review #423 P1 指摘）、という
    /// 2 つの問題を持っていた。チェーン方式では各ノードが新たに得る次数は
    /// 高々 1（チェーンの「出発点」役を一度だけ務める）なので、
    /// `connect` 直後に `shrink_links` を掛けても 1 ノードあたり
    /// O(`max_degree`) に収まり、全体で O(remaining.len()) を保ったまま
    /// 次数上限も維持できる。`shrink_links` は「次数が上限を超えていれば
    /// `protect` を強制的に残しつつヒューリスティックで上限内へ再選択し、
    /// 超えていなければ何もしない」契約（同関数のドキュメンテーション
    /// コメント参照）を持つため、チェーンの起点を entry の現在の次数に
    /// 関わらず常に選べる（"余裕があるか" を事前に走査する必要がなく、
    /// 失敗しうる分岐も生まれない）。entry への `shrink_links` 適用が
    /// entry の既存リンクを 1 本犠牲にし得る点は、フェーズ 1 が到達済み
    /// 任意ノードへ毎回同じ `shrink_links` を適用しているのと同じ性質の
    /// リスクであり、新たに導入するものではない。フェーズ 1 の上限を
    /// 入力非依存の定数に保つことで、層あたりの
    /// 総コストは O(`PRECISE_REPAIR_CAP` * N + N) に収まり、`MAX_LEVEL` も
    /// 定数上限（32）であるため `HnswIndex::build` 全体では入力規模に対し
    /// ほぼ線形（N log N 契約の範囲内）に収まる。
    fn repair_reachability(&mut self, dim: usize, vectors: &[f32]) -> Result<(), HnswError> {
        /// フェーズ 1（`shrink_links` つきの厳密修復）の反復回数の絶対上限。
        /// 意図的に `member_count`／`n` に比例させない（比例させると入力
        /// 規模に応じて計算量 DoS を招く。上記モジュールコメント参照）。
        const PRECISE_REPAIR_CAP: usize = 64;

        let Some(entry) = self.entry_point else {
            return Ok(());
        };
        let Some(max_level) = self.level_of(entry) else {
            return Ok(());
        };
        for level in 0..=max_level {
            // フェーズ 1: 全体 BFS ＋ 到達済み全ノードとの `dot` 計算を伴う
            // 厳密な修復を `PRECISE_REPAIR_CAP` 回までに限定する。
            for _ in 0..PRECISE_REPAIR_CAP {
                let reachable = self.bfs_reachable(level, entry);
                let missing_node = (0..self.nodes.len() as u32)
                    .filter(|&n| self.level_of(n).map(|l| l >= level).unwrap_or(false))
                    .find(|n| !reachable.contains(n));
                let Some(node) = missing_node else {
                    break;
                };

                let node_vec = node_vector(vectors, dim, node)?;
                let mut best: Option<(u32, f32)> = None;
                for &candidate in &reachable {
                    let cand_vec = node_vector(vectors, dim, candidate)?;
                    let score = dot(node_vec, cand_vec);
                    if !score.is_finite() {
                        return Err(HnswError::NonFiniteScore { node: candidate });
                    }
                    // スコア降順・同点は id 昇順（モジュール冒頭の順序規約）。
                    // `reachable` は `HashSet<u32>` であり走査順はプロセス
                    // ごとに変わり得るハッシュ状態に依存するため、同点時に
                    // 単純な `>` 比較（最初に見つかった候補を保持）のままだと
                    // 同一 seed・同一入力でも修復先ノードが非決定的になる
                    // （codex-review #423 P1 指摘）。ここでスコア・id の複合
                    // 順序で明示的にタイブレークすることで、`reachable` の
                    // 走査順に関係なく常に同じ (score, id) の組が選ばれる。
                    let better = match best {
                        Some((best_node, best_score)) => match score.total_cmp(&best_score) {
                            std::cmp::Ordering::Greater => true,
                            std::cmp::Ordering::Equal => candidate < best_node,
                            std::cmp::Ordering::Less => false,
                        },
                        None => true,
                    };
                    if better {
                        best = Some((candidate, score));
                    }
                }
                // `reachable` は entry 自身を含むため必ず 1 件以上存在し、`best`
                // は常に `Some` になる（entry 自身が候補になり得る）。`None` は
                // `reachable` が空という到達不能な状態であり、fail-closed で
                // 何もしない（次の反復の BFS が変化のないまま同じ `node` を
                // 選び続けることになるが、フェーズ 1 の反復回数上限で有限に
                // 打ち切られ、フェーズ 2 が確定的に閉じる）。
                if let Some((target, _)) = best {
                    self.connect(node, target, level);
                    self.connect(target, node, level);
                    self.shrink_links(target, level, dim, vectors, node)?;
                    self.shrink_links(node, level, dim, vectors, target)?;
                }
            }

            // フェーズ 2: フェーズ 1 の絶対上限までで解消しなかった残りを、
            // 上記モジュールコメントのとおり id 昇順の片方向チェーンで
            // 確定的に閉じる。`remaining` は `0..len` の昇順フィルタなので
            // 既に決定的な id 昇順である。
            let reachable = self.bfs_reachable(level, entry);
            let remaining: Vec<u32> = (0..self.nodes.len() as u32)
                .filter(|&n| self.level_of(n).map(|l| l >= level).unwrap_or(false))
                .filter(|n| !reachable.contains(n))
                .collect();
            if let Some((&head, tail)) = remaining.split_first() {
                // チェーンは entry を起点にする: `entry -> head -> tail[0]
                // -> tail[1] -> ...`。`entry -> head` の 1 本だけが「既に
                // 到達済みのノード（entry 自身）」の隣接リストを変更する
                // 危険な結線であり、それ以降の `tail` への結線はすべて
                // 「直前まで未到達だった（＝他ノードの到達性に寄与しない）
                // orphan 同士」の結線なので安全（下記ループのコメント参照）。
                //
                // `entry` は他の到達済みノードへの唯一の到達経路を握って
                // いる場合があるため、`shrink_links(entry, ...)` が次数
                // 超過を解消する際に既存リンクを 1 本犠牲にすると、その
                // 犠牲先ノードが到達不能に戻り得る（Phase 1 はこれを
                // 「1 ノードずつ直して BFS をやり直す」ワークリスト方式で
                // 検知・再修復するが、Phase 2 は計算量上限のためそれをしない
                // 設計）。そのためここだけは特別に、`shrink_links` 適用前後
                // で entry の隣接集合を比較し、犠牲になったノード（あれば
                // 高々 1 件。`shrink_links` は次数超過分の 1 件しか削らない）
                // をチェーンの末尾へ追加で連結し直すことで、この 1 箇所の
                // リスクだけを O(1) の追加コストで確定的に解消する。
                let old_entry_links: Vec<u32> = self
                    .neighbors(level, entry)
                    .map(|links| links.to_vec())
                    .unwrap_or_default();
                self.connect(entry, head, level);
                self.shrink_links(entry, level, dim, vectors, head)?;
                let evicted = self.neighbors(level, entry).and_then(|new_links| {
                    old_entry_links
                        .into_iter()
                        .find(|old| !new_links.contains(old))
                });

                let mut prev = head;
                for &node in tail {
                    // `connect(prev, node, level)` は `prev` 自身の隣接
                    // リストのみを伸ばす（`node` 側は変化しない。モジュール
                    // 冒頭のノード表現）。`prev` はこの時点でまだ
                    // 未到達だったノード（またはチェーンの `head`）であり、
                    // 未到達ノードの「自身の」隣接リストは（BFS が一度も
                    // 辿っていないため）他ノードの到達性に寄与していない。
                    // よってここで `shrink_links(prev, ...)` が `prev` の
                    // 既存リンクを 1 本犠牲にしても安全。
                    self.connect(prev, node, level);
                    self.shrink_links(prev, level, dim, vectors, node)?;
                    prev = node;
                }

                // entry の shrink で犠牲になったノードがあれば、チェーンの
                // 末尾（= 直前まで未到達だった orphan）から結線し直す。
                // `prev` は orphan なので、ここでの `shrink_links` も上記と
                // 同じ理由で安全。
                if let Some(evicted) = evicted {
                    self.connect(prev, evicted, level);
                    self.shrink_links(prev, level, dim, vectors, evicted)?;
                }
            }
        }
        Ok(())
    }

    /// 層 `level` 上でノード `start` からリンクを辿って到達可能なノード集合を
    /// 返す（`repair_reachability` 専用の内部 BFS。`tests/hnsw.rs` は公開 API
    /// `neighbors` を使い同等の BFS を独立に実装して検証する）。
    fn bfs_reachable(&self, level: usize, start: u32) -> HashSet<u32> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(start);
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            if let Some(neighbors) = self.neighbors(level, node) {
                for &n in neighbors {
                    if visited.insert(n) {
                        queue.push_back(n);
                    }
                }
            }
        }
        visited
    }

    /// 1 ノードをグラフへ挿入する（Algorithm 1 相当）。探索段（不変参照のみ）と
    /// 結線段（可変）を関数分離しておくのは、#406（並列構築）が要素単位ロックへ
    /// 差し替える際にこの境界をそのまま流用できるようにするため。
    fn insert_node(
        &mut self,
        node_id: u32,
        level: usize,
        dim: usize,
        vectors: &[f32],
        visited: &mut VisitedScratch,
    ) -> Result<(), HnswError> {
        self.nodes.push(Node {
            level,
            links: vec![Vec::new(); level + 1],
        });

        let query = node_vector(vectors, dim, node_id)?;

        let (current_entry, top_level) = match self.entry_point {
            Some(ep) => {
                let ep_level = self.level_of(ep).unwrap_or(0);
                (ep, ep_level)
            }
            None => {
                self.entry_point = Some(node_id);
                return Ok(());
            }
        };

        // (i) 上位層（挿入ノードのレベルより上）を ef=1 の貪欲降下でたどり、
        // 挿入ノードのレベル直上での最近傍 1 件をエントリポイントとして絞り込む。
        let mut nearest = current_entry;
        if top_level > level {
            for l in ((level + 1)..=top_level).rev() {
                nearest = self.greedy_descend(nearest, query, l, dim, vectors)?;
            }
        }

        // (ii) 挿入ノードのレベル以下の各層で ef_construction 幅の探索 →
        // ヒューリスティック近傍選択 → 双方向リンク。
        let mut entry_candidates = vec![nearest];
        for l in (0..=level.min(top_level)).rev() {
            let candidates = self.search_layer(
                entry_candidates.clone(),
                query,
                self.params.ef_construction,
                l,
                dim,
                vectors,
                visited,
                None,
            )?;
            // 層 0 は次数上限が最大 2*m まで許容される（`shrink_links` が参照する
            // `max_degree_for` 側で扱う）が、新規ノード自身の選択本数は Algorithm 1
            // の記法どおり層を問わず常に `m` 本にする。
            let selected =
                self.select_neighbors_heuristic(&candidates, self.params.m, dim, vectors)?;

            for &neighbor in &selected {
                self.connect(node_id, neighbor, l);
                self.connect(neighbor, node_id, l);
                self.shrink_links(neighbor, l, dim, vectors, node_id)?;
            }

            entry_candidates = if candidates.is_empty() {
                vec![nearest]
            } else {
                candidates.iter().map(|c| c.node).collect()
            };
        }

        // (vi) 挿入ノードのレベルが現行最大層を超えるならエントリポイントを更新する。
        if level > top_level {
            self.entry_point = Some(node_id);
        }

        Ok(())
    }

    /// `ef=1` の貪欲降下（Algorithm 2 の `ef=1` 特殊形。上位層のナビゲーション用）。
    fn greedy_descend(
        &self,
        start: u32,
        query: &[f32],
        level: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<u32, HnswError> {
        let mut current = start;
        let mut current_best = ScoredNode {
            node: current,
            score: self.score(current, query, dim, vectors)?,
        };
        loop {
            let mut improved = false;
            if let Some(neighbors) = self.neighbors(level, current) {
                for &cand in neighbors {
                    let cand_scored = ScoredNode {
                        node: cand,
                        score: self.score(cand, query, dim, vectors)?,
                    };
                    // スコアのみでなく `ScoredNode::cmp`（スコア降順・同点は id 昇順）
                    // で比較する。同点時にモジュール冒頭の順序契約から外れないため。
                    if cand_scored > current_best {
                        current = cand;
                        current_best = cand_scored;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
        Ok(current)
    }

    /// [`Self::greedy_descend`] のマスク付き版（Issue #431・codex-review P0
    /// 是正）。構築経路（`insert_node`）はマスクを持たないため引き続き
    /// `greedy_descend` を直接使い、本関数は探索経路（[`Self::search_masked`]）
    /// 専用とする。
    ///
    /// `mask` が `Some` の場合、`mask` が受理しないノードのスコアは一切計算
    /// しない（`self.score` を呼ばない＝当該ノードのベクトルへ触れない）。
    /// `start` 自身が非受理なら降下を行わず `Ok(None)` を返す（呼び出し元は
    /// この層より下へ安全に進められる `nearest` を持たないため、探索全体を
    /// 打ち切る——`sql::hnsw_cache::search_with_overlay` の `masked_short` 経由で
    /// plain scan へ縮退する）。`start` が受理される場合は隣接ノードのうち
    /// 受理されるものだけを候補として貪欲降下する（非受理ノードは経路上の
    /// 中継点としても使わない）。
    ///
    /// `mask` が `None` の場合は常に受理したとみなし、`greedy_descend` と同じ
    /// 計算列（呼び出し順序込み）を辿るため、[`Self::search_masked`] が
    /// `mask: None` で呼んだときに [`Self::search`] とビット同一の結果を返す
    /// 契約（`crate::hnsw::tests::search_masked_none_matches_search`）を崩さない。
    fn greedy_descend_masked(
        &self,
        start: u32,
        query: &[f32],
        level: usize,
        dim: usize,
        vectors: &[f32],
        mask: Option<&NodeMask>,
    ) -> Result<Option<u32>, HnswError> {
        let is_ok = |node: u32| mask.map(|m| m.get(node)).unwrap_or(true);
        if !is_ok(start) {
            return Ok(None);
        }
        let mut current = start;
        let mut current_best = ScoredNode {
            node: current,
            score: self.score(current, query, dim, vectors)?,
        };
        loop {
            let mut improved = false;
            if let Some(neighbors) = self.neighbors(level, current) {
                for &cand in neighbors {
                    if !is_ok(cand) {
                        continue;
                    }
                    let cand_scored = ScoredNode {
                        node: cand,
                        score: self.score(cand, query, dim, vectors)?,
                    };
                    if cand_scored > current_best {
                        current = cand;
                        current_best = cand_scored;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
        Ok(Some(current))
    }

    /// [`Self::search_masked`] 用: 固定 entry point（[`Self::entry_point`]）が
    /// `mask` に非受理のときの代替探索起点選択（codex-review P2 指摘対応。
    /// `search_masked` ドキュメンテーションコメント参照）。全ノードを 1 度だけ
    /// 線形走査し、`mask` が受理するノードのうちレベル最大のもの（同点は id
    /// 最小。決定性を保つため）を返す。受理ノードが 1 つも無ければ `None`。
    ///
    /// entry point の非受理は探索経路上の他ノードの受理可否とは独立事象（RLS
    /// フィルタ・行削除で entry point だけが偶然除外される場合を含む）なので、
    /// この走査コストは fixed entry point 1 点の非受理だけで索引全体を次の
    /// 再構築まで brute-force へ縮退させる事故を避けるための対価として許容する
    /// （このパス自体、entry point が非受理の場合にのみ発生する）。
    fn find_alternate_entry(&self, mask: &NodeMask) -> Option<u32> {
        let mut best: Option<(u32, usize)> = None;
        for (idx, node) in self.nodes.iter().enumerate() {
            let Ok(id) = u32::try_from(idx) else {
                continue;
            };
            if !mask.get(id) {
                continue;
            }
            match best {
                Some((_, best_level)) if best_level >= node.level => {}
                _ => best = Some((id, node.level)),
            }
        }
        best.map(|(id, _)| id)
    }

    /// [`Self::search_masked`]・[`Self::is_mask_fully_reachable`] が共有する
    /// 探索起点選択（codex-review P2 指摘対応・PR #435 で分離）。固定
    /// entry point（[`Self::entry_point`]）が `mask` に受理されていればそれを、
    /// されていなければ [`Self::find_alternate_entry`] が選ぶ代替起点を返す。
    /// 受理ノードが 1 つも無ければ `None`。
    ///
    /// 到達可能性の検査（`is_mask_fully_reachable`）と実探索
    /// （`search_masked`）が異なる起点を使うと、検査が「探索が実際に使う
    /// 起点」とは無関係な結果を返しかねない（Cursor Bugbot High 指摘・
    /// PR #435）。本関数を両者から呼ぶことで起点選択を構造的に一致させる。
    fn search_entry_for_mask(&self, mask: &NodeMask) -> Option<u32> {
        match self.entry_point {
            Some(e) if mask.get(e) => Some(e),
            Some(_) => self.find_alternate_entry(mask),
            None => None,
        }
    }

    /// マスクの受理ノード全体が、[`Self::search_entry_for_mask`] が選ぶ単一
    /// entry point からの誘導部分グラフ（層 0 の隣接リストのみを辿る。
    /// ベクトルへのアクセス・スコア計算は一切行わない）から到達可能かどうかを
    /// 判定する（codex-review P2 指摘対応・PR #435）。
    ///
    /// マスクが複数の連結成分に分かれている場合、単一 entry point からの
    /// 探索は entry 側の成分にしか到達できず、`search_masked` の件数検査
    /// （`sql::hnsw_cache::search_with_overlay` の `masked_short`）だけでは
    /// 「別成分により近い受理ノードを一度も探索していない」ケースを
    /// 検出できない。本関数はその分断の有無を判定する。
    ///
    /// **クエリ毎には呼ばない契約**（Cursor Bugbot High 指摘・PR #435:
    /// 旧実装は `search_masked` からクエリ毎に層 0 の全受理ノード BFS を
    /// 呼んでおり、サブ線形探索の前に必ず O(V+E) の全域探索が発生していた）。
    /// 呼び出し元（`sql::hnsw_cache::Overlay::compute`）はマスクが変わる
    /// たび（世代が進むたび）1 回だけ本関数を呼び、結果を `Overlay` へ
    /// キャッシュする——`Overlay::compute` はマスク自体の構築で既に
    /// O(N) を要するため、本関数を追加で 1 回呼んでも漸近コストは増えない。
    ///
    /// 辿るのは実探索（[`Self::search_layer`]／[`Self::greedy_descend_masked`]）
    /// と同じ規約——非受理ノードは訪問済みにするのみで、その先の隣接ノードへ
    /// は一切辿らない（Issue #431 是正・`search_masked_does_not_traverse_
    /// through_a_rejected_bridge_node` で固定した契約と同じ）——に限定し、
    /// 実探索が構造的に到達できないノードを「到達可能」と誤判定しないように
    /// する。
    pub(crate) fn is_mask_fully_reachable(&self, mask: &NodeMask) -> bool {
        let target = mask.count_ones();
        if target == 0 {
            return true;
        }
        let Some(start) = self.search_entry_for_mask(mask) else {
            // 受理ノードが 1 つ以上あるのに起点が選べない防御的分岐
            // （`search_entry_for_mask` は受理ノードが 1 つでもあれば
            // `find_alternate_entry` で必ず見つけるはず）。fail-closed に
            // 「到達不能」とみなす。
            return false;
        };
        let mut visited = VisitedBitmap::default();
        self.accepted_reachable_count(start, mask, target, &mut visited) >= target
    }

    /// [`Self::is_mask_fully_reachable`] の BFS 本体。層 0 の隣接リストのみを
    /// 辿る BFS（辺の走査のみ）で `start` から到達可能な受理ノード数を数え、
    /// `target` 件に達した時点で早期終了する。`visited` は呼び出し元が
    /// 所有するスクラッチ（本関数専用に確保する使い捨て。呼び出し頻度が
    /// クエリ毎ではなく世代毎のため、`HnswSearchScratch` を共有する必要はない）。
    fn accepted_reachable_count(
        &self,
        start: u32,
        mask: &NodeMask,
        target: usize,
        visited: &mut VisitedBitmap,
    ) -> usize {
        visited.reset(self.nodes.len());
        if visited.mark_visited(start as usize) != Some(false) {
            return 0;
        }
        if !mask.get(start) {
            // 非受理ノードを起点に辿ることはない契約（呼び出し元は受理済みの
            // `nearest` のみを渡す）。防御的に「到達可能な受理ノード 0 件」を
            // 返す。
            return 0;
        }
        let mut count = 1usize;
        if count >= target {
            return count;
        }
        let mut queue: VecDeque<u32> = VecDeque::new();
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            let Some(neighbors) = self.neighbors(0, node) else {
                continue;
            };
            for &neighbor in neighbors {
                match visited.mark_visited(neighbor as usize) {
                    Some(false) => {}
                    _ => continue,
                }
                if !mask.get(neighbor) {
                    // 非受理ノードはここで打ち切り、この先へは辿らない
                    // （§本関数ドキュメンテーションコメント参照）。
                    continue;
                }
                count = count.saturating_add(1);
                if count >= target {
                    return count;
                }
                queue.push_back(neighbor);
            }
        }
        count
    }

    /// `search_layer`（Algorithm 2）。層 `level` 上で `entry_points` から出発し、
    /// 幅 `ef` の貪欲拡張探索を行い、`dot` 降順（同点は id 昇順）に並んだ最大 `ef`
    /// 件の候補を返す。`pub(crate)` にして #405（探索 API）が `ef_search` で再利用
    /// できるようにする。
    ///
    /// `visited` は呼び出し元（`insert_node`／`build` あるいはテスト）が
    /// 全呼び出しをまたいで所有する visited 集合（[`VisitedSet`]）。挿入ごとに
    /// 新規の `Vec<bool>` を確保しゼロ初期化していた旧実装は、`build` が
    /// 挿入ごとに少なくとも層 0 で本関数を呼ぶため合計 O(N^2) の初期化コスト
    /// になっていた（codex-review #423 P1 指摘）。構築経路は世代カウンタ方式の
    /// [`VisitedScratch`]、探索経路（#405・[`HnswIndex::search`]）はビットマップ
    /// 方式の [`VisitedBitmap`] を渡し、両者は `V: VisitedSet` のジェネリック
    /// パラメータとして本関数に共有される。各呼び出しの先頭で `reset` するだけ
    /// でよい契約は変わらない。
    /// `accept`（Issue #409。`sql::hnsw_cache::search_with_overlay` のマスク付き
    /// 探索から渡される）が `Some` の場合、`accept` が受理しないノードは候補ヒープ
    /// （`candidates`）へも一切積まない——訪問済みマークは付けるが、スコア計算
    /// （`self.score`。索引ノードのベクトルへのアクセスを伴う）自体を行わず、
    /// その隣接ノードへの探索も一切行わない（Issue #431・codex-review P0 是正。
    /// 旧実装は結果ヒープのみをマスクし候補ヒープは全ノードへ拡張していたため、
    /// 非受理ノード（削除・不可視化・キー変更で失効したノード）が探索経路
    /// （打ち切り判定の基準となる `results` の充足・以降の隣接探索）へ影響して
    /// いた。`docs/design/ann-index-adoption.md`「RLS／フィルタとの相互作用と
    /// 折衷案」節が定める「非可視ノードを探索経路として通過させる設計は不採用」
    /// という P0 安全条件に反していたため、`accept` を候補ヒープの受理判定にも
    /// 適用する形へ統一した）。`None`（既存の呼び出し元。`search`・構築経路）では
    /// 常に受理したのと同じ振る舞いになり、`search_layer` はビット同一の結果を
    /// 返す（`crate::hnsw::tests::search_masked_none_matches_search` 参照）。
    /// このマスクはテナント境界そのものではなく、クエリ時点の候補集合（アリーナ）
    /// と索引ノードの差分を表す装置だが、上記の是正後は非受理ノードのベクトルへ
    /// 一切触れない（実テナント境界は索引構築入力・呼び出し元の
    /// `provider_result_is_valid`・`RlsSafetyNet` の多層防御が担う。
    /// `sql::hnsw_cache` モジュールドキュメント参照）。
    #[allow(clippy::too_many_arguments)] // visited・accept 追加で 9 引数。既存の precision.rs・arena.rs と同じ方針で許容する。
    pub(crate) fn search_layer<V: VisitedSet>(
        &self,
        entry_points: Vec<u32>,
        query: &[f32],
        ef: usize,
        level: usize,
        dim: usize,
        vectors: &[f32],
        visited: &mut V,
        accept: Option<&NodeMask>,
    ) -> Result<Vec<ScoredNode>, HnswError> {
        visited.reset(self.nodes.len());
        let mut candidates: BinaryHeap<ScoredNode> = BinaryHeap::new();
        // 結果集合は最小ヒープとして扱いたいので `Reverse` で包む。
        let mut results: BinaryHeap<std::cmp::Reverse<ScoredNode>> = BinaryHeap::new();
        let is_accepted = |node: u32| accept.map(|m| m.get(node)).unwrap_or(true);

        for ep in entry_points {
            match visited.mark_visited(ep as usize) {
                Some(true) => continue,
                Some(false) => {}
                None => continue,
            }
            if !is_accepted(ep) {
                // 非受理（stale・不可視）ノードは候補ヒープへも一切積まない
                // （§関数ドキュメンテーションコメント参照。訪問済みマークのみ
                // 付けてスコア計算・以降の探索を行わない）。
                continue;
            }
            let score = self.score(ep, query, dim, vectors)?;
            let scored = ScoredNode { node: ep, score };
            candidates.push(scored);
            results.push(std::cmp::Reverse(scored));
        }

        while let Some(top_candidate) = candidates.pop() {
            // 候補集合の最良要素が、結果集合中の最悪要素より「厳密に」劣るなら
            // 打ち切る（Algorithm 2 の停止条件）。ここは `ScoredNode::cmp`（id
            // 昇順タイブレーク込みの複合順序）ではなく **スコアのみ**の比較に
            // 限定する。複合順序で判定すると、スコアが同点で id が大きいだけの
            // 候補まで「より遠い」と誤判定して打ち切ってしまい、その候補の
            // 未訪問隣接ノードがより近い可能性を探索し損なう（同点候補が
            // 生じやすい重複 embedding で顕在化。
            // `docs/design/hnsw-graph-construction.md`「`search_layer` の
            // 停止・受理判定: 順序規約の使い分け」節参照）。
            // id 順の複合順序は結果集合の内容（`results.pop()` によるヒープ
            // 内での追い出し順）・最終出力の安定ソートでのみ使い、探索を続ける
            // か否かの判定には使わない。
            if let Some(std::cmp::Reverse(worst)) = results.peek() {
                let strictly_farther =
                    top_candidate.score.total_cmp(&worst.score) == std::cmp::Ordering::Less;
                if results.len() >= ef && strictly_farther {
                    break;
                }
            }

            if let Some(neighbors) = self.neighbors(level, top_candidate.node) {
                for &neighbor in neighbors {
                    let already = match visited.mark_visited(neighbor as usize) {
                        Some(seen) => seen,
                        None => continue,
                    };
                    if already {
                        continue;
                    }
                    if !is_accepted(neighbor) {
                        // 非受理ノードは訪問済みにするのみで候補ヒープへは
                        // 積まない（Issue #431 是正。§関数ドキュメンテーション
                        // コメント参照）。スコア計算（このノードのベクトルへの
                        // アクセス）自体を行わず、この隣接ノード経由でのさらに
                        // 先の探索も一切行わない。
                        continue;
                    }
                    let neighbor_score = self.score(neighbor, query, dim, vectors)?;
                    let scored = ScoredNode {
                        node: neighbor,
                        score: neighbor_score,
                    };
                    // 打ち切り判定と同じ理由でスコアのみの比較に限定する
                    // （`scored` が `worst` とスコア同点なら、id 順の複合順序で
                    // 「劣る」と判定されても受理する）。`worst_ok` を満たす
                    // 隣接ノードは（上の `is_accepted` チェックを通過済みのため）
                    // 候補ヒープ・結果ヒープの双方へ積む。
                    let worst_ok = match results.peek() {
                        Some(std::cmp::Reverse(worst)) => {
                            results.len() < ef
                                || scored.score.total_cmp(&worst.score) != std::cmp::Ordering::Less
                        }
                        None => true,
                    };
                    if worst_ok {
                        candidates.push(scored);
                        results.push(std::cmp::Reverse(scored));
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }

        let mut out: Vec<ScoredNode> = results.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        Ok(out)
    }

    /// 近傍選択ヒューリスティック（Algorithm 4）。既定は `extend_candidates=false`・
    /// `keep_pruned_connections=true`（余った枠を枝刈り済み候補で埋め、次数を
    /// 確保する）。この既定の採用理由・#405 での見直し余地は
    /// `docs/design/hnsw-graph-construction.md` に記録する。`extend_candidates=true`
    /// 形（候補の隣接をさらに候補へ加える拡張）は実装しない（到達しない分岐は
    /// 検証されないままコードに残り将来のバグ源になるため。既定を有効化する
    /// 場合に別途実装する）。
    fn select_neighbors_heuristic(
        &self,
        candidates: &[ScoredNode],
        m: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<Vec<u32>, HnswError> {
        // 本体は `self` を一切参照しない純粋関数（`select_neighbors_heuristic_free`）
        // へ切り出し済み。並列構築（`parallel_build`。Issue #406）の
        // `BuildGraph` からも同じ実装を共有する。
        select_neighbors_heuristic_free(candidates, m, dim, vectors)
    }

    /// `from -> to` への単方向リンクを層 `level` へ追加する（重複・自己ループは
    /// 追加しない）。次数上限の適用は呼び出し元の [`Self::shrink_links`] が担う。
    fn connect(&mut self, from: u32, to: u32, level: usize) {
        if from == to {
            return;
        }
        let Some(node) = self.nodes.get_mut(from as usize) else {
            return;
        };
        let Some(links) = node.links.get_mut(level) else {
            return;
        };
        if !links.contains(&to) {
            links.push(to);
        }
    }

    /// `node` の層 `level` における隣接数が次数上限を超えていれば、その隣接集合
    /// 全体へヒューリスティック近傍選択を再適用して上限内へ縮退させる
    /// （Algorithm 1 の「次数上限超過時の再選択」段）。
    ///
    /// `protect` は、この呼び出し直前に `insert_node` が `node <-> protect` へ
    /// 張ったばかりの逆方向リンク先（新規挿入ノード自身）。ヒューリスティックが
    /// `protect` を枝刈りしてしまうと、新規ノードへの唯一の入口だった逆方向
    /// リンクが失われ、エントリポイントからの到達路が残らないまま孤立し得る
    /// （挿入ノードは自身の外向きリンクは持つが、探索はエントリポイントから
    /// 既存ノードの隣接リストを辿って到達するため入方向のリンクが要る）。
    /// ヒューリスティック選択後に `protect` が漏れて
    /// いれば、選択済み集合中で最もスコアが低い（＝末尾の）要素と差し替えて
    /// 強制的に残す。
    ///
    /// この保証は「`protect` の挿入時点で選ばれた各近傍が `protect` への逆方向
    /// リンクを保持する」ことのみを担保する insertion-time の不変条件であり、
    /// 後続の別ノード挿入がこれらの近傍を再度 `shrink_links` する際に `protect`
    /// が漏れる余地までは塞がない（グローバルな到達性の恒久保証ではない）。
    /// 全ノード挿入後に残るその残差ケースは `HnswIndex::build` 末尾の
    /// [`Self::repair_reachability`] が閉じる（この関数自体は呼ばない。
    /// 呼ぶと、他の未到達ノードを直すための枝刈りが無関係な第三のノードの
    /// 唯一の到達経路を巻き込んで壊す whack-a-mole が起こり得るため）。
    /// 詳細は `docs/design/hnsw-graph-construction.md`
    /// 「逆方向リンクの到達性保証」節参照。
    fn shrink_links(
        &mut self,
        node: u32,
        level: usize,
        dim: usize,
        vectors: &[f32],
        protect: u32,
    ) -> Result<(), HnswError> {
        let limit = max_degree_for(&self.params, level);
        let current_links: Vec<u32> = self
            .nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .cloned()
            .unwrap_or_default();
        // 縮退の計算本体（読み取り→再選択）は `compute_shrink` という純粋関数に
        // 切り出し済み。並列構築（`parallel_build`。Issue #406）は書き込みロック
        // 1 回の中で「現在のリンクを読む→`compute_shrink`→書き戻す」を原子的に
        // 行うことで同じ計算を共有する（`docs/design/hnsw-parallel-build.md`
        // 参照）。逐次経路（本メソッド）はロック不要のため読み→計算→書き込みを
        // そのまま `self.nodes` への 2 回のアクセスとして行う。
        if let Some(shrunk) = compute_shrink(&current_links, node, dim, vectors, limit, protect)? {
            if let Some(n) = self.nodes.get_mut(node as usize) {
                if let Some(links) = n.links.get_mut(level) {
                    *links = shrunk;
                }
            }
        }
        Ok(())
    }

    /// `dot(node, query)`。ノード id が範囲外／`vectors` が短すぎる場合は
    /// `NonFiniteVector` 相当ではなく、境界検証は [`node_vector`] が担う
    /// （呼び出し元は構築時に検証済みの id しか渡さない内部専用パス）。結果が
    /// 非有限（オーバーフロー・`NaN`）なら `NonFiniteScore` として拒否する
    /// （モジュール冒頭「順序規約」節参照）。
    fn score(
        &self,
        node: u32,
        query: &[f32],
        dim: usize,
        vectors: &[f32],
    ) -> Result<f32, HnswError> {
        // 並列構築（`parallel_build`）と共有する純粋関数へ委譲する。
        score_of(vectors, dim, node, query)
    }

    /// 構築済みパラメータを返す。
    pub fn params(&self) -> &HnswParams {
        &self.params
    }

    /// ベクトルの次元数（`build` に渡した `dim`）。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 構築済みノード数。
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// ノード数が 0 か。
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// `build` 時点の内部スナップショット（`self.vectors`）から `node` 番目の
    /// ベクトルを返す（Issue #408・`sql::hnsw_cache` 専用の公開アクセサ）。
    /// `node_vector` の公開版だが、こちらは範囲外時に `Err` ではなく `None` を
    /// 返す（呼び出し元がテーブル世代整合キャッシュの差分判定〔`Overlay::compute`〕
    /// で「索引済みノードが現在も存在するか」を確認する用途のため、専用の
    /// エラー型を経由させる必要がない）。
    pub fn vector(&self, node: u32) -> Option<&[f32]> {
        node_vector(&self.vectors, self.dim as usize, node).ok()
    }

    /// 索引本体（隣接リスト・複製ベクトル）の概算ヒープバイト量（Issue #408。
    /// `sql::hnsw_cache::HnswIndexCache` の容量判定・観測用統計が使う）。
    /// `self.vectors`（`build` 時に複製した `Arc<[f32]>`）＋各ノードの隣接
    /// リスト（層ごとの `Vec<u32>`）の `capacity()` を合算する。
    pub fn approx_heap_bytes(&self) -> usize {
        let vectors_bytes = self
            .vectors
            .len()
            .saturating_mul(std::mem::size_of::<f32>());
        let nodes_bytes: usize = self
            .nodes
            .iter()
            .map(|node| {
                let links_bytes: usize = node
                    .links
                    .iter()
                    .map(|l| {
                        l.capacity()
                            .saturating_mul(std::mem::size_of::<u32>())
                            .saturating_add(std::mem::size_of::<Vec<u32>>())
                    })
                    .fold(0usize, |acc, n| acc.saturating_add(n));
                links_bytes.saturating_add(std::mem::size_of::<Node>())
            })
            .fold(0usize, |acc, n| acc.saturating_add(n));
        vectors_bytes.saturating_add(nodes_bytes)
    }

    /// グラフ全体の最大層（エントリポイントのレベル）。空索引では `None`。
    pub fn max_level(&self) -> Option<usize> {
        self.entry_point.and_then(|ep| self.level_of(ep))
    }

    /// エントリポイントのノード id。空索引では `None`。
    pub fn entry_point(&self) -> Option<u32> {
        self.entry_point
    }

    /// ノード `node` が割り当てられたレベル。存在しないノードは `None`。
    pub fn level_of(&self, node: u32) -> Option<usize> {
        self.nodes.get(node as usize).map(|n| n.level)
    }

    /// 層 `level` におけるノード `node` の隣接リスト。存在しない層・ノードは
    /// `None`（ノードのレベルが `level` 未満の場合を含む）。
    pub fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]> {
        self.nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .map(|l| l.as_slice())
    }

    /// 層 `level` における最大次数（層 0 は `2*m`、層 1 以上は `m`）。テスト・
    /// `EXPLAIN`（#411 の担当範囲）が参照する想定の公開ヘルパ。
    pub fn max_degree(&self, level: usize) -> usize {
        max_degree_for(&self.params, level)
    }

    /// ef-探索 top-k 検索（Malkov & Yashunin 2016 Algorithm 5 相当。TASK-132・
    /// CORE-9・CORE-10。#405 の担当範囲）。上位層を `ef=1` の
    /// [`greedy_descend`](Self::greedy_descend) で降下したのち、層 0 を幅
    /// `ef.max(k)` で [`search_layer`](Self::search_layer) によりビーム探索し、
    /// 上位 `k` 件を返す。結果は `kernel.rs::CandidateHit` と同じ順序規約
    /// （スコア降順・同点は id 昇順）で、`id` は内部スナップショット
    /// （`self.vectors`）上のノード番号（0 始まり）を `u64` 化したもの。
    ///
    /// ベクトル本体は引数で受け取らず、[`build`](Self::build) 時に取得した
    /// 内部スナップショット（`self.vectors`。row-major 連続バッファ、
    /// `self.vectors.len() == self.len() * dim`。モジュール冒頭「ベクトルの
    /// 所有方針」節）のみを参照する。`scratch` は クエリをまたいで呼び出し元が
    /// 再利用する [`HnswSearchScratch`]。
    ///
    /// 決定性の保証範囲は「同一索引・同一クエリ・任意のスクラッチ状態で
    /// 結果が再現する」までであり、総当たり経路（`kernel.rs`）が持つ境界
    /// 同点グループの完全化までは保証しない（spec 側の規範化は #405 の
    /// 担当外。詳細は `docs/design/hnsw-search.md` 参照）。
    ///
    /// # エラー
    ///
    /// 検証順序は次のとおり（すべて fail-closed）: クエリ次元不一致
    /// （[`HnswError::QueryDimMismatch`]）→ クエリの非有限値
    /// （[`HnswError::NonFiniteQuery`]。`kernel.rs::KernelError::NonFiniteQuery`
    /// と同じ理由で `total_cmp` の順序に委ねず事前拒否する）→ `ef`／`k` の
    /// 上限超過（[`HnswError::InvalidParams`]。`MAX_EF` を上限に流用し、
    /// untrusted な呼び出し元が無制限の候補集合を要求できないようにする）。
    /// `k == 0` または空索引は空の `Ok(Vec::new())` を返す。ベクトル本体は
    /// `build` 時に取得した内部スナップショット（`self.vectors`）を使うため、
    /// 呼び出し元がベクトル集合を渡す経路が存在せず、長さ・内容の不一致
    /// エラーはそもそも構造的に発生しない（モジュール冒頭「ベクトルの
    /// 所有方針」節・codex-review PR #430 P1 指摘対応）。
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        scratch: &mut HnswSearchScratch,
    ) -> Result<Vec<crate::kernel::CandidateHit>, HnswError> {
        self.search_masked(query, k, ef, None, scratch)
    }

    /// [`Self::search`] のマスク付き版（Issue #409。Issue #431・codex-review P0
    /// 是正で上位層の貪欲降下も含めマスクを一貫適用する形へ変更。さらに
    /// codex-review P2 指摘対応で固定 entry point 自体が非受理の場合の代替
    /// 探索起点選択を追加）。`mask` が `Some` の場合、上位層の貪欲降下
    /// （[`Self::greedy_descend_masked`]）・層 0 のビーム探索
    /// （[`Self::search_layer`]）のいずれも `mask` を候補マスクとして渡し、
    /// `mask` が受理しないノード（削除・不可視化・キー変更で失効したノード）は
    /// 探索経路のどの段階でも一切スコア計算・探索の起点として使わない
    /// （`docs/design/ann-index-adoption.md`「RLS／フィルタとの相互作用と折衷案」
    /// 節の P0 安全条件）。
    ///
    /// 固定 entry point（[`Self::entry_point`]）自体が `mask` に非受理の場合、
    /// 直ちに空集合へ縮退せず、[`Self::search_entry_for_mask`] で受理ノードの
    /// 中から代替探索起点（受理ノードのうちレベル最大・同点は id 最小）を選び、
    /// その代替起点が存在する最上層から通常どおり降下する（可視カーディナリティ
    /// が `full_scan_ratio` 以上で ANN 経路を選ぶ設計—Issue #409—と整合させる
    /// ため、entry point 1 点の非受理だけで索引全体が次の再構築まで無効化される
    /// 事故を避ける）。受理ノードが 1 つも無い場合のみ空の結果を返し
    /// （`Ok(Vec::new())`）、呼び出し元（`sql::hnsw_cache::search_with_overlay`）
    /// が `masked_short` として plain scan へ縮退する。
    ///
    /// マスクが複数の連結成分に分かれ、選んだ探索起点の成分だけでは受理ノード
    /// を覆い切れない場合の検出は本関数の責務ではない
    /// （Cursor Bugbot High 指摘・PR #435: クエリ毎に層 0 の全受理ノード BFS を
    /// 行うとサブ線形探索の前に必ず O(V+E) が発生し ANN の目的を損なう）。
    /// 呼び出し元（`sql::hnsw_cache::Overlay::compute`）が世代（マスク）が
    /// 変わるたび 1 回だけ [`Self::is_mask_fully_reachable`] を呼び、分断が
    /// あれば本関数自体を呼ばず plain scan へ縮退する契約になっている
    /// （`docs/design/hnsw-rls-cardinality-switch.md`「masked_short」節）。
    ///
    /// `None` の場合は [`Self::search`] とビット同一の結果を返す
    /// （`crate::hnsw::tests::search_masked_none_matches_search` で機械検証）。
    ///
    /// # エラー
    ///
    /// [`Self::search`] と同じ検証順序に加え、`mask.len() != self.len()`
    /// （索引のノード数と不一致）は [`HnswError::InvalidParams`] として拒否する
    /// （fail-closed。呼び出し元が古い索引世代のマスクを渡す事故を検出する）。
    pub fn search_masked(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        mask: Option<&NodeMask>,
        scratch: &mut HnswSearchScratch,
    ) -> Result<Vec<crate::kernel::CandidateHit>, HnswError> {
        let dim_usize = self.dim as usize;
        if query.len() != dim_usize {
            return Err(HnswError::QueryDimMismatch {
                expected: self.dim,
                found: query.len(),
            });
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(HnswError::NonFiniteQuery);
        }
        if ef == 0 || ef > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef must be in 1..=MAX_EF",
            });
        }
        if k > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "k exceeds MAX_EF",
            });
        }
        if let Some(m) = mask {
            if m.len() != self.nodes.len() {
                return Err(HnswError::InvalidParams {
                    reason: "mask length does not match index node count",
                });
            }
        }
        if k == 0 || self.nodes.is_empty() {
            return Ok(Vec::new());
        }

        let Some(entry) = self.entry_point else {
            return Ok(Vec::new());
        };
        let Some(top_level) = self.max_level() else {
            return Ok(Vec::new());
        };

        // 固定 entry point が非受理なら、受理ノードの中から代替探索起点を選ぶ
        // （codex-review P2 指摘対応。§本関数ドキュメンテーションコメント参照）。
        // `search_entry_for_mask` は [`Self::is_mask_fully_reachable`] と同じ
        // 起点選択を共有する（検査と探索で起点を一致させる。Cursor Bugbot
        // High 指摘・PR #435）。代替起点は自身が存在する最上層
        // （`alt_level <= top_level`。全ノードは層 0..=自身の level に存在
        // するため）までしか降下できないので、以降の降下ループの上限も
        // それに合わせて縮める。
        let (mut nearest, effective_top) = match mask {
            Some(m) => match self.search_entry_for_mask(m) {
                Some(start) if start == entry => (start, top_level),
                Some(alt) => {
                    let alt_level = self.level_of(alt).unwrap_or(0);
                    (alt, top_level.min(alt_level))
                }
                // 受理ノードが 1 つも無い: ANN 経路では応答不能なので
                // plain scan への縮退に委ねる。
                None => return Ok(Vec::new()),
            },
            None => (entry, top_level),
        };
        if effective_top > 0 {
            for l in (1..=effective_top).rev() {
                nearest = match self.greedy_descend_masked(
                    nearest,
                    query,
                    l,
                    dim_usize,
                    &self.vectors,
                    mask,
                )? {
                    Some(n) => n,
                    // 到達しない防御的分岐: `nearest` は代替起点選択の時点で
                    // 受理済みであることを検証しており、`greedy_descend_masked`
                    // は受理済みの候補へしか `current` を進めないため、以降の
                    // 呼び出しでも常に受理済みノードを渡している。
                    None => return Ok(Vec::new()),
                };
            }
        }

        // 連結成分の分断検出は呼び出し元（`sql::hnsw_cache::Overlay::compute`）が
        // [`Self::is_mask_fully_reachable`] で世代毎に 1 回だけ行う契約
        // （§本関数ドキュメンテーションコメント参照）。ここではクエリ毎の
        // 全域 BFS を行わない。

        // k > ef のとき結果集合が k 件に満たない事故を防ぐため、実効 ef を
        // `ef.max(k)` へ引き上げる（hnswlib 等の一般的慣行。詳細は
        // `docs/design/hnsw-search.md` 参照）。ef・k は共に上で MAX_EF 以下と
        // 検証済みのため `ef_eff` も MAX_EF 以下。
        let ef_eff = ef.max(k);

        let results = self.search_layer(
            vec![nearest],
            query,
            ef_eff,
            0,
            dim_usize,
            &self.vectors,
            &mut scratch.visited,
            mask,
        )?;

        let out: Vec<crate::kernel::CandidateHit> = results
            .into_iter()
            .take(k)
            .map(|s| crate::kernel::CandidateHit {
                id: s.node as u64,
                score: s.score,
            })
            .collect();
        Ok(out)
    }
}

/// row-major バッファから `node` 番目のベクトルスライスを取り出す。範囲外
/// アクセスは `[]` を使わず `get()` で検出し、`CapacityOverflow` として拒否する
/// （coding-rust.md: untrusted 添字アクセス禁止。本関数はモジュール内部専用だが
/// `node_id` は `u32::try_from` 済みの構築時検証を経ているため、通常この分岐へは
/// 到達しない防御的経路である）。
fn node_vector(vectors: &[f32], dim: usize, node: u32) -> Result<&[f32], HnswError> {
    let start = (node as usize)
        .checked_mul(dim)
        .ok_or(HnswError::CapacityOverflow)?;
    let end = start.checked_add(dim).ok_or(HnswError::CapacityOverflow)?;
    vectors.get(start..end).ok_or(HnswError::CapacityOverflow)
}

/// `build`（`HnswParams::validate` → 次元整合 → ノード数上限 → 非有限値の
/// 検証順序。モジュール `build` ドキュメンテーションコメント参照）と
/// `build_with_threads`／`build_parallel`（Issue #406）が共有する入力検証。
/// 検証済みノード数 `n` を返す。
fn validate_build_input(
    params: &HnswParams,
    dim: u32,
    vectors: &[f32],
) -> Result<usize, HnswError> {
    params.validate()?;

    if dim == 0 {
        return Err(HnswError::DimMismatch {
            dim,
            len: vectors.len(),
        });
    }
    let dim_usize = dim as usize;
    if !vectors.len().is_multiple_of(dim_usize) {
        return Err(HnswError::DimMismatch {
            dim,
            len: vectors.len(),
        });
    }
    let n = vectors.len() / dim_usize;
    if n > MAX_HNSW_NODES {
        return Err(HnswError::TooManyNodes { nodes: n });
    }
    // ノード id を u32 で表現できることを構築前に確定させる（MAX_HNSW_NODES は
    // u32::MAX よりずっと小さいため通常は失敗しないが、上限定数の将来変更に
    // 備えて明示的に検証する。coding-rust.md: `checked_*`／`try_into` の使用）。
    if u32::try_from(n).is_err() {
        return Err(HnswError::CapacityOverflow);
    }

    for (node_idx, chunk) in vectors.chunks_exact(dim_usize).enumerate() {
        if chunk.iter().any(|v| !v.is_finite()) {
            return Err(HnswError::NonFiniteVector { node: node_idx });
        }
    }

    Ok(n)
}

/// `dot(node, query)`。[`HnswIndex::score`] と並列構築（`parallel_build`。
/// Issue #406）の `BuildGraph` の双方から共有される純粋関数（`self` を
/// 参照しないため元々自然にジェネリック化できた）。境界検証は
/// [`node_vector`] が担う。結果が非有限なら `NonFiniteScore` として拒否する
/// （モジュール冒頭「順序規約」節参照）。
fn score_of(vectors: &[f32], dim: usize, node: u32, query: &[f32]) -> Result<f32, HnswError> {
    let v = node_vector(vectors, dim, node)?;
    let d = dot(v, query);
    if !d.is_finite() {
        return Err(HnswError::NonFiniteScore { node });
    }
    Ok(d)
}

/// 近傍選択ヒューリスティック（Algorithm 4）の本体。[`HnswIndex::
/// select_neighbors_heuristic`] と並列構築の `BuildGraph` の双方から共有される
/// 純粋関数。既定は `extend_candidates=false`・`keep_pruned_connections=true`
/// （余った枠を枝刈り済み候補で埋め、次数を確保する）。この既定の採用理由は
/// `docs/design/hnsw-graph-construction.md` に記録する。
fn select_neighbors_heuristic_free(
    candidates: &[ScoredNode],
    m: usize,
    dim: usize,
    vectors: &[f32],
) -> Result<Vec<u32>, HnswError> {
    const KEEP_PRUNED_CONNECTIONS: bool = true;

    // 候補は search_layer が既にスコア降順で返すため、優先度付きキューへ
    // 詰め直す代わりにそのまま消費できるが、Algorithm 4 の記法に合わせて
    // 「未処理候補」を降順に保った Vec として扱う。
    let working: Vec<ScoredNode> = candidates.to_vec();

    let mut selected: Vec<ScoredNode> = Vec::new();
    let mut discarded: Vec<ScoredNode> = Vec::new();

    for cand in working {
        if selected.len() >= m {
            break;
        }
        let cand_vec = node_vector(vectors, dim, cand.node)?;
        // 「候補が既選択集合のどの要素よりも近い場合のみ採用する」枝刈り規則
        // （Algorithm 4）。dot は大きいほど近いため `>` が「より近い」の向き。
        let mut keep = true;
        for &sel in &selected {
            let sel_vec = node_vector(vectors, dim, sel.node)?;
            let d_to_selected = dot(cand_vec, sel_vec);
            if !d_to_selected.is_finite() {
                return Err(HnswError::NonFiniteScore { node: cand.node });
            }
            if d_to_selected > cand.score {
                keep = false;
                break;
            }
        }
        if keep {
            selected.push(cand);
        } else {
            discarded.push(cand);
        }
    }

    if KEEP_PRUNED_CONNECTIONS {
        let mut i = 0;
        while selected.len() < m {
            let Some(extra) = discarded.get(i) else {
                break;
            };
            selected.push(*extra);
            i += 1;
        }
    }

    selected.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    Ok(selected.into_iter().map(|s| s.node).collect())
}

/// [`HnswIndex::shrink_links`] と並列構築の `BuildGraph::shrink_links` の
/// 双方から共有される、縮退の計算のみを行う純粋関数（読み書き分離）。
/// `current_links` の要素数が `limit` 以下かつ `protect` を含む（または
/// `node == protect`）なら `Ok(None)`（変更不要）。それ以外は
/// `Ok(Some(new_links))` を返す。`protect` を強制的に残す契約・順序規約は
/// [`HnswIndex::shrink_links`] のドキュメンテーションコメント参照。
/// 呼び出し元（逐次経路は `&mut self.nodes` への 2 回のアクセス、並列
/// 経路は 1 回の書き込みロック内）が書き戻しを担う。
///
/// PR #431 codex-review（Cursor Bugbot）Medium 指摘の修正: 並列構築
/// （`BuildGraph::shrink_links`）では `connect(node, protect, level)` と
/// この関数を呼ぶ `shrink_links(node, level, protect)` が別々のロック
/// 獲得（[`BuildGraph::connect`]・[`BuildGraph::shrink_links`]）であるため、
/// その間に別ワーカーが同じ `node` へ異なる `protect` で `shrink_links` を
/// 実行し、こちらの `protect` を `current_links` から先に落とすレースが
/// 起こり得る。旧実装は「`protect` は `current_links`（延いては `scored`）
/// に必ず含まれる」という、逐次構築（`HnswIndex::insert_node`。同一スレッド
/// 内で `connect` 直後に `shrink_links` が走り割り込みが無い）でのみ成立する
/// 前提に依存しており、`protect` を明示的に `push` した直後に
/// `scored`（`current_links` 由来。`protect` を含まない場合がある）で
/// 再フィルタしてしまい `push` した `protect` を再び落としていた。
/// 本実装は `protect` が `current_links` に無い場合も `node_vector`
/// から直接そのスコアを算出し、`protect` を含む完全なスコア表
/// （`all_scored`）で再フィルタすることで、`protect` が最終的な
/// 結果集合（`limit` 件以内）に必ず残ることを保証する。
fn compute_shrink(
    current_links: &[u32],
    node: u32,
    dim: usize,
    vectors: &[f32],
    limit: usize,
    protect: u32,
) -> Result<Option<Vec<u32>>, HnswError> {
    let protect_present = node == protect || current_links.contains(&protect);
    if current_links.len() <= limit && protect_present {
        return Ok(None);
    }

    let node_vec = node_vector(vectors, dim, node)?;
    let mut scored: Vec<ScoredNode> = Vec::with_capacity(current_links.len());
    for &id in current_links {
        let v = node_vector(vectors, dim, id)?;
        let d = dot(node_vec, v);
        if !d.is_finite() {
            return Err(HnswError::NonFiniteScore { node: id });
        }
        scored.push(ScoredNode { node: id, score: d });
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));

    let mut shrunk = select_neighbors_heuristic_free(&scored, limit, dim, vectors)?;
    // `node == protect`（自己ループ）は `connect` が張らないため起こらないが、
    // 呼び出し契約が壊れても panic せず何もしない防御的分岐にしておく。
    if node != protect && !shrunk.contains(&protect) {
        if shrunk.len() >= limit {
            // `select_neighbors_heuristic_free` は高々 `limit` 件しか返さない
            // ため、この分岐に来る時点で `shrunk.len() == limit` であり、
            // `limit >= 2`（`HnswParams::validate` の `m >= 2` から層 0 も
            // 層 1 以上も導出される）なので `pop` は必ず要素を持つ。
            shrunk.pop();
        }
        shrunk.push(protect);
        // `protect` が `current_links`（延いては `scored`）に無い場合が
        // あり得るため（上記ドキュメンテーションコメント参照）、`scored`
        // をそのまま並び順の基準にはできない。`protect` 自身のスコアを
        // 直接算出し、`scored` に無ければ補ったうえで、差し替え後も
        // 「スコア降順・同点は id 昇順」の順序（モジュール冒頭の順序規約）
        // を保つよう並び替える。
        let mut all_scored = scored.clone();
        if !all_scored.iter().any(|s| s.node == protect) {
            let protect_vec = node_vector(vectors, dim, protect)?;
            let protect_score = dot(node_vec, protect_vec);
            if !protect_score.is_finite() {
                return Err(HnswError::NonFiniteScore { node: protect });
            }
            all_scored.push(ScoredNode {
                node: protect,
                score: protect_score,
            });
        }
        all_scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        shrunk = all_scored
            .iter()
            .filter(|s| shrunk.contains(&s.node))
            .map(|s| s.node)
            .collect();
    }
    Ok(Some(shrunk))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_corpus(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
        let mut rng = DeterministicRng::new(seed);
        let mut out = Vec::with_capacity(rows * dim);
        for _ in 0..rows {
            for _ in 0..dim {
                let bits = rng.next_u64() >> 40;
                let f = (bits as f32) / (1u32 << 24) as f32;
                out.push(f * 2.0 - 1.0);
            }
        }
        out
    }

    #[test]
    fn vector_returns_none_out_of_range_and_some_in_range() {
        let dim = 4u32;
        let vectors = gen_corpus(1, dim as usize, 10);
        let index = HnswIndex::build(HnswParams::default(), dim, &vectors, 7).unwrap();
        assert!(index.vector(0).is_some());
        assert!(index.vector(9).is_some());
        assert_eq!(index.vector(10), None, "out-of-range node must be None");
        assert_eq!(
            index.vector(0).unwrap(),
            &vectors[0..dim as usize],
            "vector() must return the same bytes build() was given"
        );
    }

    #[test]
    fn approx_heap_bytes_at_least_covers_raw_vector_storage() {
        let dim = 8usize;
        let rows = 50usize;
        let vectors = gen_corpus(2, dim, rows);
        let index = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 11).unwrap();
        let raw_bytes = rows * dim * std::mem::size_of::<f32>();
        assert!(
            index.approx_heap_bytes() >= raw_bytes,
            "approx_heap_bytes must at least cover the raw vector copy"
        );
    }

    #[test]
    fn default_params_match_documented_values() {
        let p = HnswParams::default();
        assert_eq!(p.m, 16);
        assert_eq!(p.ef_construction, 100);
        assert_eq!(p.ef_search, 64);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_rejects_out_of_range_params() {
        assert!(HnswParams {
            m: 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            m: MAX_M + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_construction: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_construction: MAX_EF + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_search: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_search: MAX_EF + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn assign_level_is_deterministic_for_same_seed() {
        let m = 16;
        let mut a = DeterministicRng::new(42);
        let mut b = DeterministicRng::new(42);
        let levels_a: Vec<usize> = (0..1000).map(|_| assign_level(&mut a, m)).collect();
        let levels_b: Vec<usize> = (0..1000).map(|_| assign_level(&mut b, m)).collect();
        assert_eq!(levels_a, levels_b);
    }

    #[test]
    fn assign_level_distribution_is_roughly_geometric() {
        // P(level >= 1) はおおむね 1/m。厳密な統計検定ではなく、明らかな実装崩れ
        // （例: 常に 0 を返す・毎回 MAX_LEVEL に張り付く）を検知する緩い帯で確認する。
        let m = 16usize;
        let mut rng = DeterministicRng::new(7);
        let samples = 20_000;
        let at_least_one = (0..samples)
            .filter(|_| assign_level(&mut rng, m) >= 1)
            .count();
        let ratio = at_least_one as f64 / samples as f64;
        let expected = 1.0 / m as f64;
        assert!(
            ratio > expected * 0.5 && ratio < expected * 2.0,
            "ratio={ratio} expected~={expected}"
        );
    }

    #[test]
    fn select_neighbors_heuristic_prunes_redundant_close_candidates() {
        // 2 次元の単位ベクトルで手動検証可能な配置を作る（dot を cosine 類似度として
        // 扱う既定契約に合わせ、正規化済みベクトルで構成する）:
        // クエリ = 0°、候補 A = 10°（クエリに極めて近い）、候補 B = 12°（A に
        // さらに近く、A に対して冗長）、候補 C = -80°（クエリからは離れているが
        // A からも離れており、A を経由してクエリより近づける代替ルートにならない）。
        // m=2 で選ぶと、B は「A に対する方がクエリに対するより近い」ため枝刈りされ、
        // A・C が採用される（B は A の近傍探索で別途到達可能なため、A・B を両方
        // 直結するのは次数の無駄という Algorithm 4 の意図どおりの挙動）。
        let dim = 2usize;
        let a = [0.9848f32, 0.1736f32]; // 10°
        let b = [0.9781f32, 0.2079f32]; // 12°（A に極めて近い）
        let c = [0.1736f32, -0.9848f32]; // -80°（A からも離れている）
        let vectors: Vec<f32> = [a, b, c].concat();
        let params = HnswParams {
            m: 2,
            ..Default::default()
        };
        let index = HnswIndex {
            params,
            dim: dim as u32,
            nodes: vec![
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            entry_point: Some(0),
            // 本テストは select_neighbors_heuristic を直接呼ぶのみで search() を
            // 経由しないため、`vectors` の内容は使われない（プレースホルダで
            // 十分）。
            vectors: Arc::from(vectors.clone()),
        };
        let query = &[1.0f32, 0.0f32];
        let candidates = vec![
            ScoredNode {
                node: 0,
                score: dot(&a, query),
            },
            ScoredNode {
                node: 1,
                score: dot(&b, query),
            },
            ScoredNode {
                node: 2,
                score: dot(&c, query),
            },
        ];
        let selected = index
            .select_neighbors_heuristic(&candidates, 2, dim, &vectors)
            .unwrap();
        assert!(selected.contains(&0));
        // B(1) は A(0) に対してクエリより近いため枝刈りされ、C(2) が代わりに採用される。
        assert!(selected.contains(&2));
        assert!(!selected.contains(&1));
    }

    #[test]
    fn build_rejects_dim_mismatch() {
        let err = HnswIndex::build(HnswParams::default(), 4, &[1.0, 2.0, 3.0], 1).unwrap_err();
        assert!(matches!(err, HnswError::DimMismatch { .. }));
    }

    #[test]
    fn build_rejects_zero_dim() {
        let err = HnswIndex::build(HnswParams::default(), 0, &[1.0, 2.0], 1).unwrap_err();
        assert!(matches!(err, HnswError::DimMismatch { .. }));
    }

    #[test]
    fn build_rejects_non_finite_vector() {
        let vectors = vec![1.0, f32::NAN, 0.0, 1.0];
        let err = HnswIndex::build(HnswParams::default(), 2, &vectors, 1).unwrap_err();
        assert!(matches!(err, HnswError::NonFiniteVector { .. }));
    }

    #[test]
    fn build_empty_input_yields_empty_index() {
        let index = HnswIndex::build(HnswParams::default(), 4, &[], 1).unwrap();
        assert_eq!(index.len(), 0);
        assert!(index.is_empty());
        assert_eq!(index.entry_point(), None);
        assert_eq!(index.max_level(), None);
    }

    #[test]
    fn search_layer_finds_true_nearest_neighbor_in_small_corpus() {
        let dim = 8;
        let rows = 300;
        let vectors = gen_corpus(0xABCD_1234, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 99).unwrap();

        // 総当たりで真の最近傍を求め、ef_construction 幅の search_layer 結果に
        // 含まれる率がおおむね高いことを検証する（結合テストの受け入れ条件は
        // tests/hnsw.rs 側で厳密に検証するため、ここでは内部関数の健全性のみ確認）。
        let mut hits = 0;
        let queries = 20;
        for q in 0..queries {
            let query = gen_corpus(0x1111_0000 + q as u64, dim, 1);
            let mut brute: Vec<ScoredNode> = (0..rows as u32)
                .map(|n| ScoredNode {
                    node: n,
                    score: dot(node_vector(&vectors, dim, n).unwrap(), &query),
                })
                .collect();
            brute.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
            let true_nearest = brute[0].node;

            let ep = index.entry_point().unwrap();
            let mut visited = VisitedScratch::default();
            let found = index
                .search_layer(
                    vec![ep],
                    &query,
                    params.ef_construction,
                    0,
                    dim,
                    &vectors,
                    &mut visited,
                    None,
                )
                .unwrap();
            if found.iter().any(|c| c.node == true_nearest) {
                hits += 1;
            }
        }
        assert!(hits as f64 / queries as f64 >= 0.9, "hits={hits}/{queries}");
    }

    /// `search_layer` の停止・受理判定が `ScoredNode` の複合順序（id 昇順
    /// タイブレーク込み）ではなく**スコアのみ**で行われることを直接検証する
    /// （`docs/design/hnsw-graph-construction.md`「`search_layer` の停止・
    /// 受理判定: 順序規約の使い分け」節参照）。手作りの 3 ノードグラフ
    /// （`entry(id0) -> node1(id1, entry と厳密同点スコア) -> node2(id2, 全体
    /// 最良スコア)`。node0 と node2 の間には直接リンクを張らない）に対し、
    /// `ef=1` で `search_layer` を呼ぶと、複合順序で判定した場合は
    /// entry と node1 の同点比較で `node1.cmp(entry) == Less`（id が大きい方が
    /// 複合順序では「より遠い」）となるため、受理判定（`worst_ok`）が node1 を
    /// 拒否し、node1 経由でしか到達できない node2 を発見できない。スコアのみの
    /// 判定であれば同点は拒否されず node1 の隣接探索まで進み、最終的に
    /// より良いスコアの node2 が結果に残る。
    #[test]
    fn search_layer_continues_through_tied_score_candidates_to_find_a_strictly_closer_node() {
        let dim = 1usize;
        // dim=1 の `dot(v, q) = v[0] * q[0]` なので `q=[1.0]` のときスコアは
        // ノード値そのものになる。
        let vectors: Vec<f32> = vec![10.0, 10.0, 20.0];
        let index = HnswIndex {
            params: HnswParams::default(),
            dim: dim as u32,
            nodes: vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            entry_point: Some(0),
            // 本テストは search_layer を直接呼ぶのみで search() を経由しない
            // ため、`vectors` フィールドの内容は使われない（プレースホルダで
            // 十分）。
            vectors: Arc::from(vectors.clone()),
        };
        let query = [1.0f32];
        let mut visited = VisitedScratch::default();
        let results = index
            .search_layer(vec![0], &query, 1, 0, dim, &vectors, &mut visited, None)
            .expect("search_layer should succeed");
        assert_eq!(
            results.iter().map(|s| s.node).collect::<Vec<_>>(),
            vec![2],
            "search_layer must traverse through a tied-score node to reach a \
             strictly closer one; an id-tiebreak (complex-order) stopping or \
             admission predicate would stop at the tie and miss node 2"
        );
    }

    /// Issue #431（codex-review P0 是正）の回帰テスト: 非受理（stale・不可視）
    /// ノードが探索経路の中継点として使われず、そのノード経由でしか到達
    /// できない受理ノードは結果に現れないことを確認する。
    ///
    /// `node0 -> node1 -> node2` の一本道（`node0`・`node2` 間に直接リンクは
    /// 張らない）を作り、`node1` を非受理としてマスクする。マスクなし
    /// （`None`）では `node1` を中継して最良スコアの `node2` を発見できるが、
    /// マスクありでは `node1` が候補ヒープへ一切積まれなくなったため
    /// （§`search_layer` ドキュメンテーションコメント参照）`node2` へは
    /// 到達できず、`node0` 単体の部分結果を返す。
    ///
    /// このマスクは「`node0` と `node2` の 2 つの連結成分に分断されている」
    /// 状態でもあり、`node0` だけを完全な top-k として返すと `node0` より
    /// 真に近い `node2`（マスク受理済み）を一切探索しないまま結果を確定して
    /// しまう recall バグになる——その分断検出は本関数の責務ではなく、
    /// 呼び出し元（`sql::hnsw_cache::Overlay::compute`）が世代毎に 1 回
    /// [`HnswIndex::is_mask_fully_reachable`] を呼んで検出し、`search_masked`
    /// 自体を呼ばず plain scan へ縮退する契約になっている（クエリ毎の全域
    /// BFS を避けるため。Cursor Bugbot High 指摘・PR #435）。同じグラフで
    /// `is_mask_fully_reachable` が分断を検出することは
    /// `is_mask_fully_reachable_detects_unreachable_component_even_when_
    /// reachable_component_satisfies_k` で固定する。
    #[test]
    fn search_masked_does_not_traverse_through_a_rejected_bridge_node() {
        let dim = 1usize;
        // dim=1 の `dot(v, q) = v[0] * q[0]` なので `q=[1.0]` のときスコアは
        // ノード値そのもの。node2 が最良スコアだが node1 経由でしか到達できない。
        let vectors: Vec<f32> = vec![10.0, 15.0, 20.0];
        let index = HnswIndex {
            params: HnswParams::default(),
            dim: dim as u32,
            nodes: vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            entry_point: Some(0),
            vectors: Arc::from(vectors),
        };
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();

        let unmasked = index
            .search_masked(&query, 3, 10, None, &mut scratch)
            .expect("unmasked search should succeed");
        assert_eq!(
            unmasked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![2, 1, 0],
            "unmasked search must traverse node0 -> node1 -> node2 and rank by score"
        );

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        // node1（橋渡しノード）は非受理のまま。
        mask.set(2);
        let masked = index
            .search_masked(&query, 3, 10, Some(&mask), &mut scratch)
            .expect("masked search should succeed");
        assert_eq!(
            masked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![0],
            "masked search must not traverse through the rejected bridge node1, so \
             node2 (mask-accepted, unreachable without node1) must not appear; the \
             graph-split detection that prevents this partial result from being \
             treated as a complete top-k is the caller's responsibility (§this \
             function's doc comment), not search_masked's"
        );

        // 同じグラフで分断検出（呼び出し元の責務）が正しく機能することを固定する。
        assert!(
            !index.is_mask_fully_reachable(&mask),
            "node2 is mask-accepted but unreachable from node0 without traversing \
             the rejected bridge node1, so the mask must be reported as split"
        );
    }

    #[test]
    fn visited_bitmap_reset_clears_all_bits_and_only_grows() {
        let mut bm = VisitedBitmap::default();
        bm.reset(10);
        assert_eq!(bm.mark_visited(3), Some(false));
        assert_eq!(bm.mark_visited(3), Some(true));
        // 伸長のみで縮めない: 一旦 200 まで広げてから 5 へ縮めても、以前確保した
        // 語も次の reset で全クリアされる（縮小しないことの安全側確認）。
        bm.reset(200);
        assert_eq!(bm.mark_visited(150), Some(false));
        bm.reset(5);
        assert_eq!(
            bm.mark_visited(150),
            Some(false),
            "reset は全クリアなので縮小後の呼び出しでも既訪問と誤判定してはならない"
        );
    }

    #[test]
    fn visited_bitmap_out_of_range_id_returns_none() {
        let mut bm = VisitedBitmap::default();
        bm.reset(10);
        assert_eq!(bm.mark_visited(999), None);
    }

    /// 手作りの最小グラフ（`search_layer_continues_through_tied_score_candidates_
    /// to_find_a_strictly_closer_node` と同じ 3 ノード構成）で、上位層の貪欲降下
    /// →層 0 のビーム探索という `search` の経路が正しく動作することを確認する。
    #[test]
    fn search_finds_expected_top_k_on_minimal_graph() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![10.0, 10.0, 20.0];
        let index = HnswIndex {
            params: HnswParams::default(),
            dim: dim as u32,
            nodes: vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            entry_point: Some(0),
            // search() は `self.vectors`（build 時の不変スナップショット）を
            // 参照するため、struct literal でも同じ内容を設定する。
            vectors: Arc::from(vectors.clone()),
        };
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();
        let results = index.search(&query, 2, 1, &mut scratch).unwrap();
        assert_eq!(
            results.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![2, 0],
            "search must beam through the tied-score node to reach node 2, then \
             fall back to node 0 (score 10.0) as the 2nd best"
        );
    }

    #[test]
    fn search_masked_none_matches_search() {
        let dim = 8usize;
        let vectors = gen_corpus(7, dim, 200);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 7).unwrap();
        let query = gen_corpus(999, dim, 1);
        let mut scratch_a = HnswSearchScratch::default();
        let mut scratch_b = HnswSearchScratch::default();
        let via_search = index.search(&query, 10, 40, &mut scratch_a).unwrap();
        let via_masked = index
            .search_masked(&query, 10, 40, None, &mut scratch_b)
            .unwrap();
        assert_eq!(via_search, via_masked);
    }

    #[test]
    fn search_masked_results_are_subset_of_mask() {
        let dim = 8usize;
        let n = 200;
        let vectors = gen_corpus(11, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 11).unwrap();
        let mut mask = NodeMask::new(index.len());
        // 偶数ノードのみ受理する（密度 50%）。
        for node in 0..index.len() {
            if node % 2 == 0 {
                mask.set(node as u32);
            }
        }
        // Issue #431 是正により、探索経路（貪欲降下・ビーム探索の候補展開）は
        // マスク外ノードを一切通過しなくなった。エントリポイント自身が非受理でも
        // 代替探索起点選択（codex-review P2 指摘対応・§`search_masked` ドキュメン
        // テーションコメント参照）により空集合へは縮退しなくなったため、本テスト
        // ではエントリポイントを意図的に mask から外したまま「受理ノードの
        // 部分集合が返る」性質を検証する（代替起点選択の直接的な検証は
        // `search_masked_falls_back_to_alternate_entry_when_fixed_entry_masked_out`）。
        let entry = index
            .entry_point()
            .expect("non-empty index has an entry point");
        let _ = entry;
        let query = gen_corpus(1234, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 20, 80, Some(&mask), &mut scratch)
            .unwrap();
        assert!(!results.is_empty(), "masked search should find some hits");
        for hit in &results {
            assert!(
                mask.get(hit.id as u32),
                "hit {} must be within the mask",
                hit.id
            );
        }
    }

    #[test]
    fn search_masked_all_false_returns_empty() {
        let dim = 8usize;
        let n = 100;
        let vectors = gen_corpus(3, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 3).unwrap();
        let mask = NodeMask::new(index.len());
        let query = gen_corpus(55, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 10, 40, Some(&mask), &mut scratch)
            .unwrap();
        assert!(results.is_empty());
    }

    /// codex-review P2 指摘対応（§`search_masked` ドキュメンテーションコメント
    /// 「固定 entry point 自体が `mask` に非受理の場合」節）: 固定 entry point
    /// だけをマスク外にし、他の受理ノードは十分に残す（密度 99%）。旧実装では
    /// entry point の非受理だけで貪欲降下の起点を持てず直ちに空集合を返して
    /// いたが、代替探索起点選択（[`HnswIndex::find_alternate_entry`]）により
    /// 受理ノードから探索できることを確認する。
    #[test]
    fn search_masked_falls_back_to_alternate_entry_when_fixed_entry_masked_out() {
        let dim = 8usize;
        let n = 300;
        let vectors = gen_corpus(21, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 21).unwrap();
        let entry = index
            .entry_point()
            .expect("non-empty index has an entry point");

        // 固定 entry point だけを非受理にし、他の全ノードを受理する
        // （`NodeMask` に unset API は無いため、entry を除いて `set` する）。
        let mut mask = NodeMask::new(index.len());
        for node in 0..index.len() as u32 {
            if node != entry {
                mask.set(node);
            }
        }
        assert!(!mask.get(entry), "fixed entry point must be masked out");

        let query = gen_corpus(4321, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 20, 80, Some(&mask), &mut scratch)
            .unwrap();
        assert!(
            !results.is_empty(),
            "masked search must fall back to an alternate entry point instead of \
             degrading to an empty result solely because the fixed entry point is \
             masked out"
        );
        for hit in &results {
            assert!(
                mask.get(hit.id as u32),
                "hit {} must be within the mask (never the excluded entry point {entry})",
                hit.id
            );
            assert_ne!(hit.id as u32, entry);
        }
    }

    /// [`HnswIndex::search_entry_for_mask`] を [`HnswIndex::search_masked`]（探索）
    /// と [`HnswIndex::is_mask_fully_reachable`]（分断検査）の双方が共有して
    /// いることの回帰テスト（Cursor Bugbot High 指摘・PR #435）。固定 entry
    /// point（`node0`）を非受理にし、代替起点として選ばれるはずの `node1` を
    /// 起点とする一本道 `node1 -> node2 -> node3`（全て受理）だけをマスクに
    /// 含める。もし検査側が代替起点選択を共有せず誤って `node0`（非受理）
    /// から辿ろうとすれば、`is_mask_fully_reachable` の内部 BFS は開始点が
    /// 非受理のため直ちに 0 件を返し、分断なしのマスクを「分断あり」と
    /// 誤検知して不要な plain scan を招く。共有選択が機能していれば、
    /// 検査は `search_masked` と同じ `node1` から辿り、分断なしと正しく
    /// 判定する。
    #[test]
    fn is_mask_fully_reachable_uses_the_same_alternate_entry_as_search_masked() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![0.0, 1.0, 2.0, 3.0];
        let index = HnswIndex {
            params: HnswParams::default(),
            dim: dim as u32,
            nodes: vec![
                // node0: entry point（マスクで非受理にする）。他ノードとは
                // 無関係な孤立ノードにしておき、誤って起点に使われた場合の
                // 挙動が明確になるようにする。
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![vec![3]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            entry_point: Some(0),
            vectors: Arc::from(vectors),
        };

        let mut mask = NodeMask::new(index.len());
        mask.set(1);
        mask.set(2);
        mask.set(3);
        // node0（entry point）は非受理のまま。

        assert!(
            index.is_mask_fully_reachable(&mask),
            "the mask's accepted nodes (1, 2, 3) form a single chain reachable from \
             the shared alternate entry point (node1); if the reachability check used \
             a different (rejected) start node it would wrongly report a split"
        );

        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 3, 10, Some(&mask), &mut scratch)
            .expect("masked search should succeed");
        assert_eq!(
            results.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![3, 2, 1],
            "search_masked must reach all three accepted nodes via the same \
             alternate entry point the reachability check used"
        );
    }

    /// codex-review P2 指摘の中核シナリオ（PR #435。呼び出し元側の分断検査へ
    /// 移設した際に overlay 時判定のテストへ書き換え）: マスクが複数の連結
    /// 成分に分かれ、かつ **entry 側の成分だけで結果件数 `k` を満たして
    /// しまう**場合（`search_masked_does_not_traverse_through_a_rejected_
    /// bridge_node` のケースは `k` が成分サイズを上回り件数検査でも検出
    /// できたが、本ケースは件数検査だけでは検出できない）。
    ///
    /// entry 側成分 `{node0(entry), node1, node2}`（`0 -> 1 -> 2` の一本道、
    /// スコアは 1.0/2.0/3.0）はすべて受理され、`k=2` の結果件数は成分内だけで
    /// 満たせる。一方、entry 側成分と辺で一切繋がっていない孤立ノード
    /// `node3`（スコア 100.0・受理済み）がクエリに最も近い真の正解だが、
    /// 単一 entry point からの誘導部分グラフ探索では構造的に到達不能。
    /// [`HnswIndex::is_mask_fully_reachable`]（`sql::hnsw_cache::Overlay::
    /// compute` が世代毎に 1 回呼ぶ、`search_masked` とは独立の分断検査）が
    /// 「entry 側成分（3 件）だけではマスクの受理ノード総数（4 件）を覆え
    /// ない」ことを検出することを固定する。`search_masked` 自体はこの検査を
    /// 行わないため、`node1`・`node2` だけの部分結果を返す（件数検査は通過
    /// するが node3 を取りこぼす——この recall バグを実際に防ぐのは呼び
    /// 出し元が `is_mask_fully_reachable` を見て `search_masked` 自体を
    /// 呼ばない判断をすること）。
    #[test]
    fn is_mask_fully_reachable_detects_unreachable_component_even_when_reachable_component_satisfies_k(
    ) {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![1.0, 2.0, 3.0, 100.0];
        let index = HnswIndex {
            params: HnswParams::default(),
            dim: dim as u32,
            nodes: vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                // node3: entry 側成分とは辺を一切持たない孤立ノード（スコア最良）。
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            entry_point: Some(0),
            vectors: Arc::from(vectors),
        };
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        mask.set(1);
        mask.set(2);
        mask.set(3); // node3 も受理（到達不能なだけで RLS 上は可視）。

        // 分断検査（呼び出し元の責務）は「entry 側成分だけで k を満たす」
        // ケースでも到達不能な node3 を正しく検出する。
        assert!(
            !index.is_mask_fully_reachable(&mask),
            "node3 is mask-accepted but structurally unreachable from the single \
             entry point, so the mask must be reported as split even though the \
             reachable component alone would satisfy k=2"
        );

        // `search_masked` 自体はこの検査を行わないため、entry 側成分内の
        // 部分結果（node3 を欠く）をそのまま返す——この防止は呼び出し元が
        // 上記の `is_mask_fully_reachable` を見て `search_masked` を呼ばない
        // 判断をすることで実現される（§本関数ドキュメンテーションコメント
        // 参照）。
        let masked = index
            .search_masked(&query, 2, 10, Some(&mask), &mut scratch)
            .expect("masked search should succeed");
        assert_eq!(
            masked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![2, 1],
            "search_masked itself only explores the entry-reachable component and \
             returns its top-k (node2, node1); node3 is absent because search_masked \
             does not perform graph-split detection"
        );
    }

    #[test]
    fn search_masked_rejects_length_mismatch() {
        let dim = 4usize;
        let vectors = gen_corpus(2, dim, 50);
        let params = HnswParams {
            m: 6,
            ef_construction: 32,
            ef_search: 16,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 2).unwrap();
        // 索引のノード数より短いマスクは拒否される（fail-closed）。
        let mask = NodeMask::new(index.len().saturating_sub(1));
        let query = gen_corpus(8, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let err = index
            .search_masked(&query, 5, 20, Some(&mask), &mut scratch)
            .unwrap_err();
        assert!(matches!(err, HnswError::InvalidParams { .. }));
    }

    /// codex-review PR #430 P1 指摘への対応で `HnswIndex` は `build` 時の
    /// `vectors` を `Arc<[f32]>` として所有するよう変更した（モジュール冒頭
    /// 「ベクトルの所有方針」節）。呼び出し元が `build` に渡した元のバッファ
    /// （`Vec<f32>`）を構築後に書き換えても、`search` は build 時点で取得した
    /// 不変スナップショットのみを参照するため一切影響を受けないことを固定する
    /// ——旧設計（`search` へ毎回 `&[f32]` を渡す方式）ではサンプリング対象外
    /// の書き換えが静かに受理され得たが、この設計では「別バッファが search に
    /// 渡される」という入力のクラス自体が存在しない。
    #[test]
    fn search_is_unaffected_by_mutations_to_the_caller_owned_build_buffer() {
        let dim = 8usize;
        let rows = 50usize;
        let mut vectors = gen_corpus(0xAAAA_1111, dim, rows);
        let index =
            HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 0xBBBB_2222).unwrap();

        let query: Vec<f32> = vectors[0..dim].to_vec();
        let mut scratch = HnswSearchScratch::default();
        let before = index.search(&query, 5, 32, &mut scratch).unwrap();

        // ノード 0 とノード 1 の行を入れ替え、さらにノード 40（サンプリング
        // 方式なら見逃しうる位置）の要素も書き換える。呼び出し元が所有する
        // `vectors` を直接破壊しているが、index はこのバッファを一切参照しない。
        let (front, back) = vectors.split_at_mut(dim);
        front[..dim].swap_with_slice(&mut back[..dim]);
        vectors[40 * dim] += 1.0;
        drop(vectors); // index が自身のスナップショットのみで完結することを明示する。

        let after = index.search(&query, 5, 32, &mut scratch).unwrap();
        assert_eq!(
            before, after,
            "search must be based solely on the build-time snapshot, unaffected by \
             the caller mutating or dropping its own copy of the vectors buffer"
        );
    }

    /// PR #431 codex-review（Cursor Bugbot）Medium 指摘の回帰: `protect` が
    /// `current_links` に含まれない状態（並列構築で他ワーカーが先に
    /// `protect` への逆方向リンクを縮退させてしまうレース。`compute_shrink`
    /// のドキュメンテーションコメント参照）でも、`compute_shrink` の結果
    /// 集合には必ず `protect` が含まれ、かつ `limit` 件を超えないことを
    /// 固定する。
    #[test]
    fn compute_shrink_always_retains_protect_even_when_absent_from_current_links() {
        let dim = 1usize;
        // ノード 0（自分自身）とノード 1..=5 の 1 次元ベクトルを用意する。
        // `protect`（ノード 6）は `current_links` に含めない
        // （＝並列構築下で既に他ワーカーに縮退されてしまった状況を模す）。
        let vectors: Vec<f32> = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let current_links: Vec<u32> = vec![1, 2, 3, 4, 5];
        let limit = 3usize;
        let protect = 6u32;

        let shrunk = compute_shrink(&current_links, 0, dim, &vectors, limit, protect)
            .expect("finite 1-d vectors must not overflow")
            .expect("current_links.len() > limit must trigger a shrink");

        assert!(
            shrunk.contains(&protect),
            "protect must survive the shrink even when absent from current_links, got {shrunk:?}"
        );
        assert!(
            shrunk.len() <= limit,
            "shrink result must not exceed the degree limit, got {shrunk:?}"
        );
    }

    /// 上記の派生ケース: `current_links.len() <= limit` でも `protect` が
    /// 欠けていれば `Ok(None)`（変更不要）を返してはならない——`None` は
    /// 呼び出し元に「既存のリンクのままでよい」と伝えるため、`protect` が
    /// 欠けたまま何も書き戻されないと `protect` は永久に失われる。
    #[test]
    fn compute_shrink_adds_protect_when_missing_even_under_the_degree_limit() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![0.0, 1.0, 2.0, 6.0];
        let current_links: Vec<u32> = vec![1, 2];
        let limit = 3usize;
        let protect = 3u32;

        let shrunk = compute_shrink(&current_links, 0, dim, &vectors, limit, protect)
            .expect("finite 1-d vectors must not overflow")
            .expect("protect missing from current_links must not short-circuit to Ok(None)");

        assert!(
            shrunk.contains(&protect),
            "protect must be added even when current_links is already within the degree limit, \
             got {shrunk:?}"
        );
        assert!(shrunk.len() <= limit, "got {shrunk:?}");
    }
}
