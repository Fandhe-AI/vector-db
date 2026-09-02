//! HNSW（Hierarchical Navigable Small World）グラフの構築（TASK-132・対象ビヘイビア:
//! CORE-9・CORE-10。ポインタ: `docs/design/ann-index-adoption.md`「実装ガイド（B 案）」節）。
//!
//! 本モジュールの範囲は**グラフ構築のみ**（Malkov & Yashunin 2016 の Algorithm 1〜4 相当）。
//! 探索 API（`ef_search` を使った top-k 探索）・並列構築・`search_engine.rs` への
//! `SearchEngineKind::Hnsw` 結線・世代整合キャッシュ・RLS 統合・永続化はいずれも
//! 別タスク（#405〜#409）の担当であり、本モジュールは触れない。ADR
//! （`docs/design/ann-index-adoption.md`）の「非契約的な実装詳細」区分に基づき、
//! spec 側の確定を待たずに着手している。
//!
//! # ベクトルの所有方針（#405・#408 への申し送り）
//!
//! [`HnswIndex`] はグラフ（隣接リスト・レベル・エントリポイント）のみを保持し、
//! ベクトル本体は複製しない。呼び出し元（`arena.rs::VectorArena` や #408 の
//! 世代整合キャッシュ）が row-major 連続バッファとして所有し続け、[`HnswIndex::build`]
//! へは `&[f32]` で借用のみ渡す契約とする。768 次元 × 100 万行では複製だけで
//! 3 GB 級になるため、この方針はメモリ効率上の要請である。探索 API（#405）も
//! 同じ `vectors: &[f32]` を受け取り、`len() * dim() == vectors.len()` を毎回
//! 検証する（fail-closed。呼び出し元がビルド後にベクトル集合を差し替えてしまう
//! 事故を検出する唯一の手段が長さ照合であるため）。
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
//! `scripts/check_sort_determinism.sh` が禁止する）。
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

use crate::kernel::dot;

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

/// HNSW 構築パラメータ。
///
/// 既定値（`M=16`／`ef_construction=100`／`ef_search=64`）は ADR 起票 Issue #403
/// に記載の本リポ採用値（非規範的な実装既定値。spec 側の確定値ではない）。
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
    /// オフセット・容量計算が `usize`／`u32` の範囲を超えた。
    CapacityOverflow,
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
            HnswError::CapacityOverflow => write!(f, "capacity computation overflowed"),
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

/// 構築済み HNSW グラフ。ベクトル本体を保持しない（[`HnswIndex::build`] の
/// 呼び出し元が所有し続ける借用契約。モジュール冒頭コメント参照）。
#[derive(Debug)]
pub struct HnswIndex {
    params: HnswParams,
    dim: u32,
    nodes: Vec<Node>,
    entry_point: Option<u32>,
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

        let mut index = HnswIndex {
            params,
            dim,
            nodes: Vec::with_capacity(n),
            entry_point: None,
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

    /// `search_layer`（Algorithm 2）。層 `level` 上で `entry_points` から出発し、
    /// 幅 `ef` の貪欲拡張探索を行い、`dot` 降順（同点は id 昇順）に並んだ最大 `ef`
    /// 件の候補を返す。`pub(crate)` にして #405（探索 API）が `ef_search` で再利用
    /// できるようにする。
    ///
    /// `visited` は呼び出し元（`insert_node`／`build` あるいはテスト）が
    /// 全呼び出しをまたいで所有する [`VisitedScratch`]。挿入ごとに新規の
    /// `Vec<bool>` を確保しゼロ初期化していた旧実装は、`build` が挿入ごとに
    /// 少なくとも層 0 で本関数を呼ぶため合計 O(N^2) の初期化コストになって
    /// いた（codex-review #423 P1 指摘）。世代カウンタ方式のスクラッチへ
    /// 切り替え、各呼び出しの先頭で `reset` するだけに変更した。
    #[allow(clippy::too_many_arguments)] // visited 追加で 8 引数。既存の precision.rs・arena.rs と同じ方針で許容する。
    pub(crate) fn search_layer(
        &self,
        entry_points: Vec<u32>,
        query: &[f32],
        ef: usize,
        level: usize,
        dim: usize,
        vectors: &[f32],
        visited: &mut VisitedScratch,
    ) -> Result<Vec<ScoredNode>, HnswError> {
        visited.reset(self.nodes.len());
        let mut candidates: BinaryHeap<ScoredNode> = BinaryHeap::new();
        // 結果集合は最小ヒープとして扱いたいので `Reverse` で包む。
        let mut results: BinaryHeap<std::cmp::Reverse<ScoredNode>> = BinaryHeap::new();

        for ep in entry_points {
            match visited.mark_visited(ep as usize) {
                Some(true) => continue,
                Some(false) => {}
                None => continue,
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
                    let neighbor_score = self.score(neighbor, query, dim, vectors)?;
                    let scored = ScoredNode {
                        node: neighbor,
                        score: neighbor_score,
                    };
                    // 受理判定も打ち切り判定と同じ理由でスコアのみの比較に限定
                    // する（`scored` が `worst` とスコア同点なら、id 順の複合
                    // 順序で「劣る」と判定されても受理する）。
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
        let current_len = self
            .nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .map(|l| l.len())
            .unwrap_or(0);
        if current_len <= limit {
            return Ok(());
        }

        let node_vec = node_vector(vectors, dim, node)?;
        let neighbor_ids: Vec<u32> = self
            .nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .cloned()
            .unwrap_or_default();

        let mut scored: Vec<ScoredNode> = Vec::with_capacity(neighbor_ids.len());
        for id in neighbor_ids {
            let v = node_vector(vectors, dim, id)?;
            scored.push(ScoredNode {
                node: id,
                score: dot(node_vec, v),
            });
        }
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));

        let mut shrunk = self.select_neighbors_heuristic(&scored, limit, dim, vectors)?;
        // `node == protect`（自己ループ）は `connect` が張らないため起こらないが、
        // 呼び出し契約が壊れても panic せず何もしない防御的分岐にしておく。
        // `protect` は直前に `connect(node, protect, level)` で張られたばかりの
        // 隣接であり、上の `scored` 構築元 `neighbor_ids` に必ず含まれる。
        if node != protect && !shrunk.contains(&protect) {
            if shrunk.len() >= limit {
                // `limit >= 2`（`HnswParams::validate` の `m >= 2` から層 0 も
                // 層 1 以上も導出される）なので `pop` は必ず要素を持つ。
                shrunk.pop();
            }
            shrunk.push(protect);
            // 差し替え後も次数上限超過検出時と同じ「スコア降順・同点は id 昇順」
            // の順序（モジュール冒頭の順序規約）を保つ。`scored` は既に同じ規約で
            // ソート済みなので、その並び順を基準にフィルタし直すだけでよい。
            shrunk = scored
                .iter()
                .filter(|s| shrunk.contains(&s.node))
                .map(|s| s.node)
                .collect();
        }
        if let Some(n) = self.nodes.get_mut(node as usize) {
            if let Some(links) = n.links.get_mut(level) {
                *links = shrunk;
            }
        }
        Ok(())
    }

    /// `dot(node, query)`。ノード id が範囲外／`vectors` が短すぎる場合は
    /// `NonFiniteVector` 相当ではなく、境界検証は [`node_vector`] が担う
    /// （呼び出し元は構築時に検証済みの id しか渡さない内部専用パス）。
    fn score(
        &self,
        node: u32,
        query: &[f32],
        dim: usize,
        vectors: &[f32],
    ) -> Result<f32, HnswError> {
        let v = node_vector(vectors, dim, node)?;
        Ok(dot(v, query))
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
        };
        let query = [1.0f32];
        let mut visited = VisitedScratch::default();
        let results = index
            .search_layer(vec![0], &query, 1, 0, dim, &vectors, &mut visited)
            .expect("search_layer should succeed");
        assert_eq!(
            results.iter().map(|s| s.node).collect::<Vec<_>>(),
            vec![2],
            "search_layer must traverse through a tied-score node to reach a \
             strictly closer one; an id-tiebreak (complex-order) stopping or \
             admission predicate would stop at the tie and miss node 2"
        );
    }
}
