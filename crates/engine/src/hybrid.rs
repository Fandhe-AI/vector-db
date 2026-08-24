//! 密検索・疎検索の RRF（Reciprocal Rank Fusion）融合モジュール（TASK-103。対象ビヘイビア:
//! SEARCH-1, SEARCH-3）。
//!
//! `kernel.rs`/`parallel_search.rs`（TASK-124・TASK-126）が提供する密検索 provider
//! （[`crate::kernel::SearchProvider`]）と、`sparse.rs`（TASK-102）が提供する疎検索
//! （[`crate::sparse::SparseIndex`]）は独立に存在する。本モジュールはその 2 系統を
//! RRF（公知のランク融合手法。各リストの順位のみを使い、`weight / (k_const + rank)`
//! を id ごとに加算する）で統合する、純粋関数的な層として追加する。
//!
//! `sparse.rs` と同様に storage・catalog・policy とは結線しない。可視性判定（RLS 相当の
//! テナント境界）はこの層より上（`core.rs` 相当）で完結している前提であり、
//! [`hybrid_search`] へ渡す `input`（[`crate::kernel::SearchInput`]）は呼び出し元が
//! あらかじめ可視行のみへ縮約済みであることを契約とする。ただし `sparse_index`
//! （[`crate::sparse::SparseIndex`]）・`provider`（[`crate::kernel::SearchProvider`]
//! trait object）はいずれも `SearchInput` と異なり構造的にその縮約を強制できない別個の
//! オブジェクトのため、それぞれの検索結果を「同一の可視集合から構築されている／
//! `input.ids` 外の id を返さない」という promissory な契約だけに委ねない。
//! [`hybrid_search`] は密・疎双方のヒットを `input.ids`（可視集合）に含まれる id
//! のみへ限定してから融合する（[`crate::kernel::SearchInput`] のドキュメントが同種の
//! promissory な `is_visible` クロージャ規約から脱却した設計判断と同じ方向）。
//! 疎側は `SparseIndex::search()`（インデックス全体を母数に統計・Top-k を計算する
//! API）ではなく [`crate::sparse::SparseIndex::search_within`]（統計・候補選出
//! そのものを `visible_ids` へ縮約する API）を呼ぶ（Issue #36 codex-review P0
//! 指摘対応）: `search()` を事後フィルタするだけでは、不可視文書が Top-k の枠を
//! 占有して可視文書を押し出す経路・不可視文書の内容が可視文書の順位へ影響する経路の
//! いずれも防げない。
//! 密側の `provider`（[`crate::kernel::SearchProvider`] trait object）は `input`
//! （＝ `SearchInput { ids: input.ids, .. }`）に渡した可視 id のみを走査する契約だが、
//! `provider` は型では「`input.ids` 外の id を返さない」ことを強制できない別個の
//! オブジェクトである。疎側と同じ理由で事後フィルタ（黙って不可視 id を除外する）
//! では不十分（2 回目の codex-review P0 指摘対応）: 不可視 id が `cfg.pool_depth()`
//! の候補枠を占有していた場合、事後フィルタは可視ヒットを復元できず、検索結果の
//! 件数差から不可視データの有無が外部へ漏れる（sparse 側の pool 占有問題と同型）。
//! そのため [`hybrid_search`] は密側の戻り値に可視集合外の id が 1 件でも含まれて
//! いたら、その分だけ除外するのではなく検索全体を [`HybridError::ProviderResultRejected`]
//! で拒否する（`core.rs::EngineCore::search` の provider 結果検証と同じ
//! fail-closed の方向）。
//! [`rrf_fuse`] はさらに 2 種類のリソース安全性検証を行う（3 回目の codex-review P1
//! 指摘対応）: (1) 融合前に各入力リストの長さが `cfg.pool_depth()` を超えないことを
//! 検証し、契約違反の provider/index が巨大な結果を返した場合の無制限なメモリ・CPU
//! 消費を防ぐ（[`HybridError::TooManyCandidates`]）。(2) 融合後の加算結果の有限性を
//! 検証し、有限な重み（`dense_weight`/`sparse_weight`）同士の加算がオーバーフローして
//! `+Inf` になった場合を検知する（[`HybridError::NonFiniteScore`]）。
//! `VectorCore` trait への統合・SQL 表層統合・RLS 統合は後続タスクの管轄でありここでは扱わない。

use std::collections::BTreeMap;
use std::fmt;

use crate::kernel::{KernelError, SearchHit, SearchInput, SearchProvider};
use crate::sparse::{ScoredDoc, SparseError, SparseIndex};

/// 検索プールの深さ・k の上限。`core.rs::MAX_SEARCH_K`（同じく 10_000）と同桁を採用し、
/// 未検証の巨大な値がそのままアロケーションサイズへ伝播することを防ぐ
/// （coding-rust.md「無制限確保禁止」）。
const MAX_POOL_DEPTH: usize = 10_000;

/// RRF 融合の設定（本モジュールの既定値。関連: TASK-103）。
///
/// - `k_const`: RRF のランク減衰定数（一般的な既定値 60.0 を採用）。
/// - `dense_weight` / `sparse_weight`: 密・疎それぞれの寄与の重み（既定は等重み 1.0）。
/// - `pool_depth`: 密・疎それぞれから融合対象として取り込む先頭順位数。
///
/// フィールドは非 `pub`（private）とし、[`RrfConfig::new`] による検証済み構築のみを
/// 許可する。`rrf_fuse`/`accumulate_ranked` は「`pool_depth` は `RrfConfig::new` で
/// 検証済みの値のみが渡される」ことを契約として前提にしており、構造体リテラルでの
/// 直接構築（`RrfConfig { k_const: f64::NAN, .. }` 等）を許すと検証を迂回でき、
/// NaN スコアが黙って返る fail-open な経路になりうる（security.md「不安全な設計」）。
/// フィールド値の参照は [`RrfConfig::k_const`] 等のアクセサ経由で行う。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RrfConfig {
    k_const: f64,
    dense_weight: f64,
    sparse_weight: f64,
    pool_depth: usize,
}

impl Default for RrfConfig {
    fn default() -> Self {
        Self {
            k_const: 60.0,
            dense_weight: 1.0,
            sparse_weight: 1.0,
            pool_depth: 200,
        }
    }
}

impl RrfConfig {
    /// 検証付きコンストラクタ。`pool_depth` は `1..=MAX_POOL_DEPTH`、`k_const`・重みは
    /// 有限かつ正であることを構築時に検証し、違反は `Err`（fail-closed）。
    /// フィールドが非 `pub` のため、`RrfConfig` を構築する経路はこの関数（と
    /// 常に妥当な値を返す [`Default::default`]）のみに限定される。これにより
    /// [`rrf_fuse`]・[`hybrid_search`] は妥当性検証済みの設定のみを扱える。
    pub fn new(
        k_const: f64,
        dense_weight: f64,
        sparse_weight: f64,
        pool_depth: usize,
    ) -> Result<Self, HybridError> {
        if pool_depth == 0 || pool_depth > MAX_POOL_DEPTH {
            return Err(HybridError::InvalidConfig);
        }
        if !k_const.is_finite() || k_const <= 0.0 {
            return Err(HybridError::InvalidConfig);
        }
        if !dense_weight.is_finite() || dense_weight <= 0.0 {
            return Err(HybridError::InvalidConfig);
        }
        if !sparse_weight.is_finite() || sparse_weight <= 0.0 {
            return Err(HybridError::InvalidConfig);
        }
        Ok(Self {
            k_const,
            dense_weight,
            sparse_weight,
            pool_depth,
        })
    }

    /// RRF のランク減衰定数（検証済み: 有限かつ正）。
    pub fn k_const(&self) -> f64 {
        self.k_const
    }

    /// 密検索側の寄与の重み（検証済み: 有限かつ正）。
    pub fn dense_weight(&self) -> f64 {
        self.dense_weight
    }

    /// 疎検索側の寄与の重み（検証済み: 有限かつ正）。
    pub fn sparse_weight(&self) -> f64 {
        self.sparse_weight
    }

    /// 融合対象として取り込む先頭順位数（検証済み: `1..=MAX_POOL_DEPTH`）。
    pub fn pool_depth(&self) -> usize {
        self.pool_depth
    }
}

/// 融合後の検索結果 1 件（行 ID と RRF スコア）。
///
/// `crate::kernel::SearchHit`（`score: f32`、内積スコア）・`crate::sparse::ScoredDoc`
/// （`score: f64`、BM25 スコア）とは尺度が異なる値のため、型を分けて取り違えを防ぐ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HybridHit {
    pub id: u64,
    pub score: f64,
}

/// [`rrf_fuse`]・[`hybrid_search`] が返すエラー。下位層（[`KernelError`]・[`SparseError`]）
/// はそのまま伝播させ、握りつぶさない（fail-closed）。
#[derive(Debug, Clone, PartialEq)]
pub enum HybridError {
    /// 密検索 provider が返したエラー。
    Kernel(KernelError),
    /// 疎検索が返したエラー。
    Sparse(SparseError),
    /// [`RrfConfig::new`] の検証違反。
    InvalidConfig,
    /// [`hybrid_search`] に渡された `k` が `0` または上限超過。
    InvalidK,
    /// 融合対象の入力リスト（密・疎いずれか。全件が対象。[`TooManyCandidates`]
    /// (HybridError::TooManyCandidates) により各リストは高々 `cfg.pool_depth()` 件に
    /// 制限されるため「全件」と「先頭 `pool_depth` 件」は常に一致する）に同一 id が
    /// 複数回出現した。provider・インデックス側の契約違反（バグ）として fail-closed
    /// に拒否する（部分的に正しい融合スコアを返すより、検索全体を失敗させる方が
    /// 安全側）。
    DuplicateId,
    /// 融合対象の入力リスト（密・疎いずれか）が、それぞれの provider/index が定める
    /// 順位契約（スコア降順・同点 id 昇順）に従っていなかった。RRF は元スコアを見ず
    /// 順位のみを使うため、ソート順を信頼で通すと不正な順序が黙って誤った融合スコアを
    /// 生む（fail-open）。`kernel.rs`/`parallel_search.rs` 側の
    /// `provider_returning_hits_out_of_score_order_is_rejected` と対になる検証を、
    /// 本モジュールでも独立に行う（fail-closed）。
    UnsortedInput,
    /// 融合対象の入力リスト（密・疎いずれか）に非有限スコア（NaN・Inf）が含まれていた、
    /// または各リストの入力自体は有限でも RRF の融合（`weight / (k_const + rank)` の
    /// 加算）結果が非有限（Inf）へオーバーフローした（3 回目の codex-review P1 指摘
    /// 対応: `RrfConfig::new` は重みの有限性・正数のみを検証し上限は課さないため、
    /// 密・疎双方の重みに極端に大きい値（例: `f64::MAX` 近傍）を指定し、同一 id が
    /// 両リストの上位順位に現れると、個々の入力は有限でも加算後のスコアが `f64::MAX`
    /// を超えて `+Inf` になりうる。融合前の入力検証だけでは検知できないため、
    /// [`accumulate_ranked`] による加算後の `scores` にも同じ検証をかける）。
    /// `f64::total_cmp` は NaN にもビットパターンに基づく全順序を与えてしまうため、
    /// 有限性を確認しないまま [`UnsortedInput`](HybridError::UnsortedInput) の順序検証
    /// （`is_sorted_desc_id_asc`）だけに頼ると、NaN が「たまたま」順序契約を満たす
    /// ビットパターンで紛れ込んだ場合に検出できず、無意味な順位で融合されてしまう
    /// （fail-open）。`core.rs::EngineCore::search` の provider 結果検証が「(2) スコア
    /// 有限性 → (5) 順序」の順で先に有限性を確認するのと同じ理由・同じ順序で、
    /// 本モジュールでも順序検証より先に入力の有限性を検知する（融合後の有限性検証は
    /// 加算そのものが終わった後にしか行えないため、これとは別に行う）。
    NonFiniteScore,
    /// 融合対象の入力リスト（密・疎いずれか）の長さが `cfg.pool_depth()` を超えていた
    /// （3 回目の codex-review P1 指摘対応）。[`rrf_fuse`] は各リストの先頭
    /// `cfg.pool_depth()` 件のみを融合対象として使うが、以前は長さそのものを検証せず
    /// 全件を [`is_sorted_desc_id_asc`]・重複検査（`BTreeSet` への全件挿入）に通して
    /// いたため、契約違反の provider・呼び出し元が `cfg.pool_depth()`（高々
    /// `MAX_POOL_DEPTH`）を大きく超える件数を返すとその分だけ無制限にメモリ・CPU を
    /// 消費できた（coding-rust.md「無制限確保禁止」違反）。融合前（有限性・ソート順・
    /// 重複検査より先）に長さを検証し、超過は fail-closed に拒否する。
    TooManyCandidates { len: usize, max: usize },
    /// 密検索 `provider`（[`SearchProvider`] trait object）が、渡した
    /// `input.ids`（可視集合）に含まれない id を 1 件でも返した（provider 実装の
    /// 契約違反）。黙って不可視 id だけを除外すると、その id が `cfg.pool_depth()`
    /// の候補枠を占有していた場合に可視ヒットを復元できず、検索結果の件数・順位の
    /// 差から不可視データの有無が外部へ漏れうる（2 回目の codex-review P0 指摘対応。
    /// `crate::sparse::SparseIndex::search_within` が疎側で同じ問題を統計計算段階の
    /// 縮約で防ぐのに対し、密側は provider を型で強制できないため契約違反の検出時点
    /// で検索全体を拒否する形で fail-closed を保つ。`core.rs::CoreError::ProviderResultRejected`
    /// と同じ設計方向）。
    ProviderResultRejected,
}

impl fmt::Display for HybridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HybridError::Kernel(e) => write!(f, "hybrid search dense provider error: {e}"),
            HybridError::Sparse(e) => write!(f, "hybrid search sparse index error: {e}"),
            HybridError::InvalidConfig => write!(f, "invalid RRF config"),
            HybridError::InvalidK => write!(f, "invalid k for hybrid search"),
            HybridError::DuplicateId => write!(f, "duplicate id in hybrid fusion input list"),
            HybridError::UnsortedInput => {
                write!(f, "hybrid fusion input list is not sorted by rank contract")
            }
            HybridError::NonFiniteScore => {
                write!(f, "hybrid fusion input list contains a non-finite score")
            }
            HybridError::ProviderResultRejected => {
                write!(
                    f,
                    "dense provider returned a hit outside the visible id set"
                )
            }
            HybridError::TooManyCandidates { len, max } => {
                write!(
                    f,
                    "hybrid fusion input list too long: {len} candidates (max {max})"
                )
            }
        }
    }
}

impl std::error::Error for HybridError {}

impl From<KernelError> for HybridError {
    fn from(e: KernelError) -> Self {
        HybridError::Kernel(e)
    }
}

impl From<SparseError> for HybridError {
    fn from(e: SparseError) -> Self {
        HybridError::Sparse(e)
    }
}

/// 密・疎それぞれの Top-k 結果を RRF で融合する純粋関数。
///
/// `dense`・`sparse` はそれぞれ長さが `cfg.pool_depth()` 以下であり（超過は
/// [`HybridError::TooManyCandidates`]。3 回目の codex-review P1 指摘対応）、
/// 呼び出し元の provider/index が定める順位契約（[`SearchHit`] はスコア降順・
/// 同点 id 昇順、[`ScoredDoc`] は同様の契約）に従って既にソート済みであることを
/// 前提とする。1-based 順位 `r` に対し `weight / (k_const + r)` を id ごとに加算する
/// （両リストに出現する id は和になる）。元のスコア値（内積・BM25）は使わず順位のみを
/// 使う（RRF の定義）。
///
/// 出力は融合スコア降順・同点は id 昇順（`f64::total_cmp` ベース）で確定する。
/// 各リストの長さが `cfg.pool_depth()` を超える場合は、後続のアロケーションを伴う
/// 検証（重複検査の `BTreeSet` 構築等）へ進む前に [`HybridError::TooManyCandidates`]
/// を返す（coding-rust.md「長さフィールドは上限検証してからアロケーションに使う」）。
/// 入力リスト内に同一 id が重複して出現した場合は [`HybridError::DuplicateId`] を返す
/// （provider・インデックス側の契約違反を fail-closed に検知する）。
/// 入力に非有限スコア（NaN・Inf）が含まれる場合、またはリスト自体は有限でも融合の
/// 加算結果が非有限（Inf）へオーバーフローした場合は [`HybridError::NonFiniteScore`]
/// を、ソート順契約に違反する場合は [`HybridError::UnsortedInput`] を返す。長さ・
/// 入力有限性・ソート順・重複はこの順で検証する（[`HybridError::NonFiniteScore`]・
/// [`HybridError::TooManyCandidates`] のドキュメント参照）。
pub fn rrf_fuse(
    dense: &[SearchHit],
    sparse: &[ScoredDoc],
    cfg: &RrfConfig,
) -> Result<Vec<HybridHit>, HybridError> {
    // 長さ検証を他のどの検証よりも先に行う。以降の検証（有限性・ソート順・重複）は
    // いずれも入力を線形走査し、重複検査（[`has_duplicate_id`]）は走査した分だけ
    // `BTreeSet` へ挿入するため、長さを検証せずに通すと契約違反の provider/index が
    // `cfg.pool_depth()`（高々 `MAX_POOL_DEPTH`）を大きく超える件数を返した場合に
    // 無制限にメモリ・CPU を消費できてしまう（[`HybridError::TooManyCandidates`]
    // のドキュメント参照）。
    if dense.len() > cfg.pool_depth() {
        return Err(HybridError::TooManyCandidates {
            len: dense.len(),
            max: cfg.pool_depth(),
        });
    }
    if sparse.len() > cfg.pool_depth() {
        return Err(HybridError::TooManyCandidates {
            len: sparse.len(),
            max: cfg.pool_depth(),
        });
    }

    // スコアの有限性を順序検証より先に確認する（[`HybridError::NonFiniteScore`] の
    // ドキュメント参照）。`f64::total_cmp` は NaN にもビットパターン依存の全順序を
    // 与えてしまうため、有限性を確認しないまま順序検証だけに頼ると NaN が偶然
    // 順序契約を満たすビットパターンで紛れ込んだ場合に検出できない
    // （`core.rs::EngineCore::search` の provider 結果検証と同じ検証順序）。
    if dense.iter().any(|h| !h.score.is_finite()) {
        return Err(HybridError::NonFiniteScore);
    }
    if sparse.iter().any(|d| !d.score.is_finite()) {
        return Err(HybridError::NonFiniteScore);
    }

    // RRF は元スコアを見ず順位のみを使うため、入力がソート済みであることをここで
    // 検証してから初めて信頼する（ドキュメントコメント・[`HybridError::UnsortedInput`]
    // 参照。ソート順を検証なしで信頼すると不正な順序が黙って誤った融合スコアを生む）。
    if !is_sorted_desc_id_asc(dense.iter().map(|h| (f64::from(h.score), h.id))) {
        return Err(HybridError::UnsortedInput);
    }
    if !is_sorted_desc_id_asc(sparse.iter().map(|d| (d.score, d.doc_id))) {
        return Err(HybridError::UnsortedInput);
    }

    // 重複 id 検査は入力リスト全体（`dense`・`sparse` それぞれの全件。長さ検証を
    // 通過済みのため高々 `cfg.pool_depth()` 件）に対して行う。同一 id・同一スコアの
    // 重複は `is_sorted_desc_id_asc` の「同点は id 昇順」判定を id が等しいまますり
    // 抜けるため（`id < prev_id` は偽になる）、順序検証だけでは重複を検出できない。
    if has_duplicate_id(dense.iter().map(|h| h.id)) {
        return Err(HybridError::DuplicateId);
    }
    if has_duplicate_id(sparse.iter().map(|d| d.doc_id)) {
        return Err(HybridError::DuplicateId);
    }

    // 融合マップの要素数は高々 `2 * pool_depth` に有界（`pool_depth` は
    // `RrfConfig::new` で検証済みの値のみが渡される契約）。id をキーにした
    // `BTreeMap` を使うことで、出現順・ハッシュ実装に依存しない決定的な走査順序を
    // 保証する（同点タイブレークの安定性に寄与）。
    let mut scores: BTreeMap<u64, f64> = BTreeMap::new();

    accumulate_ranked(
        dense.iter().map(|h| h.id),
        cfg.pool_depth(),
        cfg.k_const(),
        cfg.dense_weight(),
        &mut scores,
    );
    accumulate_ranked(
        sparse.iter().map(|d| d.doc_id),
        cfg.pool_depth(),
        cfg.k_const(),
        cfg.sparse_weight(),
        &mut scores,
    );

    // 融合後の有限性検証（3 回目の codex-review P1 指摘対応。[`HybridError::NonFiniteScore`]
    // のドキュメント参照）。`RrfConfig::new` は重み（`dense_weight`/`sparse_weight`）の
    // 有限性・正数のみを検証し上限は課さないため、個々の入力・加算前の各寄与
    // （`weight / (k_const + rank)`）が有限でも、同一 id が密・疎双方の上位順位に
    // 現れて寄与を加算した結果が `f64::MAX` を超えて `+Inf` へオーバーフローしうる。
    // 融合前の入力検証（有限性・ソート順・重複）だけでは検知できないため、加算後の
    // `scores` に対して独立に検証する。
    if scores.values().any(|score| !score.is_finite()) {
        return Err(HybridError::NonFiniteScore);
    }

    let mut out: Vec<HybridHit> = scores
        .into_iter()
        .map(|(id, score)| HybridHit { id, score })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// [`rrf_fuse`] の入力検証ヘルパ。`(score, id)` の列が「スコア降順、同点は id 昇順」
/// （[`SearchHit`]・[`ScoredDoc`] 双方のドキュメントが定める順位契約）に従っているかを
/// 判定する。`SearchHit`（`f32`）・`ScoredDoc`（`f64`）双方から `f64` へ変換して同一の
/// 判定ロジックを共有する（`f32` → `f64` は常に値を保存する拡大変換のため精度劣化なし）。
fn is_sorted_desc_id_asc(items: impl Iterator<Item = (f64, u64)>) -> bool {
    let mut prev: Option<(f64, u64)> = None;
    for (score, id) in items {
        if let Some((prev_score, prev_id)) = prev {
            // 直前の要素 (prev_score, prev_id) が現在の要素 (score, id) より前に
            // 来てよいのは、prev_score > score（スコア降順）、または同点で
            // prev_id <= id（同点は id 昇順）の場合のみ。それ以外は契約違反。
            let score_order = score.total_cmp(&prev_score);
            let violates = score_order == std::cmp::Ordering::Greater
                || (score_order == std::cmp::Ordering::Equal && id < prev_id);
            if violates {
                return false;
            }
        }
        prev = Some((score, id));
    }
    true
}

/// [`rrf_fuse`] の入力検証ヘルパ。`ids` の列（`dense`・`sparse` それぞれの全件。
/// `rrf_fuse` の長さ検証を通過済みのため高々 `cfg.pool_depth()` 件）に同一 id が
/// 複数回出現するかを判定する。[`accumulate_ranked`] 側では検査しない（[`rrf_fuse`]
/// が呼び出し元であり、有限性・ソート順の検証と同じ「全件」スコープで一度だけ
/// 検査する設計）。
fn has_duplicate_id(ids: impl Iterator<Item = u64>) -> bool {
    let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return true;
        }
    }
    false
}

/// [`rrf_fuse`] の内部ヘルパ。1 つのランク付き id 列（先頭 `pool_depth` 件）を
/// RRF スコアへ変換し、`scores` へ加算する。密・疎の両リストから同じロジックで
/// 呼ばれることで加算順序を一本化する。呼び出し元（[`rrf_fuse`]）が [`has_duplicate_id`]
/// で入力リスト全体の重複なしを事前に検証済みのため、`ids` の先頭 `pool_depth` 件も
/// 重複しないことが保証されており、本関数自体は重複検知を行わない。
fn accumulate_ranked(
    ids: impl Iterator<Item = u64>,
    pool_depth: usize,
    k_const: f64,
    weight: f64,
    scores: &mut BTreeMap<u64, f64>,
) {
    for (idx, id) in ids.take(pool_depth).enumerate() {
        // 1-based 順位。`idx` は `take(pool_depth)` により高々 `pool_depth - 1`
        // （`pool_depth <= MAX_POOL_DEPTH`）に収まるため `as f64` 変換で精度は失われない。
        let rank = (idx as f64) + 1.0;
        let contribution = weight / (k_const + rank);
        let entry = scores.entry(id).or_insert(0.0);
        *entry += contribution;
    }
}

/// 密検索 provider と疎検索インデックスを RRF で統合検索する入口
/// （CORE-3〜5 実装の密検索 provider・`sparse.rs` の疎検索との統合点）。
///
/// 密側は `provider.search()` を `k = cfg.pool_depth` で実行し（`input.k` は本関数が
/// 上書きするため呼び出し元の値は無視される）、疎側は
/// `sparse_index.search_within(query_text, cfg.pool_depth, &visible_ids)`
/// （[`crate::sparse::SparseIndex::search_within`]）を実行する。[`rrf_fuse`] で
/// 融合した後、先頭 `k` 件へ切り詰めて返す。
///
/// `k` は `1..=cfg.pool_depth()` を検証し、`0` または超過は [`HybridError::InvalidK`]。
/// `cfg.pool_depth()` 自体が `MAX_POOL_DEPTH` 以下であることは [`RrfConfig::new`] が
/// 構築時に保証済みのため、ここでの上限は常に `cfg.pool_depth()` を基準にする（密・疎
/// 双方とも融合対象は先頭 `cfg.pool_depth()` 件までしか取り込まれないため、`k` が
/// それを超えると要求された件数を満たせないまま静かに縮退する。上限を
/// `MAX_POOL_DEPTH` 固定にすると `cfg.pool_depth()` がそれより小さい既定構成
/// （`RrfConfig::default()` は 200）で `k` が縮退域に入っても検出できないため、
/// `cfg.pool_depth()` を基準にして fail-closed に拒否する）。
///
/// 契約: `input`（[`SearchInput`]）は `core.rs` と同じく「呼び出し元が可視行のみへ
/// 縮約済み」であることが前提であり、テナント境界はこの層より上で完結する（本関数は
/// 境界を弱めない）。密側は `provider`（`input` と別個の trait object であり
/// 「`input.ids` 外の id を返さない」ことは型では強制されない）の戻り値を検証し、
/// `input.ids`（可視集合）外の id が 1 件でも含まれていたら黙って除外せず
/// [`HybridError::ProviderResultRejected`] で検索全体を拒否する（モジュール
/// ドキュメント参照。事後フィルタだと不可視 id が `cfg.pool_depth()` の候補枠を
/// 占有した場合に可視ヒットを復元できず、結果件数の差から不可視データの有無が
/// 外部へ漏れうるため）。疎側も同じ理由で事後フィルタは使わず、
/// `sparse_index.search()` ではなく統計・候補選出を `visible_ids` へ縮約する
/// [`crate::sparse::SparseIndex::search_within`] を呼ぶ（モジュールドキュメント参照）。
///
/// 出力の順序契約: [`rrf_fuse`] が返す融合スコア降順・同点 id 昇順の順序をそのまま
/// 維持して `k` 件へ `truncate` する。RRF 同点グループ（同一融合スコアを持つ id 群）の
/// 途中で `k` 打ち切りが起こりうるが、その場合も含めタイブレークは常に id 昇順で
/// 決定的である（`fused` は `truncate` 前の時点で既に id 昇順にソート済みの同点グループ
/// を持つため、どの位置で打ち切っても採用される id 集合は再現可能）。
pub fn hybrid_search(
    provider: &dyn SearchProvider,
    input: SearchInput<'_>,
    sparse_index: &SparseIndex,
    query_text: &str,
    k: usize,
    cfg: &RrfConfig,
) -> Result<Vec<HybridHit>, HybridError> {
    if k == 0 || k > cfg.pool_depth() {
        return Err(HybridError::InvalidK);
    }

    let visible_ids: std::collections::BTreeSet<u64> = input.ids.iter().copied().collect();

    let dense_input = SearchInput {
        ids: input.ids,
        vectors: input.vectors,
        dim: input.dim,
        query: input.query,
        k: cfg.pool_depth(),
    };
    // `provider` は trait object（[`SearchProvider`]）であり、「`input.ids` 外の id を
    // 返さない」「要求した `k`（＝ `cfg.pool_depth()`）以下の件数しか返さない」ことは
    // いずれも型では強制されない（呼び出し元の実装ミス・バグの余地がある）。
    let dense_hits: Vec<SearchHit> = provider.search(dense_input)?;
    // 長さ検証を可視性走査より先に行う（3 回目の codex-review P1 指摘対応）。
    // `rrf_fuse` 自身も同じ長さ検証を行うが、ここで早期に拒否することで、契約違反
    // provider が `cfg.pool_depth()` を大きく超える件数を返した場合に直後の可視性
    // 走査（`.iter().any(...)`）が不要な O(n) コストを払わずに済む
    // （[`HybridError::TooManyCandidates`] のドキュメント参照）。
    if dense_hits.len() > cfg.pool_depth() {
        return Err(HybridError::TooManyCandidates {
            len: dense_hits.len(),
            max: cfg.pool_depth(),
        });
    }
    // 事後フィルタ（不可視 id だけを黙って除外する）はしない: 不可視 id が
    // `cfg.pool_depth()` の候補枠を占有していた場合、フィルタ後に可視ヒットを
    // 復元できず、結果件数の差から不可視データの有無が外部へ漏れる（2 回目の
    // codex-review P0 指摘対応。モジュールドキュメント参照）。1 件でも可視集合外の
    // id が含まれていたら検索全体を拒否する（fail-closed）。
    if dense_hits.iter().any(|hit| !visible_ids.contains(&hit.id)) {
        return Err(HybridError::ProviderResultRejected);
    }
    // 疎側は `sparse_index.search()`（インデックス全体を母数に統計・Top-k を計算する
    // API）ではなく `search_within()`（[`SparseIndex::search_within`]）を使う。
    // `search()` の後段フィルタ（旧実装）は「不可視文書が Top-k のプールを占有して
    // 可視文書を押し出す」「`doc_count`/`doc_freq` を通じて不可視文書の内容・存在が
    // 可視文書の順位へ影響する」という 2 つの経路でテナント境界を弱めてしまう
    // （後段フィルタでは統計計算・候補選出そのものへの影響を防げない。Issue #36
    // codex-review P0 指摘対応）。`search_within` は統計・Top-k 選出の両方を
    // `visible_ids` へ縮約した上で計算するため、この 2 経路をともに断つ。
    let sparse_hits: Vec<ScoredDoc> =
        sparse_index.search_within(query_text, cfg.pool_depth(), &visible_ids)?;

    let mut fused = rrf_fuse(&dense_hits, &sparse_hits, cfg)?;
    fused.truncate(k);
    Ok(fused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::CpuScalarProvider;
    use crate::sparse::SparseIndex;

    fn hit(id: u64, score: f32) -> SearchHit {
        SearchHit { id, score }
    }

    fn doc(doc_id: u64, score: f64) -> ScoredDoc {
        ScoredDoc { doc_id, score }
    }

    #[test]
    fn rrf_config_default_matches_expected_values() {
        let cfg = RrfConfig::default();
        assert_eq!(cfg.k_const(), 60.0);
        assert_eq!(cfg.dense_weight(), 1.0);
        assert_eq!(cfg.sparse_weight(), 1.0);
        assert_eq!(cfg.pool_depth(), 200);
    }

    #[test]
    fn rrf_config_rejects_invalid_values() {
        assert_eq!(
            RrfConfig::new(60.0, 1.0, 1.0, 0).unwrap_err(),
            HybridError::InvalidConfig
        );
        assert_eq!(
            RrfConfig::new(60.0, 1.0, 1.0, MAX_POOL_DEPTH + 1).unwrap_err(),
            HybridError::InvalidConfig
        );
        assert_eq!(
            RrfConfig::new(0.0, 1.0, 1.0, 10).unwrap_err(),
            HybridError::InvalidConfig
        );
        assert_eq!(
            RrfConfig::new(f64::NAN, 1.0, 1.0, 10).unwrap_err(),
            HybridError::InvalidConfig
        );
        assert_eq!(
            RrfConfig::new(60.0, -1.0, 1.0, 10).unwrap_err(),
            HybridError::InvalidConfig
        );
        assert_eq!(
            RrfConfig::new(60.0, 1.0, f64::INFINITY, 10).unwrap_err(),
            HybridError::InvalidConfig
        );
    }

    #[test]
    fn rrf_fuse_matches_hand_computed_scores_for_top_rank_overlap() {
        // 両リストとも 1 位が id=1。融合スコアは 2 / (k_const + 1)（等重み）。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).unwrap();
        let dense = [hit(1, 3.0), hit(2, 2.0)];
        let sparse = [doc(1, 5.0), doc(3, 4.0)];
        let fused = rrf_fuse(&dense, &sparse, &cfg).expect("fuse ok");

        let expected_id1 = 2.0 / (60.0 + 1.0);
        let id1 = fused.iter().find(|h| h.id == 1).expect("id=1 present");
        assert!((id1.score - expected_id1).abs() < 1e-12);

        let expected_id2 = 1.0 / (60.0 + 2.0);
        let id2 = fused.iter().find(|h| h.id == 2).expect("id=2 present");
        assert!((id2.score - expected_id2).abs() < 1e-12);

        let expected_id3 = 1.0 / (60.0 + 2.0);
        let id3 = fused.iter().find(|h| h.id == 3).expect("id=3 present");
        assert!((id3.score - expected_id3).abs() < 1e-12);
    }

    #[test]
    fn rrf_fuse_orders_by_score_descending_id_ascending_on_tie() {
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).unwrap();
        // id=5 と id=6 は両方 dense のみ 1 位・2 位ではなく、同一順位（1 位）を
        // それぞれ異なるリストで得て同点になるよう構成する。
        let dense = [hit(5, 1.0)];
        let sparse = [doc(6, 1.0)];
        let fused = rrf_fuse(&dense, &sparse, &cfg).expect("fuse ok");
        assert_eq!(fused.len(), 2);
        // 同点タイブレークは id 昇順。
        assert_eq!(fused[0].id, 5);
        assert_eq!(fused[1].id, 6);
        assert!((fused[0].score - fused[1].score).abs() < 1e-15);
    }

    #[test]
    fn rrf_fuse_applies_weights() {
        let cfg = RrfConfig::new(60.0, 2.0, 1.0, 10).unwrap();
        let dense = [hit(1, 1.0)];
        let sparse = [doc(2, 1.0)];
        let fused = rrf_fuse(&dense, &sparse, &cfg).expect("fuse ok");
        let id1 = fused.iter().find(|h| h.id == 1).unwrap();
        let id2 = fused.iter().find(|h| h.id == 2).unwrap();
        // dense_weight=2.0 のため id=1 のスコアは id=2 の 2 倍になる。
        assert!((id1.score - 2.0 * id2.score).abs() < 1e-15);
    }

    #[test]
    fn rrf_fuse_accepts_input_list_at_pool_depth_boundary() {
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 1).unwrap();
        let dense = [hit(1, 3.0)];
        let sparse: [ScoredDoc; 0] = [];
        let fused = rrf_fuse(&dense, &sparse, &cfg).expect("fuse ok");
        assert_eq!(
            fused,
            vec![HybridHit {
                id: 1,
                score: 1.0 / 61.0
            }]
        );
    }

    #[test]
    fn rrf_fuse_rejects_input_list_exceeding_pool_depth() {
        // [P1] レビュー指摘対応（3 回目の codex-review）: 以前は各リストの長さを
        // 検証せず `cfg.pool_depth()` を超える件数もそのまま `is_sorted_desc_id_asc`・
        // `has_duplicate_id`（`BTreeSet` へ全件挿入）へ通していたため、契約違反の
        // provider/index が巨大な結果を返すと無制限にメモリ・CPU を消費できた。
        // `pool_depth=1` に対し dense を 2 件（id は重複なし）渡すと、以前は先頭
        // 1 件だけを融合対象として静かに切り詰めていたが、現在は長さ超過そのものを
        // `TooManyCandidates` で拒否する。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 1).unwrap();
        let dense = [hit(1, 3.0), hit(2, 2.0)];
        let sparse: [ScoredDoc; 0] = [];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::TooManyCandidates { len: 2, max: 1 });
    }

    #[test]
    fn rrf_fuse_rejects_non_finite_dense_score() {
        // [P1] レビュー指摘対応: `f64::total_cmp` は NaN にもビットパターン依存の
        // 全順序を与えるため、有限性検証を欠くと NaN が順序検証をすり抜けうる。
        // dense 側に NaN を含む列は `NonFiniteScore` で拒否されるべき。
        let cfg = RrfConfig::default();
        let dense = [hit(1, f32::NAN)];
        let sparse: [ScoredDoc; 0] = [];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::NonFiniteScore);
    }

    #[test]
    fn rrf_fuse_rejects_non_finite_sparse_score() {
        let cfg = RrfConfig::default();
        let dense: [SearchHit; 0] = [];
        let sparse = [doc(1, f64::INFINITY)];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::NonFiniteScore);
    }

    #[test]
    fn rrf_fuse_rejects_non_finite_score_before_checking_sort_order() {
        // 非有限スコアが先頭に混ざり、かつ列全体としては（非有限値を除けば）順序
        // 契約に違反していないケースでも、有限性検証が先に走るため `UnsortedInput`
        // ではなく `NonFiniteScore` を返すことを確認する（検証順序の固定）。
        let cfg = RrfConfig::default();
        let dense = [hit(1, f32::NAN), hit(2, 1.0)];
        let sparse: [ScoredDoc; 0] = [];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::NonFiniteScore);
    }

    #[test]
    fn rrf_fuse_rejects_score_that_overflows_to_infinity_after_fusion() {
        // [P1] レビュー指摘対応（3 回目の codex-review）: `RrfConfig::new` は重みの
        // 有限性・正数のみを検証し上限は課さないため、密・疎双方の重みへ `f64::MAX`
        // 近傍を指定して同一 id を両リストの 1 位に置くと、個々の寄与
        // （`weight / (k_const + 1)`）は有限でも加算後の合計が `f64::MAX` を超えて
        // `+Inf` になりうる。融合前の入力（`dense`/`sparse` それぞれのスコア）は
        // 有限のままであり、`rrf_fuse` 冒頭の入力有限性検証だけでは検知できない
        // （融合後の加算結果に対する検証で初めて検知できることを確認する）。
        let k_const = 1e-300; // rank=1 の分母 (k_const + 1) をほぼ 1 にし、寄与を weight に近づける。
        let cfg = RrfConfig::new(k_const, f64::MAX, f64::MAX, 1).unwrap();
        let dense = [hit(1, 1.0)];
        let sparse = [doc(1, 1.0)];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::NonFiniteScore);
    }

    #[test]
    fn rrf_fuse_rejects_duplicate_id_within_a_list() {
        let cfg = RrfConfig::default();
        let dense = [hit(1, 3.0), hit(1, 2.0)];
        let sparse: [ScoredDoc; 0] = [];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::DuplicateId);
    }

    #[test]
    fn rrf_fuse_rejects_duplicate_id_within_pool_depth_bound() {
        // [P1] レビュー指摘対応（初回）: 重複 id 検査は入力リストの全件（長さ検証
        // 通過後は高々 `cfg.pool_depth()` 件）に対して行われるべきで、
        // `accumulate_ranked` の走査窓だけに限定してはならない。長さ検証
        // （3 回目の codex-review P1 指摘対応。[`rrf_fuse_rejects_input_list_exceeding_pool_depth`]
        // 参照）により `cfg.pool_depth()` を超える入力はこの検証へ到達する前に
        // `TooManyCandidates` で拒否されるため、`pool_depth` 件ちょうどの範囲内に
        // 重複がある入力で検証する。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 2).unwrap();
        let dense = [hit(1, 5.0), hit(1, 3.0)];
        let sparse: [ScoredDoc; 0] = [];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::DuplicateId);
    }

    #[test]
    fn rrf_fuse_rejects_input_list_exceeding_pool_depth_before_checking_duplicates() {
        // 長さ検証は重複検査より先に行われる（検証順序の固定）。`pool_depth` を
        // 超える長さの入力に重複 id が含まれていても、返るエラーは `DuplicateId`
        // ではなく `TooManyCandidates` である。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 1).unwrap();
        let dense = [hit(1, 5.0), hit(2, 4.0), hit(1, 3.0)];
        let sparse: [ScoredDoc; 0] = [];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::TooManyCandidates { len: 3, max: 1 });
    }

    #[test]
    fn rrf_fuse_rejects_dense_input_not_sorted_by_score_descending() {
        let cfg = RrfConfig::default();
        // スコア昇順（本来は降順であるべき契約に違反）。
        let dense = [hit(1, 1.0), hit(2, 2.0)];
        let sparse: [ScoredDoc; 0] = [];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::UnsortedInput);
    }

    #[test]
    fn rrf_fuse_rejects_sparse_input_with_tie_break_id_descending() {
        let cfg = RrfConfig::default();
        // 同点スコアだが id が降順（本来は id 昇順であるべき契約に違反）。
        let dense: [SearchHit; 0] = [];
        let sparse = [doc(2, 1.0), doc(1, 1.0)];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::UnsortedInput);
    }

    #[test]
    fn rrf_fuse_accepts_correctly_tie_broken_input() {
        let cfg = RrfConfig::default();
        // 同点スコアで id 昇順（契約通り）。
        let dense: [SearchHit; 0] = [];
        let sparse = [doc(1, 1.0), doc(2, 1.0)];
        let fused = rrf_fuse(&dense, &sparse, &cfg).expect("fuse ok");
        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn hybrid_search_rejects_k_zero_and_over_limit() {
        let cfg = RrfConfig::default();
        // `SparseIndex::build` は空コーパスを受け付けないため、ダミー文書 1 件で
        // 構築する（本テストの関心事は `k` の境界検証であり、疎検索の内容自体は
        // 使われない）。
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids: [u64; 0] = [];
        let vectors: [f32; 0] = [];
        let query: [f32; 0] = [];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 0,
            query: &query,
            k: 1,
        };
        let err = hybrid_search(&CpuScalarProvider, input, &index, "q", 0, &cfg).unwrap_err();
        assert_eq!(err, HybridError::InvalidK);

        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 0,
            query: &query,
            k: 1,
        };
        let err = hybrid_search(
            &CpuScalarProvider,
            input,
            &index,
            "q",
            MAX_POOL_DEPTH + 1,
            &cfg,
        )
        .unwrap_err();
        assert_eq!(err, HybridError::InvalidK);
    }

    #[test]
    fn hybrid_search_rejects_k_over_pool_depth_even_within_max_pool_depth() {
        // [Medium] レビュー指摘対応: `k` の検証は `MAX_POOL_DEPTH` 固定ではなく
        // `cfg.pool_depth()` を基準にする。`pool_depth=1`（`MAX_POOL_DEPTH` 未満）の
        // 構成で `k=2` を渡すと、密・疎とも融合対象は先頭 1 件しか取り込まれず
        // 要求件数を満たせないまま静かに縮退しうるため、`InvalidK` で拒否する。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 1).unwrap();
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids: [u64; 0] = [];
        let vectors: [f32; 0] = [];
        let query: [f32; 0] = [];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 0,
            query: &query,
            k: 1,
        };
        let err = hybrid_search(&CpuScalarProvider, input, &index, "q", 2, &cfg).unwrap_err();
        assert_eq!(err, HybridError::InvalidK);
    }

    #[test]
    fn hybrid_search_on_empty_corpus_returns_empty() {
        let cfg = RrfConfig::default();
        // 密側は行を持たず（`ids`/`vectors` が空）、疎側もクエリと一致しない
        // ダミー文書のみ（BM25 スコアは 0 になり候補から除外される）。融合結果は
        // 空になるはず。
        let index = SparseIndex::build(&[(1, "unrelated content")]).expect("build ok");
        let ids: [u64; 0] = [];
        let vectors: [f32; 0] = [];
        let query: [f32; 0] = [];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 0,
            query: &query,
            k: 1,
        };
        let hits =
            hybrid_search(&CpuScalarProvider, input, &index, "nomatch", 5, &cfg).expect("ok");
        assert!(hits.is_empty());
    }

    #[test]
    fn hybrid_search_propagates_kernel_error() {
        let cfg = RrfConfig::default();
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids = [1u64];
        let vectors = [1.0f32, 0.0];
        // dim=2 だが query は 3 要素 → DimMismatch。
        let query = [1.0f32, 0.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 1,
        };
        let err = hybrid_search(&CpuScalarProvider, input, &index, "q", 5, &cfg).unwrap_err();
        assert!(matches!(
            err,
            HybridError::Kernel(KernelError::DimMismatch { .. })
        ));
    }

    #[test]
    fn hybrid_search_propagates_sparse_error() {
        // [Low] レビュー指摘対応: `HybridError::Sparse`（`From<SparseError>` 経由の
        // 伝播）を検証するテストが未網羅だったため追加する。`MAX_QUERY_BYTES`
        // （sparse.rs）を超えるクエリ文字列で `SparseError::QueryTooLong` を誘発する。
        let cfg = RrfConfig::default();
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids = [1u64];
        let vectors = [1.0f32];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 1,
        };
        // sparse.rs::MAX_QUERY_BYTES（16 KiB）を超える長さのクエリ文字列。
        let long_query = "a".repeat(17 * 1024);
        let err =
            hybrid_search(&CpuScalarProvider, input, &index, &long_query, 5, &cfg).unwrap_err();
        assert!(matches!(
            err,
            HybridError::Sparse(SparseError::QueryTooLong { .. })
        ));
    }

    /// [`SearchProvider`] の契約違反（`input.ids` に含まれない id を返す実装バグ）を
    /// 模したモック provider。`hybrid_search` が密検索側の契約違反を検出して
    /// fail-closed に拒否することを検証するために使う（`input` の中身は無視して
    /// 固定の [`SearchHit`] 列を返す）。
    struct LeakyProvider;
    impl SearchProvider for LeakyProvider {
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
            // id=1（可視）と id=99（不可視のはず）の両方を、スコア降順・同点 id 昇順の
            // 順位契約を満たしたまま返す（provider の順位契約自体には違反していない。
            // あくまで可視集合の境界を無視した契約違反のみを模す）。
            Ok(vec![
                SearchHit { id: 1, score: 2.0 },
                SearchHit { id: 99, score: 1.0 },
            ])
        }
    }

    #[test]
    fn hybrid_search_rejects_dense_provider_returning_invisible_id() {
        // [P0] レビュー指摘対応（テナント境界。2 回目の codex-review）:
        // `provider.search()` が `input.ids`（可視集合）外の id を返す契約違反を
        // 起こした場合、以前は黙って不可視 id だけを除外していたが、これだと
        // 不可視 id が `cfg.pool_depth()` の候補枠を占有していたケースで可視ヒットを
        // 復元できず、結果件数の差から不可視データの有無が外部へ漏れうる。
        // `hybrid_search` は検索全体を `ProviderResultRejected` で拒否すべき。
        // `SearchProvider` は trait object のため「可視集合外の id を返さない」ことは
        // 型では強制されず、fail-closed な検証で担保する必要がある。
        let cfg = RrfConfig::default();
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids = [1u64];
        let vectors = [1.0f32];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 1,
        };
        let err = hybrid_search(&LeakyProvider, input, &index, "nomatch", 10, &cfg).unwrap_err();
        assert_eq!(err, HybridError::ProviderResultRejected);
    }

    /// [`SearchProvider`] の契約違反（要求した `input.k` を超える件数を返す実装バグ）を
    /// 模したモック provider。返す id はすべて可視集合内かつ順位契約
    /// （スコア降順・重複なし）を満たすため、契約違反は「件数」のみに限定される。
    struct OverflowingProvider;
    impl SearchProvider for OverflowingProvider {
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
            Ok(vec![
                SearchHit { id: 1, score: 3.0 },
                SearchHit { id: 2, score: 2.0 },
                SearchHit { id: 3, score: 1.0 },
            ])
        }
    }

    #[test]
    fn hybrid_search_rejects_dense_provider_returning_more_hits_than_pool_depth() {
        // [P1] レビュー指摘対応（3 回目の codex-review）: `provider.search()` が
        // 要求した `k`（＝ `cfg.pool_depth()`）を超える件数を返す契約違反を起こした
        // 場合、`hybrid_search` 経由でも `rrf_fuse` の長さ検証（`TooManyCandidates`）
        // を通ることを確認する。`pool_depth=1` に対し 3 件（すべて可視・順位契約は
        // 満たす）を返す provider を使う。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 1).unwrap();
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids = [1u64, 2, 3];
        let vectors = [1.0f32, 1.0, 1.0];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 1,
        };
        let err =
            hybrid_search(&OverflowingProvider, input, &index, "nomatch", 1, &cfg).unwrap_err();
        assert_eq!(err, HybridError::TooManyCandidates { len: 3, max: 1 });
    }

    #[test]
    fn hybrid_search_truncates_dense_candidates_exceeding_pool_depth() {
        // [Low] レビュー指摘対応: `accumulate_ranked` の `take(pool_depth)` が、
        // 実 provider（`CpuScalarProvider`）が `pool_depth` を超える現実的な候補数を
        // 持つケースでも機能することを統合レベルで確認する。
        // `pool_depth=3` に対し可視行を 10 件用意し、dense 側の Top-k
        // （`k = cfg.pool_depth()` で呼ばれる）が 3 件に絞られたうえで融合されることを
        // 検証する（dim=1・スコアは id と同値になるよう構成し、上位 3 件が
        // id=10,9,8 であることを固定値で確認する）。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 3).unwrap();
        let ids: Vec<u64> = (1..=10).collect();
        let vectors: Vec<f32> = ids.iter().map(|&id| id as f32).collect();
        let query = [1.0f32];
        let docs: Vec<(u64, &str)> = vec![(1, "unrelated")];
        let index = SparseIndex::build(&docs).expect("build ok");
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 10,
        };
        let hits =
            hybrid_search(&CpuScalarProvider, input, &index, "nomatch", 3, &cfg).expect("ok");
        let returned_ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(
            returned_ids,
            vec![10, 9, 8],
            "dense pool must be truncated to pool_depth=3 (top-3 by dot-product score)"
        );
    }

    #[test]
    fn hybrid_search_filters_sparse_hits_to_visible_ids() {
        // [Medium] レビュー指摘対応（テナント境界）: `sparse_index` が `input.ids`
        // より広い集合（他テナントの文書を含みうる集合）から構築されていても、
        // `input.ids` に含まれない id は疎検索側でヒットしても結果へ混入しない
        // ことを確認する。id=99 は `sparse_index` には存在するが `input.ids`
        // （可視集合）には含まれない。
        //
        // [P2-1] レビュー指摘対応: 従来は dense 側が id=1 に必ずヒットする構成のため、
        // 疎側フィルタが no-op（機能していない）でも `h.id != 99` の緩い assert が
        // 偶然通り得る作りだった。`id=1` は密・疎の両方で 1 位ヒットする構成（両者の
        // BM25 スコアが同点になる id=1・id=99 のうち可視な id=1 のみが疎側の融合対象に
        // 残る）にしたうえで、[`hybrid_search_filters_dense_hits_to_visible_ids`] と
        // 同様に融合スコアの厳密一致まで検証し、疎側フィルタが実際に効いていること
        // （効いていなければ id=99 も融合されスコア・件数が変わる）を担保する。
        let cfg = RrfConfig::default();
        let index =
            SparseIndex::build(&[(1, "shared term"), (99, "shared term")]).expect("build ok");
        let ids = [1u64];
        let vectors = [1.0f32];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 1,
        };
        let hits =
            hybrid_search(&CpuScalarProvider, input, &index, "shared term", 10, &cfg).expect("ok");
        // dense 側は id=1（dot product = 1.0*1.0 = 1.0）のみが 1 位、疎側は id=1・id=99
        // が同点 1 位だが id=99 は可視集合外のためフィルタで除外され、id=1 のみが
        // 疎側の融合対象として残る。両リストとも id=1 が rank=1 のため、融合スコアは
        // `dense_weight/(k_const+1) + sparse_weight/(k_const+1) = 2.0/61.0`
        // （既定 `RrfConfig`: k_const=60.0・dense_weight=sparse_weight=1.0）に確定する。
        // 疎側フィルタが機能していなければ id=99 も融合されて件数・スコアが変わる。
        assert_eq!(
            hits,
            vec![HybridHit {
                id: 1,
                score: 2.0 / 61.0
            }]
        );
        assert!(
            hits.iter().all(|h| h.id != 99),
            "invisible id must not leak into hybrid results: {hits:?}"
        );
    }

    #[test]
    fn hybrid_search_sparse_side_does_not_let_invisible_docs_occupy_the_pool() {
        // [P0] レビュー指摘対応（Issue #36 codex-review）: `hybrid_search` が
        // `SparseIndex::search()`（インデックス全体を母数に Top-k を選出する旧経路）を
        // 呼んでいた場合、`pool_depth` が小さいと不可視文書が疎側の Top-k プールを
        // 独占し、事後フィルタでは可視文書を復元できなかった（`sparse.rs` の
        // `search_within_excludes_invisible_docs_from_pool_occupation` 参照）。
        // `pool_depth=1` の狭いプールに対し、可視文書 id=1 は "cat" 1 回のみだが、
        // sparse_index 全体には id=1 を除きすべて "cat" を大量に繰り返す不可視文書
        // （id=2〜11）が存在し、`search()` なら Top-1 を独占する構成にする。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 1).unwrap();
        let mut docs: Vec<(u64, &str)> = vec![(1, "cat")];
        let filler: Vec<(u64, &str)> = (2u64..=11)
            .map(|id| (id, "cat cat cat cat cat cat cat cat cat cat"))
            .collect();
        docs.extend(filler.iter().copied());
        let index = SparseIndex::build(&docs).expect("build ok");

        let ids = [1u64];
        let vectors = [1.0f32];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 1,
        };
        let hits = hybrid_search(&CpuScalarProvider, input, &index, "cat", 1, &cfg).expect("ok");
        // [Cursor Medium] レビュー指摘対応: 件数・id だけの緩い assert では、旧経路
        // （`search()` + 事後フィルタ）でも「たまたま」空でない結果を返すケースを
        // 見逃しうる。可視集合は id=1 のみ（密側も疎側も id=1 だけが候補になりうる）
        // ため、両リストとも id=1 が rank=1 になり、融合スコアは
        // `dense_weight/(k_const+1) + sparse_weight/(k_const+1) = 2.0/61.0`
        // （既定 `RrfConfig`: k_const=60.0・dense_weight=sparse_weight=1.0。RRF は
        // 元のスコア値ではなく順位のみを使うため BM25 スコアの実値には依存しない）
        // に確定する。旧経路（`search()` + 事後フィルタ）であれば疎側の Top-1
        // プールを不可視文書が占有し、可視文書 id=1 が疎側の融合対象からこぼれ落ちて
        // 空 or 異なるスコアの結果になる。
        assert_eq!(
            hits,
            vec![HybridHit {
                id: 1,
                score: 2.0 / 61.0
            }]
        );
    }
}
