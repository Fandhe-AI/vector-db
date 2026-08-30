//! 密検索・疎検索の RRF（Reciprocal Rank Fusion）融合モジュール（TASK-103。対象ビヘイビア:
//! SEARCH-1, SEARCH-3）。
//!
//! `kernel.rs`/`parallel_search.rs`（TASK-124・TASK-126）が提供する密検索 provider
//! （[`crate::kernel::SearchProvider`]）と、`sparse.rs`（TASK-102）が提供する疎検索
//! （[`crate::sparse::SparseIndex`]）は独立に存在する。本モジュールはその 2 系統を
//! RRF（公知のランク融合手法。各リストの順位を使い `weight / (k_const + rank)` を
//! id ごとに加算する。スコアの大小そのものは使わない）で統合する、純粋関数的な層と
//! して追加する。
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
//!
//! TASK-111（対象ビヘイビア: PLAN-1。関連: EXT-4）: `query_planner.rs`（TASK-110）が
//! 返す `QueryExpansion::path_hint`/`kind_hint` を、[`rrf_fuse`] 融合後の候補スコアへの
//! 小さな加点として反映する「ソフトブースト」機構（[`BoostRule`]・[`apply_soft_boost`]・
//! [`hybrid_search_boosted`]）を追加する。設計上の要点は「ハードフィルタ化しない」こと:
//! ヒント一致は候補の除外・絞り込みには一切使わず、[`truncate(k)`](Vec::truncate) で
//! 切り詰める**前**の融合済みプールへスコア加点のみを行い再順位付けする。候補集合の
//! 要素数はブースト前後で不変（メタデータ一致ブーストの共通基盤として EXT-4 でも
//! 再利用される設計であり、ヒント種別に依存しない `BoostRule` として実装する）。
//! `path_hint`/`kind_hint` は LLM 由来の untrusted 出力のため、一致判定
//! （[`path_hint_matches`]/[`kind_hint_matches`]）は部分文字列一致・完全一致のみに
//! 限定し、正規表現・glob は使わない（ReDoS の余地を作らない）。ヒント文字列・パスの
//! 内容自体は本モジュールのエラー・ログへ含めない。
//!
//! TASK-148（対象ビヘイビア: EXT-4。ポインタ: `docs/spec/05-tasks.md` TASK-148・
//! `docs/spec/04-behavior/extensions.md` EXT-4）: [`crate::scoring_boost`] が本モジュールの
//! [`BoostRule`]/[`apply_soft_boost`] を一般化する（加点の意味論そのものは本モジュールへ
//! 一元化したまま変更しない。詳細は `scoring_boost.rs` モジュールドキュメント参照）。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider};
use crate::sparse::{ScoredDoc, SparseError, SparseIndex};

/// 検索プールの深さ・k の上限。`core.rs::MAX_SEARCH_K`（同じく 10_000）と同桁を採用し、
/// 未検証の巨大な値がそのままアロケーションサイズへ伝播することを防ぐ
/// （coding-rust.md「無制限確保禁止」）。
pub(crate) const MAX_POOL_DEPTH: usize = 10_000;

/// [`hybrid_search_boosted`] の境界同点グループ完全化（Issue #310・Issue #320）が
/// 再取得（`fetch_k` を倍増しての再フェッチ）で終端確定を試みる際の `fetch_k` 上限
/// （Issue #320 codex-review P1 指摘対応）。`pool_depth` 境界のグループが `fetch_k`
/// 末尾まで同点で終端確定できない場合、位置ベースの `pool_depth` 件切り詰め
/// （ID 昇順で最小 ID だけが生き残る id 依存）はせず、`fetch_k` を倍増して再取得する。
/// この再取得は無制限には続けられない（coding-rust.md「無制限確保禁止」）ため、
/// [`MAX_POOL_DEPTH`] を基準に上限を定める。[`MAX_POOL_DEPTH`] の 4 倍を絶対上限
/// とし、これを [`rrf_fuse_with_limits`] の長さ上限にもそのまま使う（境界完全化で
/// `fetch_k` を超えて保持することはないため、融合対象の長さ上限を `fetch_k` の
/// 上限と揃えれば整合する）。可視集合が本上限より小さい構成では、通常の初期
/// `fetch_k = pool_depth * 2` から可視集合の大きさに達するまで（本上限より先に）
/// 倍増を繰り返す（可視集合サイズに達した時点で `exhaustive` が確定するため、
/// それ以上の倍増は起こらない）。可視集合が本上限以上に大きい構成でのみ、
/// 倍増が本上限で打ち切られうる。
pub(crate) const MAX_FETCH_K: usize = MAX_POOL_DEPTH * 4;

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
    tie_rank: TieRank,
}

/// RRF 融合（[`accumulate_ranked`]）で同点グループへ割り当てる順位の規約
/// （Issue #310・TASK-84・対象ビヘイビア SEARCH-1/SEARCH-3。id 依存のノイズを
/// 除去する目的で追加。詳細な導出は `docs/design/hybrid-recall-regression.md`
/// 「Issue #310: engine 側改善」節を参照）。
///
/// `#[non_exhaustive]` にすることで、将来バリアントを追加しても呼び出し側の
/// 網羅的 `match` を壊さず、かつ構造体リテラル同様の外部からの想定外構築を防ぐ
/// （[`RrfConfig`] と同じ fail-closed 方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TieRank {
    /// 位置順位（従来挙動）。同点グループ内でも列に現れる位置をそのまま順位にする。
    /// 列の並び順（同点は候補識別子昇順。[`is_sorted_desc_id_asc`] が検証する契約）に
    /// 依存するため、同点グループが大きいほど識別子の小さい候補が根拠なく高順位を
    /// 得る（Issue #310 で確認した id 依存バイアス）。
    Positional,
    /// グループ末尾順位（modified competition ranking）。同点グループの全要素へ、
    /// そのグループの列内での末尾位置（1-based）を順位として割り当てる。グループが
    /// 大きいほど寄与が小さくなるため、同点グループの大きさ自体を「不確実性」と
    /// みなす規約になる。[`RrfConfig::default`]・[`RrfConfig::new`] の既定。
    GroupEnd,
}

impl Default for RrfConfig {
    fn default() -> Self {
        Self {
            k_const: 60.0,
            dense_weight: 1.0,
            sparse_weight: 1.0,
            pool_depth: 200,
            tie_rank: TieRank::GroupEnd,
        }
    }
}

impl RrfConfig {
    /// 検証付きコンストラクタ。`pool_depth` は `1..=MAX_POOL_DEPTH`、`k_const`・重みは
    /// 有限かつ正であることを構築時に検証し、違反は `Err`（fail-closed）。
    /// フィールドが非 `pub` のため、`RrfConfig` を構築する経路はこの関数（と
    /// 常に妥当な値を返す [`Default::default`]）のみに限定される。これにより
    /// [`rrf_fuse`]・[`hybrid_search`] は妥当性検証済みの設定のみを扱える。
    /// `tie_rank` は [`TieRank::GroupEnd`]（既定）で構築する。異なる規約が必要な
    /// 場合は [`RrfConfig::with_tie_rank`] で明示的に切り替える。
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
            tie_rank: TieRank::GroupEnd,
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

    /// 同点グループへ割り当てる順位の規約（既定 [`TieRank::GroupEnd`]）。
    pub fn tie_rank(&self) -> TieRank {
        self.tie_rank
    }

    /// `tie_rank` を明示的に切り替えた新しい設定を返す（builder 風 API。他フィールドは
    /// 検証済みのまま変更しない）。Issue #310 の規約変更を撤回する場合は
    /// `with_tie_rank(TieRank::Positional)` で従来挙動へ 1 行で復帰できる。
    pub fn with_tie_rank(mut self, tie_rank: TieRank) -> Self {
        self.tie_rank = tie_rank;
        self
    }
}

/// 融合後の検索結果 1 件（行 ID と RRF スコア）。
///
/// `crate::kernel::CandidateHit`（`score: f32`、内積スコア）・`crate::sparse::ScoredDoc`
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
    /// 融合対象の入力リスト（密・疎いずれか）の長さが呼び出し元の指定する上限
    /// （[`rrf_fuse`] からは `cfg.pool_depth()`、[`hybrid_search_boosted`] の境界
    /// 同点グループ完全化からは再取得後の `fetch_k`。いずれも高々 [`MAX_FETCH_K`]）を
    /// 超えていた（3 回目の codex-review P1 指摘対応）。以前は長さそのものを検証せず
    /// 全件を [`is_sorted_desc_id_asc`]・重複検査（`BTreeSet` への全件挿入）に通して
    /// いたため、契約違反の provider・呼び出し元が上限を大きく超える件数を返すと
    /// その分だけ無制限にメモリ・CPU を消費できた（coding-rust.md「無制限確保禁止」
    /// 違反）。融合前（有限性・ソート順・重複検査より先）に長さを検証し、超過は
    /// fail-closed に拒否する。
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
    /// [`BoostRule::new`] の検証違反（`amount` が非有限、または `0.0 < amount <=
    /// MAX_BOOST_AMOUNT` の範囲外。TASK-111）。ヒントが誤っていてもハードフィルタで
    /// 候補を除外しない設計上、加点自体は検証付きコンストラクタでのみ有界化する
    /// （不正な `amount` を通すと [`NonFiniteScore`](HybridError::NonFiniteScore) を
    /// 事後検知に頼ることになり fail-open な経路が広がる）。
    InvalidBoost,
    /// [`BoostRule::new`] に渡された `ids`（1 ルールの一致対象識別子集合）の要素数が
    /// [`MAX_BOOST_IDS`] を超えた（TASK-111。PR #257 codex-review P2 指摘対応）。
    /// [`InvalidBoost`](HybridError::InvalidBoost) は `amount` の値域検証専用の
    /// バリアントのため、`ids` のサイズ検証には別のバリアントを設ける（[`rrf_fuse`]
    /// の [`TooManyCandidates`](HybridError::TooManyCandidates) と同じ「アロケーション・
    /// 走査前に長さを検証する」順序を踏襲する）。
    TooManyBoostIds { len: usize, max: usize },
    /// [`apply_soft_boost`] に渡された `rules` の件数が [`MAX_BOOST_RULES`] を超えた
    /// （TASK-111）。[`rrf_fuse`] の [`TooManyCandidates`](HybridError::TooManyCandidates)
    /// と同じ「アロケーション・走査前に長さを検証する」順序を踏襲する。
    TooManyBoostRules { len: usize, max: usize },
    /// ある候補（`total`。実際に一致した `rules` の加点合計）が、[`apply_soft_boost`]
    /// がその候補に許容する加点合計の上限（`max`）以上だった（TASK-111。3 回目の
    /// codex-review P1 指摘・cursor bot「Boost error contract is inconsistent」
    /// 指摘対応、および PR #257 codex-review 指摘〔`max` を候補の元スコア込みで
    /// 判定するよう修正〕対応）。`max` は候補ごとに異なりうる（[`soft_boost_confirm_cap`]
    /// のドキュメント参照）: 候補の加点前スコア（`hit.score`）が
    /// [`soft_boost_confirm_cap`] 未満の場合のみ、その上限から `hit.score`（候補の
    /// 元スコア）を差し引いた残余を `max` として算出し、`hit.score` が
    /// [`soft_boost_confirm_cap`] 以上の候補（元スコア単独で既に保証下限相当の
    /// 近接順位級）はこの確定判定の対象外（本エラーを返さない）。
    ///
    /// 過去の実装（2 回目の codex-review P1 指摘対応）は `max` を「その候補の
    /// 実スコアから真の 1 位（融合プール `hits` の実際の最高スコア）までの差」
    /// として候補ごとに算出しており、真の 1 位を追い越す加点を一律拒否していた。
    /// しかしこれは近接順位を入れ替えるというソフトブースト本来の用途（PLAN-1 の
    /// 意図）そのものを検索エラーにしてしまう過剰拒否だった: 既定 RRF の単一
    /// チャネルでの実測順位差は僅か（例: 約 0.000264）であり、公開済み既定値
    /// [`SOFT_BOOST_PER_MATCH`]（`0.0007`）を 1 件適用しただけの通常利用でも
    /// この僅差を上回ってしまい `hybrid_search_boosted` が失敗していた。真の
    /// 1 位を追い越して新たな 1 位になること自体は正常な結果であり拒否すべき
    /// ではないため、`max` は実データの順位関係（真の 1 位との margin）とは無関係の
    /// 絶対値（[`soft_boost_confirm_cap`]。`min(dense_weight, sparse_weight) /
    /// (k_const + 1)`）とする。`MAX_BOOST_AMOUNT` は [`BoostRule::new`] 単体では
    /// 「有限かつ極端でない」ことしか保証できず、実際にどこまで安全かは `cfg`
    /// （[`RrfConfig`]）に依存するため、加点の適用時点（[`apply_soft_boost`]）で
    /// `cfg` に対して動的に検証する。`min` を使うのは、重みが大きい方のチャネルが
    /// クエリ不一致で空になるケース（`tests::
    /// hybrid_search_boosted_rejects_boost_when_heavier_channel_is_empty`）でも
    /// 安全側（弱い方のチャネルの寄与だけを仮定する）に倒すためで、`max` を使うと
    /// 本来拒否すべき加点を通してしまい危険（過去 2 回目の codex-review P1
    /// 指摘の核心だった「重みが大きい方のチャネルに必ず 1 位候補があると誤仮定
    /// する」問題の再発）。
    BoostSoftBoundExceeded { total: f64, max: f64 },
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
            HybridError::InvalidBoost => write!(f, "invalid soft boost rule amount"),
            HybridError::TooManyBoostIds { len, max } => {
                write!(f, "too many ids in soft boost rule: {len} ids (max {max})")
            }
            HybridError::TooManyBoostRules { len, max } => {
                write!(f, "too many soft boost rules: {len} rules (max {max})")
            }
            HybridError::BoostSoftBoundExceeded { total, max } => {
                write!(
                    f,
                    "candidate soft boost total {total} at or above the absolute soft boost cap {max}"
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
/// 呼び出し元の provider/index が定める順位契約（[`CandidateHit`] はスコア降順・
/// 同点 id 昇順、[`ScoredDoc`] は同様の契約）に従って既にソート済みであることを
/// 前提とする。1-based 順位 `r`（列に現れる位置。同点は候補識別子昇順の位置が
/// そのまま順位になる）に対し `weight / (k_const + r)` を id ごとに加算する
/// （両リストに出現する id は和になる）。スコアの大小そのもの（内積・BM25 の実値）は
/// 使わない（RRF の定義。同点時の順位規約は Issue #307・SEARCH-1・SEARCH-3 の
/// 原因調査対象であり、詳細は `docs/design/hybrid-recall-regression.md`
/// 「小規模段ゲート未達の engine 側原因調査（Issue #307）」節を参照）。
///
/// 出力は融合スコア降順・同点は**候補識別子**の昇順（`f64::total_cmp` ベース）で確定する
/// （識別子は呼び出し元定義。`sql/exec.rs` はアリーナのスロット番号を渡すため実質
/// `(tenant_id, id)` 昇順になる。`docs/design/rrf-tie-break-determinism.md` 参照）。
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
    dense: &[CandidateHit],
    sparse: &[ScoredDoc],
    cfg: &RrfConfig,
) -> Result<Vec<HybridHit>, HybridError> {
    rrf_fuse_with_limits(dense, cfg.pool_depth(), sparse, cfg.pool_depth(), cfg)
}

/// [`rrf_fuse`] の内部実装（`pub(crate)`）。密・疎それぞれの長さ上限を独立に指定できる
/// 薄い拡張版で、密側のみ `cfg.pool_depth()` を超えて受理したい
/// [`hybrid_search_boosted`] の境界同点グループ完全化（Issue #310。
/// [`complete_boundary_tie_group`] 参照）から呼ばれる。公開 API [`rrf_fuse`] は
/// `dense_limit = sparse_limit = cfg.pool_depth()` で呼ぶだけの契約維持ラッパで、
/// 既存の検証順序・エラー種別は変えない。
///
/// `dense_limit`（`sparse_limit` も同様）は [`MAX_FETCH_K`] 以下のみ受理する
/// （[`RrfConfig::new`] と同じ fail-closed 検証。呼び出し元が構造体リテラル相当の
/// 迂回で無制限な上限を渡せないようにする）。境界同点グループ完全化（Issue #310・
/// Issue #320）の再取得ループが `fetch_k` を `pool_depth` 超まで伸ばすことがある
/// ため、上限は [`MAX_POOL_DEPTH`] ではなく再取得の上限である [`MAX_FETCH_K`] に
/// 揃える。
pub(crate) fn rrf_fuse_with_limits(
    dense: &[CandidateHit],
    dense_limit: usize,
    sparse: &[ScoredDoc],
    sparse_limit: usize,
    cfg: &RrfConfig,
) -> Result<Vec<HybridHit>, HybridError> {
    if dense_limit > MAX_FETCH_K || sparse_limit > MAX_FETCH_K {
        return Err(HybridError::InvalidConfig);
    }
    // 長さ検証を他のどの検証よりも先に行う。以降の検証（有限性・ソート順・重複）は
    // いずれも入力を線形走査し、重複検査（[`has_duplicate_id`]）は走査した分だけ
    // `BTreeSet` へ挿入するため、長さを検証せずに通すと契約違反の provider/index が
    // 上限（高々 `MAX_POOL_DEPTH`）を大きく超える件数を返した場合に
    // 無制限にメモリ・CPU を消費できてしまう（[`HybridError::TooManyCandidates`]
    // のドキュメント参照）。
    if dense.len() > dense_limit {
        return Err(HybridError::TooManyCandidates {
            len: dense.len(),
            max: dense_limit,
        });
    }
    if sparse.len() > sparse_limit {
        return Err(HybridError::TooManyCandidates {
            len: sparse.len(),
            max: sparse_limit,
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

    // 融合マップの要素数は高々 `dense_limit + sparse_limit`（境界同点グループ完全化の
    // 再取得により `pool_depth` を超えうるが、[`MAX_FETCH_K`] を上回ることはない
    // 上記の長さ検証で保証済み）に有界。id をキーにした
    // `BTreeMap` を使うことで、出現順・ハッシュ実装に依存しない決定的な走査順序を
    // 保証する（同点タイブレークの安定性に寄与）。
    let mut scores: BTreeMap<u64, f64> = BTreeMap::new();

    accumulate_ranked(
        dense,
        |h| (f64::from(h.score), h.id),
        cfg.k_const(),
        cfg.dense_weight(),
        cfg.tie_rank(),
        &mut scores,
    );
    accumulate_ranked(
        sparse,
        |d| (d.score, d.doc_id),
        cfg.k_const(),
        cfg.sparse_weight(),
        cfg.tie_rank(),
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
/// （[`CandidateHit`]・[`ScoredDoc`] 双方のドキュメントが定める順位契約）に従っているかを
/// 判定する。`CandidateHit`（`f32`）・`ScoredDoc`（`f64`）双方から `f64` へ変換して同一の
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

/// [`rrf_fuse`] の内部ヘルパ。1 つのランク付き列（呼び出し元で既に長さ検証済み。
/// [`HybridError::TooManyCandidates`] のドキュメント参照）を RRF スコアへ変換し、
/// `scores` へ加算する。密・疎の両リストから同じロジックで呼ばれることで加算順序を
/// 一本化する。呼び出し元（[`rrf_fuse`]）が [`has_duplicate_id`] で入力リスト全体の
/// 重複なしを事前に検証済みのため、本関数自体は重複検知を行わない。
///
/// 1-based の**位置順位**（`items` に現れる並び順がそのまま順位になる。`items` の
/// 並び順は provider/index が定める「スコア降順、同点は候補識別子昇順」
/// （[`is_sorted_desc_id_asc`] が検証する契約）に従う）を、同点グループ内の全要素へも
/// 個別に割り当てる。`key` は `(score, id)` を返すが、本関数はこのうち `id`（順位の
/// 加算先）のみを使い、`score` 自体は同点判定に使わない（呼び出し元 [`rrf_fuse`] が
/// スコアの有限性・ソート順を事前検証済みのため）。
///
/// 同点グループへ割り当てる順位は `tie_rank`（[`TieRank`]。Issue #310 で
/// [`TieRank::Positional`]／[`TieRank::GroupEnd`] の 2 規約を確定。詳細な導出は
/// `docs/design/hybrid-recall-regression.md`「Issue #310: engine 側改善」節を参照）
/// が決める。`items` は呼び出し元（[`rrf_fuse_with_limits`]）がスコアの有限性・
/// ソート順（[`is_sorted_desc_id_asc`]）を検証済みのため、同点判定は
/// `f64::total_cmp` の `Equal` のみで行う（NaN は事前拒否済みで全順序上も安全）。
fn accumulate_ranked<T>(
    items: &[T],
    key: impl Fn(&T) -> (f64, u64),
    k_const: f64,
    weight: f64,
    tie_rank: TieRank,
    scores: &mut BTreeMap<u64, f64>,
) {
    match tie_rank {
        TieRank::Positional => {
            for (idx, item) in items.iter().enumerate() {
                // 1-based 順位。`idx` は `items.len() <= pool_depth <= MAX_POOL_DEPTH`
                // に収まるため `as f64` 変換で精度は失われない。
                let rank = (idx as f64) + 1.0;
                let contribution = weight / (k_const + rank);
                let (_, id) = key(item);
                let entry = scores.entry(id).or_insert(0.0);
                *entry += contribution;
            }
        }
        TieRank::GroupEnd => {
            // グループ末尾順位（modified competition ranking）。同一スコアの連続
            // 区間（`items` は呼び出し元がスコア降順・同点 id 昇順であることを検証
            // 済みのため、同点は必ず連続する）ごとに走査し、区間末尾の 1-based 位置
            // を区間内の全要素へ割り当てる。
            let mut idx = 0usize;
            while idx < items.len() {
                let (group_score, _) = key(&items[idx]);
                let mut end = idx + 1;
                while end < items.len() {
                    let (score, _) = key(&items[end]);
                    if score.total_cmp(&group_score) != std::cmp::Ordering::Equal {
                        break;
                    }
                    end += 1;
                }
                // グループ末尾の 1-based 順位。`end` はグループ内の要素数だけ進んだ
                // 「末尾の次」のインデックスのため、そのまま 1-based 順位に一致する。
                let rank = end as f64;
                let contribution = weight / (k_const + rank);
                for item in &items[idx..end] {
                    let (_, id) = key(item);
                    let entry = scores.entry(id).or_insert(0.0);
                    *entry += contribution;
                }
                idx = end;
            }
        }
    }
}

/// ソフトブースト（TASK-111・PLAN-1）でヒント種別 1 件一致あたりに加点する既定値。
/// 受け入れ基準の数値そのものは非公開（ポインタ: TASK-111・PLAN-1。spec 本文は
/// 転記しない）で、本値は spec 由来の数値ではなく [`RrfConfig::default`]
/// （`k_const=60.0`・`dense_weight=sparse_weight=1.0`・`pool_depth=200`、いずれも
/// 本ファイル内で既に公開済みの定数）から純粋に導出した安全側の値である。
///
/// 保証している性質は次の 1 点のみ（TASK-111。3 回目の codex-review P1 指摘対応で
/// 判定方式自体を見直したが、本定数が保証する不変条件は変わらない）:
/// [`RrfConfig::default`] 下で、[`MAX_BOOST_RULES`] 件（最大 16 件）のルールが
/// 同一候補へ同時に一致しても、その候補が**プール最下位級**（融合プール中で唯一の
/// 出現が片方のチャネルの最下位順位のみ、という最悪ケース。RRF はヒットしない
/// チャネルに `0.0` を割り当てるのではなく、そもそも `hits` に現れない ── 「0.0 と
/// 比較すれば安全」という誤った前提を置かない）の加点後スコアで、真の 1 位が
/// 取りうる保証下限（[`soft_boost_confirm_cap`]。`min(dense_weight, sparse_weight) /
/// (k_const + 1)` = `1.0 / 61.0` ≈ `0.016393`）を上回ることはできない。この
/// 不変条件は [`apply_soft_boost`] が呼び出しのたびに候補ごとの加点合計を
/// [`soft_boost_confirm_cap`] という絶対上限と比較する形で動的に検証する
/// （[`HybridError::BoostSoftBoundExceeded`]）ため、本定数はあくまで
/// [`RrfConfig::default`] 向け・**プール最下位級**の候補を想定した一例に過ぎず、
/// この定数自体が全ケースでの安全性を保証するわけではない。真の 1 位のすぐ
/// 近く（近接順位）にいる候補への加点は、この絶対上限を下回る限り真の 1 位を
/// 追い越すことも含めて正常に受理される（`tests/soft_boost.rs` で検証。PLAN-1 の
/// 意図どおりのソフトブースト本来の用途）。
pub const SOFT_BOOST_PER_MATCH: f64 = 0.0007;

/// [`apply_soft_boost`] が 1 回の呼び出しで受け付けるブーストルール数の上限
/// （無制限入力の拒否。coding-rust.md「長さフィールドは上限検証してから」）。
pub const MAX_BOOST_RULES: usize = 16;

/// [`BoostRule::new`] が受け付ける加点値 `amount` の単体上限。あくまで「非有限・
/// 極端に巨大な値を弾く」ためのラフな安全弁であり、実際にどこまで「ソフト」で
/// あり続けられるかは呼び出し元が使う `cfg`（[`RrfConfig`]）に依存するため、
/// この定数だけでは softness を保証しない（保証は [`apply_soft_boost`] が `cfg` に
/// 対して動的に行う。codex-review P1 指摘対応: 以前は本定数のみに頼っており、
/// [`RrfConfig::default`] を使う呼び出し元でも `BoostRule::new(ids, 1.0)` を渡すだけで
/// [`SOFT_BOOST_PER_MATCH`] の想定を大きく超える加点が可能だった）。
const MAX_BOOST_AMOUNT: f64 = 1.0;

/// [`BoostRule::new`] が受け付ける `ids`（1 ルールが一致対象とする候補識別子
/// 集合）の要素数上限（TASK-111。PR #257 codex-review P2 指摘対応）。
///
/// 以前は `BoostRule::new` が `ids` のサイズを検証していなかったため、
/// [`hybrid_search_boosted`] の早期拒否（`per_id_boost` 構築ループ）が、検索対象
/// `hits` に一切含まれない id まで含め `rules` の全 `ids` 集合を検索実行前に
/// 走査・複製していた。少数（[`MAX_BOOST_RULES`] 以内）のルールでも、各ルールの
/// `ids` が巨大であれば `O(全 rule.ids)` の CPU・メモリを無制限に消費できてしまう
/// （coding-rust.md「長さフィールドは上限検証してからアロケーションに使う」）。
///
/// 上限は [`MAX_POOL_DEPTH`] を流用する: `BoostRule` が対象とする id は
/// [`apply_soft_boost`] が受け取る融合済みプール `hits`（境界同点グループ完全化の
/// 再取得〔Issue #320〕により `cfg.pool_depth()` を超えうるが、高々
/// `dense_limit + sparse_limit` ≤ `2 * `[`MAX_FETCH_K`] 件に有界）に含まれる id
/// にしか実際には作用しないため、それを大きく超える要素数を許しても呼び出し元の
/// 実用上の意味はなく、無制限確保を許すだけの安全弁として不要に緩い
/// （`MAX_BOOST_IDS` 自体は素朴な安全弁であり、実プールの厳密な上限に追従させる
/// 必要はない）。
pub const MAX_BOOST_IDS: usize = MAX_POOL_DEPTH;

/// `cfg`（[`RrfConfig`]）だけから、実際の融合プールの分布を見ずに算出できる
/// ソフトブースト加点合計の**緩い**（＝安全側に大きすぎることはあっても小さすぎ
/// ない）上限。[`hybrid_search_boosted`] が `rrf_fuse` 実行より前に行う早期拒否
/// （codex-review P2 指摘対応）専用で、[`apply_soft_boost`] 側の確定判定
/// （[`soft_boost_confirm_cap`]）を代替しない（2 回目の codex-review P1 指摘対応:
/// 以前の版は「重みが大きい方のチャネルに必ず 1 位候補が存在する」と誤って
/// 仮定しており、そのチャネルがクエリ不一致で空になるケース〔例:
/// `dense_weight=1`・`sparse_weight=100` で疎ヒットが空〕を見落としていた）。
///
/// 導出根拠: どちらのチャネルが実際に候補を返すかを `cfg` だけからは知り得ないため、
/// 「両チャネルとも同一候補が 1 位を取った」という**理論上の最良ケース**
/// （`(dense_weight + sparse_weight) / (k_const + 1)`）を融合スコアの絶対上限とし、
/// 最低スコアは `0.0` に近づきうる（`pool_depth` が大きいほど下限がいくらでも 0 に
/// 近づく）ことから、スコア差の絶対上限は分子のみで抑えられる。これは常に
/// 実際のスコア差以上になる安全側の上界であり、ここを下回った加点合計だけが
/// 「確定的に安全」というわけではない（確定判定は [`apply_soft_boost`] が候補
/// ごとに行う [`soft_boost_confirm_cap`] 比較が担う。呼び出し元がまだ実際の
/// `hits` を持たない早期拒否専用のため、意図的に `sum` 集約を使う。`min` 集約の
/// [`soft_boost_confirm_cap`] より緩い＝早期拒否を漏らす側に倒すのは、早期拒否は
/// 「確実に危険」の判定であり、確定判定を代替しないため安全）。
fn soft_boost_loose_upper_bound(cfg: &RrfConfig) -> f64 {
    (cfg.dense_weight() + cfg.sparse_weight()) / (cfg.k_const() + 1.0)
}

/// [`apply_soft_boost`] の確定判定が使う、候補 1 件あたりの加点合計の**絶対**上限
/// （TASK-111。3 回目の codex-review P1 指摘・cursor bot「Boost error contract is
/// inconsistent」指摘対応）。[`HybridError::BoostSoftBoundExceeded`] のドキュメント
/// 参照: 過去の実装（2 回目の codex-review P1 指摘対応）は候補ごとに「実スコアから
/// 融合プールの実際の最高スコア（真の 1 位）までの差」と比較しており、真の 1 位を
/// 追い越す加点を一律拒否してしまっていた。これは近接順位を入れ替えるという
/// ソフトブースト本来の用途（PLAN-1 の意図）そのものを検索エラーにする過剰拒否
/// だったため、実データの順位関係（margin）とは無関係な絶対値へ置き換える。
///
/// 上限は `min(dense_weight, sparse_weight) / (k_const + 1)`: 融合プールが空でない
/// 限り、真の 1 位は少なくともどちらか一方のチャネルで rank 1 として出現している
/// はずであり、その最悪ケース（弱い方のチャネルにのみ rank 1 で出現）でも
/// 保証される下限がこの値である。[`soft_boost_loose_upper_bound`]（`sum`。
/// `hybrid_search_boosted` の早期拒否専用）より厳しい `min` を使うのは、重みが
/// 大きい方のチャネルがクエリ不一致で空になるケース（`tests::
/// hybrid_search_boosted_rejects_boost_when_heavier_channel_is_empty`）でも安全側
/// （弱い方のチャネルの寄与だけを仮定する）に倒すため。ここで `max` を使うと、
/// 過去 2 回目の codex-review P1 指摘の核心だった「重みが大きい方のチャネルに
/// 必ず 1 位候補が存在する」という誤仮定を確定判定側で再現してしまい、本来
/// 拒否すべき加点を通してしまう危険がある。
///
/// [`apply_soft_boost`] はこの値を単独では使わず、候補ごとに `この値 -
/// hit.score`（`hit.score` が本値以上なら本値そのもの）を許容加点量として使う
/// （PR #257 codex-review 指摘対応: 本値単独を加点合計の上限にすると、候補の
/// 加点前スコア（`hit.score`。正の値）を無視するため、プール最下位級
/// （`hit.score` が本値未満）の候補でも `hit.score + candidate_boost` が本値
/// 〔＝真の 1 位の保証下限〕を上回りうる「安全性コメントと実装の乖離」が
/// 生じていた。また `hit.score` が本値以上の近接順位級候補を許容加点量の
/// 上限判定から一律除外すると、加点量そのものが本値を大幅に超える大幅加点を
/// 許してしまい「小さな加点のみ」という契約に反するため、その場合も本値自体を
/// 上限として適用する）。
fn soft_boost_confirm_cap(cfg: &RrfConfig) -> f64 {
    cfg.dense_weight().min(cfg.sparse_weight()) / (cfg.k_const() + 1.0)
}

// `RrfConfig::default` に対する早期拒否側の緩い上限をコンパイル時にも固定する
// （`MAX_BOOST_RULES` や `SOFT_BOOST_PER_MATCH` の将来変更で、実行時検証
// （[`apply_soft_boost`]・[`soft_boost_confirm_cap`]）を経ない限り無言のうちに
// 破られないようにする回帰ガード。実行時の確定判定は実際の `hits` に対して
// 行われるため、本 assertion は「デフォルト設定の早期拒否チェックが常識的な範囲に
// 収まっている」ことの早期発見用に過ぎない）。
const _: () = assert!(
    (MAX_BOOST_RULES as f64) * SOFT_BOOST_PER_MATCH < 2.0 / 61.0,
    "SOFT_BOOST_PER_MATCH * MAX_BOOST_RULES must stay below the RrfConfig::default \
     soft_boost_loose_upper_bound ((dense_weight + sparse_weight) / (k_const + 1) = 2/61)"
);

// 確定判定側（[`soft_boost_confirm_cap`]。`min` 集約）に対する不変条件も同様に
// コンパイル時固定する。こちらは実際に [`apply_soft_boost`] が候補ごとの加点合計を
// 拒否するかどうかを左右する、より厳しい（`min <= sum`）実効上の上限である。
const _: () = assert!(
    (MAX_BOOST_RULES as f64) * SOFT_BOOST_PER_MATCH < 1.0 / 61.0,
    "SOFT_BOOST_PER_MATCH * MAX_BOOST_RULES must stay below the RrfConfig::default \
     soft_boost_confirm_cap (min(dense_weight, sparse_weight) / (k_const + 1) = 1/61)"
);

/// ソフトブーストの 1 ルール（TASK-111・PLAN-1。EXT-4 の汎用メタデータ一致ブーストの
/// 共通基盤として、ヒント種別（path/kind）に依存しない形にしてある）。
///
/// `ids` は「このルールに一致した候補識別子の集合」で、パス・種別等のメタデータへの
/// アクセスは呼び出し元の責務とする（本モジュールは membership 判定のみ行う。
/// [`hybrid_search`] と同じ「呼び出し元定義の識別子」契約）。フィールドは非公開とし、
/// [`BoostRule::new`] の検証付きコンストラクタのみで構築できる（`RrfConfig` と同じ
/// 「構造体リテラルでの直接構築を許すと検証を迂回できる」流儀）。
#[derive(Debug, Clone, Copy)]
pub struct BoostRule<'a> {
    ids: &'a BTreeSet<u64>,
    amount: f64,
}

impl<'a> BoostRule<'a> {
    /// `amount`・`ids` の検証付きコンストラクタ。`amount` が有限かつ
    /// `0.0 < amount <= MAX_BOOST_AMOUNT` の範囲外の場合は
    /// [`HybridError::InvalidBoost`] を返す（fail-closed）。`ids.len()` が
    /// [`MAX_BOOST_IDS`] を超える場合は [`HybridError::TooManyBoostIds`] を返す
    /// （TASK-111。PR #257 codex-review P2 指摘対応: 呼び出し側〔[`hybrid_search_boosted`]
    /// の早期拒否〕がこの集合を検索実行前に全件走査・複製するため、構築時点で
    /// サイズを上限検証しアロケーション・走査コストの無制限な発生を防ぐ）。
    /// `BTreeSet::len()` は `O(1)` のため、この検証自体は集合を走査しない。
    pub fn new(ids: &'a BTreeSet<u64>, amount: f64) -> Result<Self, HybridError> {
        if ids.len() > MAX_BOOST_IDS {
            return Err(HybridError::TooManyBoostIds {
                len: ids.len(),
                max: MAX_BOOST_IDS,
            });
        }
        if !amount.is_finite() || amount <= 0.0 || amount > MAX_BOOST_AMOUNT {
            return Err(HybridError::InvalidBoost);
        }
        Ok(Self { ids, amount })
    }
}

/// ヒント一致判定ヘルパ（呼び出し元が [`BoostRule`] 構築に使う共通述語。TASK-111）。
/// パスヒントは部分文字列一致で判定する。空ヒントは常に不一致（ブーストなし側へ
/// 倒す fail-closed な既定）。正規表現・glob は使わない（LLM 由来の untrusted
/// 文字列に対する ReDoS 類の余地を作らない。ヒント文字列長は `query_planner.rs`
/// （TASK-110・[`crate::query_planner::MAX_HINT_LEN`]）側で上限検証済み）。
pub fn path_hint_matches(hint: &str, path: &str) -> bool {
    !hint.is_empty() && path.contains(hint)
}

/// ヒント一致判定ヘルパ（TASK-111）。種別ヒントは完全一致で判定する。空ヒントは
/// 常に不一致（[`path_hint_matches`] と同じ fail-closed な既定）。
pub fn kind_hint_matches(hint: &str, kind: &str) -> bool {
    !hint.is_empty() && hint == kind
}

/// [`rrf_fuse`] が返した融合済みプール `hits` へソフトブーストを適用する
/// （TASK-111・PLAN-1）。各 hit につき、所属する全ルールの `amount` を加算する。
///
/// 候補の追加・削除は構造的に不可能（既存 `Vec` の `score` 更新のみ）で、EXT-4 の
/// 「完全除外しない」性質を型レベルで担保する。`rules.len() > MAX_BOOST_RULES` は
/// アロケーション・走査前に [`HybridError::TooManyBoostRules`] で拒否する（[`rrf_fuse`]
/// の長さ検証と同じ順序）。入力 `hits` に非有限スコアが 1 件でもあれば加点前に
/// [`HybridError::NonFiniteScore`] で拒否する（`cfg` に対する検証を非有限値で汚染
/// させないための順序。fail-closed）。各候補について、加点前スコア（`hit.score`）が
/// [`soft_boost_confirm_cap`]（`cfg` から導出する絶対上限）未満の場合、実際に
/// 一致した `rules` の加点合計が「その上限から `hit.score` を差し引いた残余」
/// 以上なら [`HybridError::BoostSoftBoundExceeded`] で拒否する（TASK-111。3 回目の
/// codex-review P1 指摘・cursor bot「Boost error contract is inconsistent」指摘
/// 対応、および PR #257 codex-review 指摘対応: 以前は加点合計を
/// [`soft_boost_confirm_cap`] 単独とだけ比較しており、候補の元スコアを無視して
/// いたため、プール最下位級（元スコアが上限未満）の候補でも「元スコア＋加点」が
/// 上限〔＝真の 1 位の保証下限〕を上回りえた）。`hit.score` が上限以上の候補
/// （元スコア単独で既に保証下限相当の近接順位級）も、加点合計そのものが上限
/// （`cap`）以上なら同じエラーで拒否する（4 回目の codex-review P1 指摘対応:
/// 近接順位の逆転自体は許すべきだが、複数ルールを同一候補へ積んで
/// `soft_boost_confirm_cap` を大幅に超える加点量を与えることまでは許容しない。
/// 「小さな加点のみ」という PLAN-1 の契約は、真の 1 位との相対差ではなく加点量
/// 自体の絶対上限として全候補に一貫して課す）。過去の実装（2 回目の
/// codex-review P1 指摘対応）は候補ごとに「実スコアから
/// `hits` の実際の最高スコア（真の 1 位）までの差」と比較しており、真の 1 位を
/// 追い越す加点を一律拒否していた。これは近接順位を入れ替えるというソフトブースト
/// 本来の用途（PLAN-1 の意図）そのものを検索エラーにする過剰拒否だった: 既定 RRF
/// の単一チャネルでの実測順位差は僅か（例: 約 0.000264）であり、公開済み既定値
/// [`SOFT_BOOST_PER_MATCH`]（`0.0007`）を 1 件適用しただけの通常利用でもこの僅差を
/// 上回り失敗していた。真の 1 位を追い越して新たな 1 位になること自体は正常な
/// 結果であり拒否すべきではないため、実データの `hits` の最高スコアを都度計算する
/// 方式には戻さず、`cfg` から導出する絶対上限（[`soft_boost_confirm_cap`]）から
/// 候補自身の元スコアを差し引いた値（元スコアが上限以上の候補は上限そのもの）と
/// だけ比較する。近接順位の正当な逆転は従来どおり妨げないが、加点量自体は
/// 上限を超えないことを全候補に一貫して要求する（詳細は
/// [`HybridError::BoostSoftBoundExceeded`]・[`soft_boost_confirm_cap`] のドキュメント
/// 参照）。この判定は長さ検証・有限性検証の後、スコア加算より先に
/// 行う（[`TooManyCandidates`] (HybridError::TooManyCandidates) と同じ順序）。
/// 加算後の非有限化（Inf オーバーフロー）も同様に [`HybridError::NonFiniteScore`]
/// で拒否する（`rrf_fuse` の融合後検証と同じ方向）。最後に既存と同一の比較器
/// （スコア降順・同点 id 昇順、`f64::total_cmp` ベース）で再ソートし、決定性を
/// 維持する（`sort_by` を使い `sort_unstable_*` は使わない。
/// `scripts/check_sort_determinism.sh` の対象）。
///
/// `hits` が空の場合は候補が存在せず、加点合計の絶対上限チェックは対象なく
/// 自然にスキップされる（`rules` の長さ・`BoostRule::new` の値域検証は通常どおり
/// 適用される）。
pub fn apply_soft_boost(
    hits: &mut [HybridHit],
    rules: &[BoostRule<'_>],
    cfg: &RrfConfig,
) -> Result<(), HybridError> {
    if rules.len() > MAX_BOOST_RULES {
        return Err(HybridError::TooManyBoostRules {
            len: rules.len(),
            max: MAX_BOOST_RULES,
        });
    }

    // 加点前に入力自体の有限性を検証する（fail-closed。`rrf_fuse` は融合結果の
    // 有限性を保証するが、`apply_soft_boost` は `rrf_fuse` の出力に限らず任意の
    // 呼び出し元が渡しうる `hits` を前提としない契約のため、ここでも独立に
    // 検証する）。
    if hits.iter().any(|h| !h.score.is_finite()) {
        return Err(HybridError::NonFiniteScore);
    }

    // `rules` が空なら加点合計は必ず `0.0` で、`apply_soft_boost` は no-op のまま
    // 安全（`BoostRule::new` は `amount > 0.0` を要求するため、非空の `rules` なら
    // `total_boost` は必ず正になる）。`total_boost > 0.0` を先に確認することで、
    // 空ルール呼び出しでは以下の絶対上限チェックのループ自体を省略する。
    let total_boost: f64 = rules.iter().map(|rule| rule.amount).sum();
    if total_boost > 0.0 {
        // 確定判定は候補ごとの加点合計を [`soft_boost_confirm_cap`]（`cfg` から
        // 導出する絶対上限）と比較するが、`cap` 単独ではなく候補自身の加点前
        // スコア（`hit.score`）を差し引いた**候補ごとの残余**（`cap - hit.score`）
        // と比較する（TASK-111。PR #257 codex-review P1 指摘対応: 以前は
        // `candidate_boost >= cap` のみで判定しており、候補の元スコアを無視して
        // いたため、`hit.score` が正で `cap` 未満の「プール最下位級」候補でも
        // `hit.score + candidate_boost` が `cap`（＝真の 1 位の保証下限）を上回り
        // 得た。`hit.score` が `cap` 以上の候補（元スコア単独で既に保証下限相当
        // 以上の近接順位級）は残余を負にせず `cap` 自体を上限として素通しする
        // （4 回目の codex-review P1 指摘対応: 以前はここを確定判定から無条件に
        // `continue` していたため、複数ルールを同一 id へ積んで `cap` を大幅に
        // 超える加点量を近接順位候補へ与えられ、「小さな加点のみ」という契約に
        // 反していた）。これにより、真の 1 位の実際のスコアが保証下限を上回る
        // 通常のケースで、近接順位（元スコアが `cap` 以上）の候補が真の 1 位を
        // 追い越す正当な**小さい**加点は妨げず（2 回目の codex-review P1 指摘の
        // 再発防止）、かつ加点量自体が `cap` 以上になる非正当な大幅加点は拒否する。
        // 詳細は [`soft_boost_confirm_cap`]・[`HybridError::BoostSoftBoundExceeded`]
        // のドキュメント参照。`apply_soft_boost_rejects_total_exceeding_soft_bound`
        // （`hit.score == 0.0` のケース。`cap - 0.0 == cap` で従来どおりの挙動に
        // 一致）・`apply_soft_boost_allows_near_top_candidate_to_overtake_true_top`
        // （`hit.score` が `cap` を大きく超える近接順位候補への**小さい**加点は
        // 通過するケース）・
        // `apply_soft_boost_rejects_boost_that_would_cross_guaranteed_floor_with_candidate_score`
        // （`hit.score` が `cap` 未満かつ正のプール最下位級候補で、`candidate_boost`
        // 単独では `cap` 未満でも `hit.score` を加えると `cap` を超えるケース）・
        // `apply_soft_boost_rejects_boost_exceeding_cap_for_near_top_candidate`
        // （本指摘の直接回帰: `hit.score` が `cap` 以上の近接順位候補へ、加点量
        // 自体が `cap` を超える大幅な加点を与えるケース）で固定する。
        let cap = soft_boost_confirm_cap(cfg);
        for hit in hits.iter() {
            let candidate_boost: f64 = rules
                .iter()
                .filter(|rule| rule.ids.contains(&hit.id))
                .map(|rule| rule.amount)
                .sum();
            if candidate_boost <= 0.0 {
                continue;
            }
            // `allowed` は「この候補が受け取れる加点量そのものの上限」。元スコアが
            // `cap` 未満の候補は残余（`cap - hit.score`）を上限とし（近接順位でない
            // 候補が加点だけで保証下限を飛び越えるのを防ぐ、従来どおりの判定）、
            // 元スコアが `cap` 以上の候補（近接順位級。追い越し自体は PLAN-1 の
            // 正当な用途のため拒否しない）でも `cap` 自体を上限にする（4 回目の
            // codex-review P1 指摘対応: 以前は `hit.score >= cap` の候補を確定判定
            // から無条件に除外していたため、`BoostRule::new` の範囲内で複数ルールを
            // 同一 id に積むと `soft_boost_confirm_cap` を大幅に超える加点量
            // （早期検査 `soft_boost_loose_upper_bound` は通過する程度の合計）を
            // 適用でき、「小さな加点のみ」という契約〔PLAN-1〕に反していた）。
            let allowed = if hit.score >= cap {
                cap
            } else {
                cap - hit.score
            };
            if candidate_boost >= allowed {
                return Err(HybridError::BoostSoftBoundExceeded {
                    total: candidate_boost,
                    max: allowed,
                });
            }
        }
    }

    for hit in hits.iter_mut() {
        for rule in rules {
            if rule.ids.contains(&hit.id) {
                hit.score += rule.amount;
            }
        }
    }

    if hits.iter().any(|h| !h.score.is_finite()) {
        return Err(HybridError::NonFiniteScore);
    }

    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    Ok(())
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
    hybrid_search_boosted(provider, input, sparse_index, query_text, k, cfg, &[])
}

/// [`hybrid_search`] のソフトブースト対応版（TASK-111・PLAN-1）。密・疎の検証・融合は
/// [`hybrid_search`] と全く同じ手順で行い、[`rrf_fuse`] の融合結果（`truncate(k)` の
/// **前**、切り詰め前のプール全体）に対してのみ [`apply_soft_boost`] を適用してから
/// 先頭 `k` 件へ切り詰める。切り詰め前に適用するのは、`k` 圏外だった一致候補が
/// ブーストで Top-k 内へ浮上できるようにするため（切り詰め後だと圏外候補が浮上
/// できず効果が失われる）。`rules` が空の場合は [`hybrid_search`] と完全に同じ結果を
/// 返す（[`apply_soft_boost`] は no-op）。
pub fn hybrid_search_boosted(
    provider: &dyn SearchProvider,
    input: SearchInput<'_>,
    sparse_index: &SparseIndex,
    query_text: &str,
    k: usize,
    cfg: &RrfConfig,
    rules: &[BoostRule<'_>],
) -> Result<Vec<HybridHit>, HybridError> {
    if k == 0 || k > cfg.pool_depth() {
        return Err(HybridError::InvalidK);
    }
    // ルール件数の検証は [`apply_soft_boost`] 内でも行われるが、ここではそれより先
    // （`provider.search`・可視性走査・`SparseIndex::search_within`・`rrf_fuse` より
    // 前）に同じ検証を行う（codex-review P2 指摘対応）。`k` の検証と同じ「安価な
    // 入力検証を高コストな処理より先に行う」順序を踏襲し、契約違反の呼び出し元が
    // 無駄な検索コストを消費させられないようにする。
    if rules.len() > MAX_BOOST_RULES {
        return Err(HybridError::TooManyBoostRules {
            len: rules.len(),
            max: MAX_BOOST_RULES,
        });
    }
    // 加点上限の**確定**判定は融合プールが揃った後の [`apply_soft_boost`]
    // （[`soft_boost_confirm_cap`]。`cfg` から導出する絶対上限）が担う。ここでは
    // まだ `hits` が存在しないため確定判定はできないが、`cfg` だけから導出できる
    // **緩い**上限（[`soft_boost_loose_upper_bound`]。理論上のスコア絶対上限を
    // 分子に、下限を `0.0` とみなした安全側の上界）を使い、それすら下回れない
    // 加点合計は `provider.search`・`SparseIndex::search_within`・`rrf_fuse` を
    // 経ずに早期拒否する（codex-review P2 指摘対応）。この早期拒否は「確実に安全」
    // の判定ではなく「確実に危険」の判定であり、ここを通過しても [`apply_soft_boost`]
    // 側の確定判定で拒否されうる（2 回目の codex-review P1 指摘対応: 以前はここが
    // `cfg` の重みだけから理論値を導出し、それを唯一の判定基準にしていたため、
    // 重みが大きい方のチャネルがクエリ不一致で空になるケースで上限を過大評価
    // していた）。
    //
    // 「確実に危険」の見積もりは候補ごとに行う必要がある: `rules` の対象 id が
    // 互いに素な場合、実際にはどの候補も全ルールの加点を同時に受け取ることは
    // ないため、単純に全ルールの `amount` を合算すると実際には起こり得ない過大な
    // 合計で誤って早期拒否してしまう（3 回目の codex-review P1 指摘・cursor bot
    // 「Boost error contract is inconsistent」指摘対応）。`rules` はまだ `hits` に
    // 触れていないため、各ルールの対象 `ids`（[`BoostRule`]）自体から「同一 id に
    // 適用され得る加点の合計」の最大値を求める。
    let mut per_id_boost: BTreeMap<u64, f64> = BTreeMap::new();
    for rule in rules {
        for &id in rule.ids {
            *per_id_boost.entry(id).or_insert(0.0) += rule.amount;
        }
    }
    let max_candidate_boost = per_id_boost.values().copied().fold(0.0_f64, f64::max);
    let loose_upper_bound = soft_boost_loose_upper_bound(cfg);
    if max_candidate_boost > 0.0 && max_candidate_boost >= loose_upper_bound {
        return Err(HybridError::BoostSoftBoundExceeded {
            total: max_candidate_boost,
            max: loose_upper_bound,
        });
    }

    let visible_ids: std::collections::BTreeSet<u64> = input.ids.iter().copied().collect();

    // 密プール境界の同点グループ完全化（Issue #310・Issue #320）: `cfg.pool_depth()`
    // ちょうどで要求すると、境界（`pool_depth` 番目と `pool_depth + 1` 番目）が
    // 同点グループの途中を切っている場合にグループの一部だけを取り込んでしまい、
    // [`accumulate_ranked`] の [`TieRank::GroupEnd`] 規約がグループ全体を見られない
    // （グループ末尾順位を過小評価する）。初期 `fetch_k`（`pool_depth * 2`）で取得し、
    // [`complete_boundary_tie_group`] が境界の同点グループの終端を確定できなければ
    // （[`TieBoundary::Undetermined`]）、`fetch_k` を倍増して provider を再度呼び、
    // 上限 [`MAX_FETCH_K`]（可視集合の大きさでも有界化）まで終端確定を試みる。
    // 上限に達してもなお終端確定できない場合は、観測できた同点グループの全メンバーを
    // そのまま保持する（位置ベースで `pool_depth` 件へ部分採用すると、拡張取得列は
    // 常に同点 id 昇順のため最小 id だけが根拠なく生き残る id 依存バイアスが残る。
    // Issue #320 codex-review P1 指摘対応）。
    let dense_cap = MAX_FETCH_K.min(input.ids.len());
    let mut dense_fetch_k = cfg
        .pool_depth()
        .checked_mul(2)
        .unwrap_or(dense_cap)
        .min(dense_cap);
    let (dense_hits, dense_limit) = loop {
        let dense_input = SearchInput {
            ids: input.ids,
            vectors: input.vectors,
            dim: input.dim,
            query: input.query,
            k: dense_fetch_k,
        };
        // `provider` は trait object（[`SearchProvider`]）であり、「`input.ids` 外の id を
        // 返さない」「要求した `k`（＝ `dense_fetch_k`）以下の件数しか返さない」ことは
        // いずれも型では強制されない（呼び出し元の実装ミス・バグの余地がある）。
        let hits: Vec<CandidateHit> = provider.search(dense_input)?;
        // 長さ検証を可視性走査より先に行う（3 回目の codex-review P1 指摘対応）。
        // `rrf_fuse_with_limits` 自身も同じ長さ検証を行うが、ここで早期に拒否する
        // ことで、契約違反 provider が `dense_fetch_k` を大きく超える件数を返した
        // 場合に直後の可視性走査（`.iter().any(...)`）が不要な O(n) コストを
        // 払わずに済む（[`HybridError::TooManyCandidates`] のドキュメント参照）。
        if hits.len() > dense_fetch_k {
            return Err(HybridError::TooManyCandidates {
                len: hits.len(),
                max: dense_fetch_k,
            });
        }
        // 事後フィルタ（不可視 id だけを黙って除外する）はしない: 不可視 id が
        // `dense_fetch_k` の候補枠を占有していた場合、フィルタ後に可視ヒットを
        // 復元できず、結果件数の差から不可視データの有無が外部へ漏れる（2 回目の
        // codex-review P0 指摘対応。モジュールドキュメント参照）。可視性検証は
        // 取得した全件（拡張後の `dense_fetch_k` 件）に対して行い、境界完全化が
        // この検証を弱めない（[`complete_boundary_tie_group`] は可視性検証を
        // 通過した後の列にのみ作用する）。1 件でも可視集合外の id が含まれていたら
        // 検索全体を拒否する（fail-closed）。
        // 識別子の契約（TABLE-12 関連）: 融合キーは `input.ids`（および疎側
        // `DocId`）が何を表すかに従う「呼び出し元定義の識別子」であり、本モジュール
        // は行 `id` であることを前提にしない。行 `id` の一意性スコープはテナント
        // 内に閉じている（同一 `id` の可視行が自テナントと他テナントの `Public` 行
        // で並存しうる）ため、唯一の production 呼び出し元である `sql/exec.rs` は
        // 行 `id` ではなくアリーナのスロット番号を渡し、同一 `id` の別テナント行が
        // 1 エントリへ畳み込まれないようにしている（`tests/row_id_tenant_scope.rs`
        // が固定）。
        if hits.iter().any(|hit| !visible_ids.contains(&hit.id)) {
            return Err(HybridError::ProviderResultRejected);
        }
        // 拡張取得列（`dense_fetch_k` 件。`pool_depth` を超えうる）全体に対し、
        // 有限性・ソート順・重複 id を [`complete_boundary_tie_group`] の前に検証
        // する（codex-review P1 指摘対応。`complete_boundary_tie_group` は境界より
        // 内側を保持しつつ末尾側だけを切り詰める場合があり、切り詰められて消える
        // 末尾部分は後段の [`rrf_fuse_with_limits`] の検証対象に含まれなくなる。
        // fail-closed 方針（coding-rust.md）に従い、切り詰め前の拡張列全体を検証
        // してから初めて [`complete_boundary_tie_group`] へ渡す）。
        if hits.iter().any(|hit| !hit.score.is_finite()) {
            return Err(HybridError::NonFiniteScore);
        }
        if !is_sorted_desc_id_asc(hits.iter().map(|h| (f64::from(h.score), h.id))) {
            return Err(HybridError::UnsortedInput);
        }
        if has_duplicate_id(hits.iter().map(|h| h.id)) {
            return Err(HybridError::DuplicateId);
        }
        // `hits` が可視集合内で存在しうる密ヒットを全件含む（＝取得済み範囲の末尾が
        // そのまま真の終端であると確定できる）かどうか。`dense_fetch_k` が可視 id
        // 総数以上なら provider はそれ以上返しようがなく、`hits.len() <
        // dense_fetch_k` なら provider 自身が「これ以上ない」ことを示している。
        let exhaustive = dense_fetch_k >= input.ids.len() || hits.len() < dense_fetch_k;
        match complete_boundary_tie_group(hits, cfg.pool_depth(), exhaustive) {
            TieBoundary::Resolved(resolved) => break (resolved, dense_fetch_k),
            TieBoundary::Undetermined(observed) => {
                if dense_fetch_k >= dense_cap {
                    // 再取得の余地がない（[`MAX_FETCH_K`]・可視集合の大きさに
                    // 達した）: 観測できた同点グループの全メンバーをそのまま最終
                    // 結果として受理する（位置ベースの部分採用はしない）。
                    break (observed, dense_fetch_k);
                }
                dense_fetch_k = dense_fetch_k.saturating_mul(2).min(dense_cap);
            }
        }
    };
    // 疎側は `sparse_index.search()`（インデックス全体を母数に統計・Top-k を計算する
    // API）ではなく `search_within()`（[`SparseIndex::search_within`]）を使う。
    // `search()` の後段フィルタ（旧実装）は「不可視文書が Top-k のプールを占有して
    // 可視文書を押し出す」「`doc_count`/`doc_freq` を通じて不可視文書の内容・存在が
    // 可視文書の順位へ影響する」という 2 つの経路でテナント境界を弱めてしまう
    // （後段フィルタでは統計計算・候補選出そのものへの影響を防げない。Issue #36
    // codex-review P0 指摘対応）。`search_within` は統計・Top-k 選出の両方を
    // `visible_ids` へ縮約した上で計算するため、この 2 経路をともに断つ。
    //
    // 境界の同点グループ完全化（Issue #310 codex-review P1 指摘対応。当初「BM25 は
    // 連続値なので同点が実質発生しない」としていたが誤り: 同一語頻度・同一文書長の
    // 文書は同一スコアになりうるため、`search_within` が内部で `pool_depth` 件へ
    // 切り詰めた後に `TieRank::GroupEnd` を適用するだけでは境界の同点グループが
    // `SparseIndex` 内部の doc_id 昇順タイブレークで分断され、密側と同型の id 依存
    // バイアスが残る）。密側と同じ再取得ループ（Issue #320）で `search_within` を
    // 呼び、[`complete_boundary_tie_group_by`] で境界の同点グループの終端確定を
    // 試みる。
    let sparse_cap = MAX_FETCH_K.min(visible_ids.len());
    let mut sparse_fetch_k = cfg
        .pool_depth()
        .checked_mul(2)
        .unwrap_or(sparse_cap)
        .min(sparse_cap);
    let (sparse_hits, sparse_limit) = loop {
        let hits: Vec<ScoredDoc> =
            sparse_index.search_within(query_text, sparse_fetch_k, &visible_ids)?;
        // 密側と同じ理由（[`HybridError::TooManyCandidates`] のドキュメント参照）で、
        // 拡張取得列が `sparse_fetch_k` を超えていないかを検証する。`search_within`
        // は自作の内部関数であり `provider` のような trait object 契約違反の余地は
        // 薄いが、fail-closed の防御を密側と揃える。
        if hits.len() > sparse_fetch_k {
            return Err(HybridError::TooManyCandidates {
                len: hits.len(),
                max: sparse_fetch_k,
            });
        }
        // `hits` が可視集合内で存在しうる疎ヒットを全件含む（＝取得済み範囲の末尾が
        // そのまま真の終端であると確定できる）かどうか。密側の `exhaustive` 算出と
        // 同じ判定基準。
        let exhaustive = sparse_fetch_k >= visible_ids.len() || hits.len() < sparse_fetch_k;
        match complete_boundary_tie_group_by(hits, cfg.pool_depth(), exhaustive, |d| d.score) {
            TieBoundary::Resolved(resolved) => break (resolved, sparse_fetch_k),
            TieBoundary::Undetermined(observed) => {
                if sparse_fetch_k >= sparse_cap {
                    break (observed, sparse_fetch_k);
                }
                sparse_fetch_k = sparse_fetch_k.saturating_mul(2).min(sparse_cap);
            }
        }
    };

    let mut fused =
        rrf_fuse_with_limits(&dense_hits, dense_limit, &sparse_hits, sparse_limit, cfg)?;
    // `truncate(k)` の前にブーストを適用する（本関数ドキュメント参照。切り詰め後だと
    // 圏外候補が浮上できず EXT-4/PLAN-1 の効果が失われる）。
    apply_soft_boost(&mut fused, rules, cfg)?;
    fused.truncate(k);
    Ok(fused)
}

/// [`complete_boundary_tie_group_by`] の判定結果（Issue #320 codex-review P1 指摘
/// 対応）。境界の同点グループが取得済み範囲内で終端確定できたか（`Resolved`）、
/// できず再取得が必要か（`Undetermined`）を型で区別し、位置ベースの部分採用
/// （ID 昇順で最小 ID だけが残る id 依存バイアス）を呼び出し元が誤って選べない
/// ようにする。いずれのバリアントも内部の `Vec<T>` は元の並び順（スコア降順・
/// 同点 id 昇順）を保持したまま返す。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TieBoundary<T> {
    /// 境界の同点グループを終端まで含めて（または境界がグループを割っていない
    /// ため単純に）切り詰め済み。呼び出し元はこの列をそのまま融合対象として使える。
    Resolved(Vec<T>),
    /// 取得済み範囲の末尾まで境界の同点グループが続いており、かつ可視集合に
    /// まだ未取得の候補が残りうる（非 exhaustive）ため終端を確定できない。
    /// 呼び出し元は `fetch_k` を伸ばして再取得するか（[`MAX_FETCH_K`] 未満なら）、
    /// 再取得の余地がなければこの列（観測できた同点グループの全メンバー）を
    /// そのまま最終結果として受理する（部分採用はしない）。
    Undetermined(Vec<T>),
}

/// 密検索プールを [`RrfConfig::pool_depth`] の境界で「同点グループの途中で切らない」
/// よう完全化する（Issue #310。[`hybrid_search_boosted`] からのみ呼ばれる純粋関数。
/// 疎側は [`complete_boundary_tie_group_by`] を直接呼ぶ）。
fn complete_boundary_tie_group(
    dense: Vec<CandidateHit>,
    pool_depth: usize,
    exhaustive: bool,
) -> TieBoundary<CandidateHit> {
    complete_boundary_tie_group_by(dense, pool_depth, exhaustive, |hit| f64::from(hit.score))
}

/// [`complete_boundary_tie_group`] の汎用実装（`pub(crate)`）。密側（[`CandidateHit`]・
/// `score: f32`）・疎側（[`crate::sparse::ScoredDoc`]・`score: f64`）いずれの型からも
/// `score_of` で `f64` スコアを取り出せるようにし、境界完全化ロジック自体は 1 箇所に
/// 集約する（Issue #310 codex-review P1 指摘対応: 疎側も密側と同じ境界完全化が必要な
/// ため、型ごとの実装の重複・乖離を避ける）。
///
/// `items` は呼び出し元契約（スコア降順・同点 id 昇順）に従っている前提だが、その
/// 前提自体は呼び出し元の後段 [`rrf_fuse_with_limits`] が独立に検証する。
///
/// - `items.len() < pool_depth` なら呼び出し元の `fetch_k` 未満しか返っていない
///   （＝呼び出し元の `*_exhaustive` 算出により必ず exhaustive）ためそのまま
///   `Resolved` で返す（境界そのものが存在しない）。
/// - `items.len() == pool_depth` は、境界の末尾要素が未取得候補と同点かどうかを
///   比較対象なしに判定できない。`exhaustive` ならそのまま真の終端として
///   `Resolved` で返す。非 `exhaustive` なら終端を確定できないため `Undetermined`
///   で返す（呼び出し元が `fetch_k` を伸ばして再取得するか、再取得の余地がなければ
///   この列を最終結果として受理する。Issue #320 codex-review P1 指摘対応: 丸ごと
///   除外すると全件同点コーパスで結果が空になる回帰があったため、削除ではなく
///   観測できた範囲を保持する設計にした上で、さらに位置ベースの `pool_depth` 件
///   切り詰め〔ID 依存〕もしない設計へ改めた）。
/// - `items.len() > pool_depth`（`fetch_k > pool_depth` で拡張取得できた通常経路）は
///   先頭 `pool_depth` 件の末尾スコアと `pool_depth` 番目（0-based で `pool_depth`）の
///   スコアが異なれば、境界は同点グループを切っていないため `pool_depth` 件へ
///   切り詰めて `Resolved` で返す（従来と同じ挙動）。一致する場合は境界のグループが
///   `pool_depth` を跨いでいる。そのグループと同点の要素を末尾まで走査し、グループの
///   終端が取得済み範囲内で確定できればグループ全体を含めて `Resolved` で返す。
///   取得済み範囲の最後の要素までが同点でグループ終端が確定できない場合、
///   `exhaustive` なら取得済み範囲の末尾がそのまま真の終端だと確定できるため
///   グループ全体（＝取得済み範囲全体）を含めて `Resolved` で返す。非 `exhaustive`
///   なら `Undetermined` で返す（グループを削除せず、観測できた範囲をそのまま
///   保持する。呼び出し元が再取得するか、再取得の余地がなければこの列を最終結果
///   として受理する）。
pub(crate) fn complete_boundary_tie_group_by<T>(
    items: Vec<T>,
    pool_depth: usize,
    exhaustive: bool,
    score_of: impl Fn(&T) -> f64,
) -> TieBoundary<T> {
    if pool_depth == 0 || items.len() < pool_depth {
        // 境界そのものが存在しない（`pool_depth == 0` は呼び出し元契約上
        // 到達しないが、関数を全域にするため明示的に扱う）。
        return TieBoundary::Resolved(items);
    }
    if items.len() == pool_depth {
        return if exhaustive {
            TieBoundary::Resolved(items)
        } else {
            TieBoundary::Undetermined(items)
        };
    }
    // `pool_depth >= 1`（`RrfConfig::new`/`Default` が保証）のため `pool_depth - 1` は
    // 常に有効な添字。
    let boundary_score = score_of(&items[pool_depth - 1]);
    let next_score = score_of(&items[pool_depth]);
    if boundary_score.total_cmp(&next_score) != std::cmp::Ordering::Equal {
        let mut truncated = items;
        truncated.truncate(pool_depth);
        return TieBoundary::Resolved(truncated);
    }
    // 境界のグループは `pool_depth - 1` 番目（0-based）から同点が始まっている保証は
    // ないため、グループの開始位置を後方から探す（同点は provider 契約により連続
    // する）。グループ開始位置自体は終端確定の判定には使わないため変数に保持しない。
    // グループの終端を取得済み範囲内で探す。
    let mut group_end = pool_depth;
    while group_end < items.len()
        && score_of(&items[group_end]).total_cmp(&boundary_score) == std::cmp::Ordering::Equal
    {
        group_end += 1;
    }
    if group_end < items.len() {
        // グループ終端が取得済み範囲内で確定できた（`group_end` の要素は非同点）。
        // グループ全体を含めて返す。
        let mut result = items;
        result.truncate(group_end);
        TieBoundary::Resolved(result)
    } else if exhaustive {
        // 取得済み範囲の末尾まで同点だが、`exhaustive` により取得済み範囲の末尾が
        // そのまま真の終端だと確定できる。グループ全体（＝取得済み範囲全体）を
        // 含めて返す。
        TieBoundary::Resolved(items)
    } else {
        // 取得済み範囲の最後までが同点で、かつそれ以上のデータが存在しうる
        // （非 exhaustive）ためグループ終端を確定できない。呼び出し元が再取得
        // するか、再取得の余地がなければこの観測範囲を最終結果として受理する。
        TieBoundary::Undetermined(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::CpuScalarProvider;
    use crate::sparse::SparseIndex;

    fn hit(id: u64, score: f32) -> CandidateHit {
        CandidateHit { id, score }
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
        // Issue #310: 同点順位規約の既定は `GroupEnd`（位置順位から切り替え）。
        assert_eq!(cfg.tie_rank(), TieRank::GroupEnd);
    }

    #[test]
    fn rrf_config_new_defaults_to_group_end_tie_rank() {
        // Issue #310: `RrfConfig::new`（唯一の production 呼び出し元 `sql/exec.rs` が
        // 使う経路）も `Default` と同じ既定 `GroupEnd` を返す。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 200).unwrap();
        assert_eq!(cfg.tie_rank(), TieRank::GroupEnd);
    }

    #[test]
    fn rrf_config_with_tie_rank_overrides_default() {
        // `with_tie_rank` は他フィールドを変えずに `tie_rank` だけを差し替える
        // builder 風 API。既定 `GroupEnd` からの撤回（`Positional` への 1 行復帰）を
        // 固定する。
        let cfg = RrfConfig::default().with_tie_rank(TieRank::Positional);
        assert_eq!(cfg.tie_rank(), TieRank::Positional);
        assert_eq!(cfg.k_const(), 60.0);
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

    // TASK-84（対応 Issue #61）: PoC-10 が指摘した「同点タイブレーク欠如による
    // 非決定性」の回帰テスト。密のみ・疎のみそれぞれの同一順位（rank）は
    // `dense_weight == sparse_weight` の既定設定下で同一 RRF スコアになる
    // （モジュールドキュメントの RRF 定義参照）。rank 0〜2 の 3 段で
    // 密のみ／疎のみのペアを 1 組ずつ作り（計 6 id、3 段の同点グループ）、
    // `rrf_fuse` が返す順序が「スコア降順・各同点グループ内は id 昇順」を
    // 常に満たすことを検証する。 `hybrid_search` の `truncate(k)` はこの
    // 順序をそのまま使うため（`hybrid_search` doc コメント参照）、本テストで
    // 順序そのものの決定性を保証すればグループ途中の `k` 切断でも
    // 非決定性は生じない。
    #[test]
    fn rrf_fuse_multiple_tie_groups_are_ordered_score_desc_id_asc() {
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).unwrap();
        // 各 rank の密側 id は疎側 id より大きい値にして、
        // 「挿入順（id 降順に並べても）と無関係に id 昇順が保たれる」ことを
        // 別途確認できるようにする。
        let dense = [hit(20, 3.0), hit(21, 2.0), hit(22, 1.0)];
        let sparse = [doc(10, 3.0), doc(11, 2.0), doc(12, 1.0)];
        let fused = rrf_fuse(&dense, &sparse, &cfg).expect("fuse ok");
        assert_eq!(fused.len(), 6);
        // rank 0 ペア (20, 10) が最上位の同点グループ、rank 2 ペア (22, 12) が
        // 最下位の同点グループになるはず。
        assert_eq!(
            fused.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![10, 20, 11, 21, 12, 22]
        );
        for window in fused.windows(2) {
            let (a, b) = (&window[0], &window[1]);
            assert!(
                a.score > b.score || (a.score - b.score).abs() < 1e-15,
                "fused must be sorted by score descending: {fused:?}"
            );
        }
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
        let dense: [CandidateHit; 0] = [];
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
        let dense: [CandidateHit; 0] = [];
        let sparse = [doc(2, 1.0), doc(1, 1.0)];
        let err = rrf_fuse(&dense, &sparse, &cfg).unwrap_err();
        assert_eq!(err, HybridError::UnsortedInput);
    }

    #[test]
    fn rrf_fuse_accepts_correctly_tie_broken_input() {
        let cfg = RrfConfig::default();
        // 同点スコアで id 昇順（契約通り）。
        let dense: [CandidateHit; 0] = [];
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
    /// 固定の [`CandidateHit`] 列を返す）。
    struct LeakyProvider;
    impl SearchProvider for LeakyProvider {
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            // id=1（可視）と id=99（不可視のはず）の両方を、スコア降順・同点 id 昇順の
            // 順位契約を満たしたまま返す（provider の順位契約自体には違反していない。
            // あくまで可視集合の境界を無視した契約違反のみを模す）。
            Ok(vec![
                CandidateHit { id: 1, score: 2.0 },
                CandidateHit { id: 99, score: 1.0 },
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
        // Issue #310（密プール境界の同点グループ完全化）以降、密 provider への要求件数
        // は `fetch_k = min(pool_depth * 2, MAX_POOL_DEPTH, input.ids.len())`
        // で有界化される。可視 id を 2 件（`LeakyProvider` が返す件数と一致）にして
        // `fetch_k` が可視性検証を素通りさせず、本テストが検証したい
        // `ProviderResultRejected`（長さ検証ではなく可視性検証での拒否）を引き続き
        // 固定する。
        let cfg = RrfConfig::default();
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids = [1u64, 2];
        let vectors = [1.0f32, 1.0];
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
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            Ok(vec![
                CandidateHit { id: 1, score: 3.0 },
                CandidateHit { id: 2, score: 2.0 },
                CandidateHit { id: 3, score: 1.0 },
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
        // Issue #310（密プール境界の同点グループ完全化）以降、`hybrid_search_boosted`
        // は `fetch_k = min(pool_depth * 2, MAX_POOL_DEPTH, input.ids.len())` で
        // 密 provider を呼ぶ（`pool_depth=1` に対し可視 id 3 件のため `fetch_k=2`）。
        // 契約違反 provider が要求された `k` を無視して 3 件返した場合、上限は
        // `fetch_k`（=2）で検証される。
        let err =
            hybrid_search(&OverflowingProvider, input, &index, "nomatch", 1, &cfg).unwrap_err();
        assert_eq!(err, HybridError::TooManyCandidates { len: 3, max: 2 });
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

    // TASK-111（PLAN-1・EXT-4）: ソフトブースト機構のユニットテスト。

    #[test]
    fn boost_rule_rejects_invalid_amount() {
        let ids: BTreeSet<u64> = BTreeSet::new();
        assert_eq!(
            BoostRule::new(&ids, 0.0).unwrap_err(),
            HybridError::InvalidBoost
        );
        assert_eq!(
            BoostRule::new(&ids, -0.1).unwrap_err(),
            HybridError::InvalidBoost
        );
        assert_eq!(
            BoostRule::new(&ids, f64::NAN).unwrap_err(),
            HybridError::InvalidBoost
        );
        assert_eq!(
            BoostRule::new(&ids, MAX_BOOST_AMOUNT + 0.001).unwrap_err(),
            HybridError::InvalidBoost
        );
        // 境界値ちょうどは受理される。
        assert!(BoostRule::new(&ids, MAX_BOOST_AMOUNT).is_ok());
    }

    #[test]
    fn apply_soft_boost_adds_amount_for_single_match() {
        let cfg = RrfConfig::default();
        let mut hits = vec![
            HybridHit { id: 1, score: 0.5 },
            HybridHit { id: 2, score: 0.4 },
        ];
        let ids: BTreeSet<u64> = [2].into_iter().collect();
        let rule = BoostRule::new(&ids, SOFT_BOOST_PER_MATCH).unwrap();
        apply_soft_boost(&mut hits, &[rule], &cfg).expect("ok");
        let h1 = hits.iter().find(|h| h.id == 1).unwrap();
        let h2 = hits.iter().find(|h| h.id == 2).unwrap();
        assert!((h1.score - 0.5).abs() < 1e-15);
        assert!((h2.score - (0.4 + SOFT_BOOST_PER_MATCH)).abs() < 1e-15);
    }

    #[test]
    fn apply_soft_boost_sums_multiple_rule_matches() {
        let cfg = RrfConfig::default();
        let mut hits = vec![HybridHit { id: 1, score: 0.1 }];
        let path_ids: BTreeSet<u64> = [1].into_iter().collect();
        let kind_ids: BTreeSet<u64> = [1].into_iter().collect();
        let rule_a = BoostRule::new(&path_ids, SOFT_BOOST_PER_MATCH).unwrap();
        let rule_b = BoostRule::new(&kind_ids, SOFT_BOOST_PER_MATCH).unwrap();
        apply_soft_boost(&mut hits, &[rule_a, rule_b], &cfg).expect("ok");
        assert!((hits[0].score - (0.1 + 2.0 * SOFT_BOOST_PER_MATCH)).abs() < 1e-15);
    }

    #[test]
    fn apply_soft_boost_changes_rank_order() {
        // 確定判定は加点合計を [`soft_boost_confirm_cap`]（既定 cfg では
        // `1/61 ≈ 0.0164`）未満にしか許さない。近接順位の入れ替え（PLAN-1 の
        // 想定用途）自体は真の 1 位（id=1）とプール最下位（id=3）を残したまま、
        // その中間にいる 2 件（id=2・id=3）の順位だけを逆転させることで確認する
        // （加点 0.001 はこの絶対上限より十分小さく安全に受理される）。
        let cfg = RrfConfig::default();
        let mut hits = vec![
            HybridHit { id: 1, score: 0.5 },
            HybridHit { id: 2, score: 0.45 },
            HybridHit {
                id: 3,
                score: 0.4495,
            },
        ];
        let ids: BTreeSet<u64> = [3].into_iter().collect();
        // id=3 に十分大きい加点（ただしソフト上限未満）をして id=2 と順位を逆転させる。
        let rule = BoostRule::new(&ids, 0.001).unwrap();
        apply_soft_boost(&mut hits, &[rule], &cfg).expect("ok");
        assert_eq!(hits[0].id, 1);
        assert_eq!(hits[1].id, 3);
        assert_eq!(hits[2].id, 2);
    }

    #[test]
    fn apply_soft_boost_default_amount_cannot_overtake_default_cfg_top_rank() {
        // codex-review P1・cursor bot 指摘対応の回帰: `SOFT_BOOST_PER_MATCH`
        // （既定値）で `MAX_BOOST_RULES` 件全てが同一候補へ一致しても、
        // [`RrfConfig::default`] 下で真の 1 位が取りうる保証下限（`weight /
        // (k_const + 1)` = 1/61）を上回れないことを確認する。id=2 は「0.0」ではなく
        // cursor bot 指摘どおりの真のプール最下位級スコア（弱い方のチャネル
        // （既定は等重みのため dense=sparse）の最下位順位 `pool_depth` でのみ
        // 出現した場合の `weight / (k_const + pool_depth)` = 1/260）から出発させ、
        // [`soft_boost_confirm_cap`] が有界化する最悪ケースの加点合計を与える。
        let cfg = RrfConfig::default();
        let top1_guaranteed_floor = 1.0 / 61.0;
        let pool_bottom_worst_case = 1.0 / 260.0;
        let mut hits = vec![
            HybridHit {
                id: 1,
                score: top1_guaranteed_floor,
            },
            HybridHit {
                id: 2,
                score: pool_bottom_worst_case,
            },
        ];
        let ids: BTreeSet<u64> = [2].into_iter().collect();
        let rule = BoostRule::new(&ids, SOFT_BOOST_PER_MATCH).unwrap();
        let rules: Vec<BoostRule<'_>> = (0..MAX_BOOST_RULES).map(|_| rule).collect();
        apply_soft_boost(&mut hits, &rules, &cfg).expect("ok");

        let h1 = hits.iter().find(|h| h.id == 1).unwrap();
        let h2 = hits.iter().find(|h| h.id == 2).unwrap();
        assert!(h2.score < h1.score, "h2={h2:?} must not overtake h1={h1:?}");
        // 順位（先頭が id=1 のまま）も併せて確認する。
        assert_eq!(hits[0].id, 1);
    }

    #[test]
    fn soft_boost_loose_upper_bound_matches_default_cfg_compile_time_assert_literal() {
        // `soft_boost_loose_upper_bound` 直後の compile-time assertion（`const _: ()
        // = assert!(...)`）は `2.0 / 61.0` を [`RrfConfig::default`] の `k_const`・
        // 重みから手計算したリテラルとして埋め込んでいる。`RrfConfig::default` の
        // 値を変更しても、このリテラル自体は自動更新されないため、両者が一致する
        // ことをテストで固定する（不一致になれば compile-time assertion が
        // [`soft_boost_loose_upper_bound`] の実際の計算とは無関係な値を検証するだけの
        // 張り子になり、次の `RrfConfig::default` 変更時に無言で早期拒否チェックの
        // 前提を破りうる）。
        let expected = 2.0 / 61.0;
        let actual = soft_boost_loose_upper_bound(&RrfConfig::default());
        assert!(
            (actual - expected).abs() < 1e-15,
            "soft_boost_loose_upper_bound(default)={actual} must match the const assert literal {expected}"
        );
    }

    #[test]
    fn soft_boost_confirm_cap_matches_default_cfg_compile_time_assert_literal() {
        // `soft_boost_confirm_cap` 直後の compile-time assertion（`const _: () =
        // assert!(...)`）は `1.0 / 61.0` を [`RrfConfig::default`] の `k_const`・
        // 重みから手計算したリテラルとして埋め込んでいる。`RrfConfig::default` の
        // 値を変更しても、このリテラル自体は自動更新されないため、両者が一致する
        // ことをテストで固定する（不一致になれば compile-time assertion が
        // [`soft_boost_confirm_cap`] の実際の計算とは無関係な値を検証するだけの
        // 張り子になり、次の `RrfConfig::default` 変更時に無言で確定判定チェックの
        // 前提を破りうる）。
        let expected = 1.0 / 61.0;
        let actual = soft_boost_confirm_cap(&RrfConfig::default());
        assert!(
            (actual - expected).abs() < 1e-15,
            "soft_boost_confirm_cap(default)={actual} must match the const assert literal {expected}"
        );
    }

    #[test]
    fn apply_soft_boost_rejects_total_exceeding_soft_bound() {
        // codex-review P1・cursor bot 指摘の回帰: `BoostRule::new` 単体は
        // `MAX_BOOST_AMOUNT`（1.0）まで受理するが、`apply_soft_boost` は加点合計が
        // [`soft_boost_confirm_cap`]（既定 cfg では `1/61 ≈ 0.0164`）以上なら構築
        // 成功済みのルールでも実行時に拒否する（1 ルールだけで再現できることを
        // 確認: 以前の実装は `BoostRule::new(ids, 1.0)` を渡すだけで最下位候補を
        // 新 1 位へ押し上げられた）。
        let cfg = RrfConfig::default();
        let mut hits = vec![
            HybridHit { id: 1, score: 0.5 },
            HybridHit { id: 2, score: 0.0 },
        ];
        let ids: BTreeSet<u64> = [2].into_iter().collect();
        let rule = BoostRule::new(&ids, MAX_BOOST_AMOUNT).unwrap();
        let err = apply_soft_boost(&mut hits, &[rule], &cfg).unwrap_err();
        match err {
            HybridError::BoostSoftBoundExceeded { total, max } => {
                assert!((total - MAX_BOOST_AMOUNT).abs() < 1e-15);
                assert!((max - 1.0 / 61.0).abs() < 1e-15);
            }
            other => panic!("expected BoostSoftBoundExceeded, got {other:?}"),
        }
    }

    #[test]
    fn apply_soft_boost_allows_near_top_candidate_to_overtake_true_top() {
        // 3 回目の codex-review P1 指摘の直接回帰（PR #257 レビューコメント記載の
        // 反例そのもの）: 真の 1 位（id=1・0.5）とごく僅差の第 2 候補
        // （id=2・0.4995）へ小さい加点（0.001。`SOFT_BOOST_PER_MATCH` と同オーダー）
        // をすると、過去の実装（2 回目の codex-review P1 指摘対応。候補ごとの
        // 加点合計を「真の 1 位までの差」と比較する方式）はこれを一律拒否して
        // いた。しかし真の 1 位を追い越して新たな 1 位になること自体は、近接順位を
        // 入れ替えるというソフトブースト本来の用途（PLAN-1 の意図。既定 RRF の
        // 単一チャネルでの実測順位差は約 0.000264 であり、既定値
        // `SOFT_BOOST_PER_MATCH = 0.0007` の通常適用でも同種の逆転が起こる）であり
        // 拒否すべきではない。修正後は候補ごとの加点合計を実際の順位関係
        // （真の 1 位との差）ではなく [`soft_boost_confirm_cap`]（既定 cfg では
        // `1/61 ≈ 0.0164`）という絶対上限とだけ比較するため、`0.001 < 1/61` の
        // この加点は受理され id=2 が新たな 1 位になる。
        let cfg = RrfConfig::default();
        let mut hits = vec![
            HybridHit { id: 1, score: 0.5 },
            HybridHit {
                id: 2,
                score: 0.4995,
            },
            HybridHit { id: 3, score: 0.4 },
        ];
        let ids: BTreeSet<u64> = [2].into_iter().collect();
        let rule = BoostRule::new(&ids, 0.001).unwrap();
        apply_soft_boost(&mut hits, &[rule], &cfg)
            .expect("legitimate near-top reorder must succeed");
        assert_eq!(hits[0].id, 2);
        assert_eq!(hits[1].id, 1);
        assert_eq!(hits[2].id, 3);
    }

    #[test]
    fn apply_soft_boost_rejects_boost_that_would_cross_guaranteed_floor_with_candidate_score() {
        // PR #257 codex-review P1 指摘の直接回帰: `soft_boost_confirm_cap` 単独
        // （候補の元スコアを考慮しない）判定では、プール最下位級（元スコアが
        // `cap` 未満の正の値）の候補に対して「加点単独では `cap` 未満」でも
        // 「元スコア＋加点」が `cap`（＝真の 1 位の保証下限）を上回る加点を
        // 誤って受理してしまっていた。本テストの候補（id=2）は元スコアが `cap`
        // よりわずかに小さく、加点単独は `cap` を下回るが、元スコアを加えると
        // `cap` を超える。修正後は `cap - hit.score`（残余）と比較するため拒否
        // される。
        let cfg = RrfConfig::default();
        let cap = soft_boost_confirm_cap(&cfg);
        let near_cap_score = cap - 0.005;
        let mut hits = vec![
            HybridHit { id: 1, score: cap },
            HybridHit {
                id: 2,
                score: near_cap_score,
            },
        ];
        let ids: BTreeSet<u64> = [2].into_iter().collect();
        // 加点単独 (0.006) は cap (≈0.01639) を下回るが、元スコア (cap - 0.005) と
        // 合わせると cap を超える。
        let boost_amount = 0.006;
        assert!(
            boost_amount < cap,
            "test premise: boost alone must stay under cap"
        );
        assert!(
            near_cap_score + boost_amount > cap,
            "test premise: score + boost must cross the guaranteed floor"
        );
        let rule = BoostRule::new(&ids, boost_amount).expect("rule ok");
        let err = apply_soft_boost(&mut hits, &[rule], &cfg).unwrap_err();
        match err {
            HybridError::BoostSoftBoundExceeded { total, max } => {
                assert!((total - boost_amount).abs() < 1e-15);
                assert!((max - 0.005).abs() < 1e-12, "max={max}");
            }
            other => panic!("expected BoostSoftBoundExceeded, got {other:?}"),
        }
    }

    #[test]
    fn apply_soft_boost_rejects_boost_exceeding_cap_for_near_top_candidate() {
        // PR #257 codex-review P1 指摘（4 回目）の直接回帰: `hit.score >= cap` の
        // 候補（元スコア単独で既に保証下限相当の近接順位級）を確定判定から
        // 無条件に `continue` していたため、`BoostRule::new` が許す範囲内で
        // `soft_boost_confirm_cap` を大幅に超える加点を同一 id へ積み上げられて
        // いた。本テストは指摘のとおり合計 0.03 の加点（`soft_boost_confirm_cap`
        // ≈ 0.01639 を超えるが `soft_boost_loose_upper_bound` ≈ 0.03279 未満）を
        // 元スコアが `cap` 以上の候補へ与える。修正後は近接順位の逆転自体は
        // 許しつつ、加点量自体（`candidate_boost`）が `cap` 未満であることを
        // 全候補に要求するため拒否される。
        let cfg = RrfConfig::default();
        let cap = soft_boost_confirm_cap(&cfg);
        let mut hits = vec![
            HybridHit { id: 1, score: cap },
            HybridHit { id: 2, score: 0.0 },
        ];
        let ids: BTreeSet<u64> = [1].into_iter().collect();
        let boost_amount_each = 0.015;
        let total = boost_amount_each * 2.0;
        assert!(total > cap, "test premise: total boost must exceed cap");
        assert!(
            total < soft_boost_loose_upper_bound(&cfg),
            "test premise: total boost must pass the early loose check"
        );
        let rule_a = BoostRule::new(&ids, boost_amount_each).expect("rule ok");
        let rule_b = BoostRule::new(&ids, boost_amount_each).expect("rule ok");
        let err = apply_soft_boost(&mut hits, &[rule_a, rule_b], &cfg).unwrap_err();
        match err {
            HybridError::BoostSoftBoundExceeded {
                total: got_total,
                max,
            } => {
                assert!((got_total - total).abs() < 1e-15);
                assert!((max - cap).abs() < 1e-12, "max={max}");
            }
            other => panic!("expected BoostSoftBoundExceeded, got {other:?}"),
        }
    }

    #[test]
    fn boost_rule_construction_rejects_ids_set_exceeding_max_boost_ids() {
        // PR #257 codex-review P2 指摘の直接回帰: `BoostRule::new` は `ids` の
        // 要素数を検証せず、[`hybrid_search_boosted`] の早期拒否ループが検索対象
        // `hits` に含まれない id まで無制限に走査・複製できてしまっていた。
        // `ids.len() > MAX_BOOST_IDS` はアロケーション・走査（`BTreeMap` への複製）
        // より前に拒否される。
        let ids: BTreeSet<u64> = (0..=(MAX_BOOST_IDS as u64)).collect();
        assert_eq!(ids.len(), MAX_BOOST_IDS + 1);
        assert_eq!(
            BoostRule::new(&ids, SOFT_BOOST_PER_MATCH).unwrap_err(),
            HybridError::TooManyBoostIds {
                len: MAX_BOOST_IDS + 1,
                max: MAX_BOOST_IDS,
            }
        );
    }

    #[test]
    fn apply_soft_boost_tie_breaks_by_id_ascending() {
        let cfg = RrfConfig::default();
        let mut hits = vec![
            HybridHit { id: 5, score: 0.1 },
            HybridHit { id: 3, score: 0.1 },
        ];
        apply_soft_boost(&mut hits, &[], &cfg).expect("ok");
        assert_eq!(hits[0].id, 3);
        assert_eq!(hits[1].id, 5);
    }

    #[test]
    fn apply_soft_boost_empty_rules_is_no_op() {
        let cfg = RrfConfig::default();
        let mut hits = vec![
            HybridHit { id: 2, score: 0.5 },
            HybridHit { id: 1, score: 0.4 },
        ];
        let before = hits.clone();
        apply_soft_boost(&mut hits, &[], &cfg).expect("ok");
        assert_eq!(hits, before);
    }

    #[test]
    fn apply_soft_boost_rejects_too_many_rules() {
        let cfg = RrfConfig::default();
        let ids: BTreeSet<u64> = BTreeSet::new();
        let rule = BoostRule::new(&ids, SOFT_BOOST_PER_MATCH).unwrap();
        let rules: Vec<BoostRule<'_>> = (0..(MAX_BOOST_RULES + 1)).map(|_| rule).collect();
        let mut hits = vec![HybridHit { id: 1, score: 0.1 }];
        let err = apply_soft_boost(&mut hits, &rules, &cfg).unwrap_err();
        assert_eq!(
            err,
            HybridError::TooManyBoostRules {
                len: MAX_BOOST_RULES + 1,
                max: MAX_BOOST_RULES,
            }
        );
    }

    #[test]
    fn apply_soft_boost_rejects_non_finite_result() {
        // `apply_soft_boost` は `hits`（`rrf_fuse` の出力に限らず任意の呼び出し元が
        // 渡しうる）を加点前提とせず、加点上限の算出（[`soft_boost_confirm_cap`]
        // との比較）より前に全件の有限性を検証する。空ルールでも非有限な入力自体は
        // 拒否されることを確認する（ルール一致の有無に関わらず検証される契約の
        // 確認）。
        let cfg = RrfConfig::default();
        let mut hits = vec![HybridHit {
            id: 1,
            score: f64::INFINITY,
        }];
        let err = apply_soft_boost(&mut hits, &[], &cfg).unwrap_err();
        assert_eq!(err, HybridError::NonFiniteScore);
    }

    #[test]
    fn path_hint_matches_substring_and_rejects_empty_hint() {
        assert!(path_hint_matches("src/hybrid", "src/hybrid.rs"));
        assert!(!path_hint_matches("src/other", "src/hybrid.rs"));
        assert!(!path_hint_matches("", "src/hybrid.rs"));
    }

    #[test]
    fn kind_hint_matches_exact_and_rejects_empty_hint() {
        assert!(kind_hint_matches("doc", "doc"));
        assert!(!kind_hint_matches("doc", "docx"));
        assert!(!kind_hint_matches("", "doc"));
    }

    #[test]
    fn hybrid_search_boosted_with_empty_rules_matches_hybrid_search() {
        let cfg = RrfConfig::default();
        let vectors: Vec<f32> = vec![1.0, 0.0, 0.0, 1.0];
        let ids = [1u64, 2u64];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 2,
        };
        let index = SparseIndex::build(&[(1, "cat dog"), (2, "bird fish")]).expect("build index");

        let plain = hybrid_search(&CpuScalarProvider, input, &index, "cat", 2, &cfg).expect("ok");
        let input2 = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 2,
        };
        let boosted =
            hybrid_search_boosted(&CpuScalarProvider, input2, &index, "cat", 2, &cfg, &[])
                .expect("ok");
        assert_eq!(plain, boosted);
    }

    #[test]
    fn hybrid_search_boosted_rejects_boost_when_heavier_channel_is_empty() {
        // 2 回目の codex-review P1 指摘の直接回帰: `dense_weight=1`・`sparse_weight=100`
        // で疎チャネルがクエリ不一致により空になる場合、以前の実装
        // （`soft_boost_ceiling` が `max(dense_weight, sparse_weight) / (k_const + 1)`
        // ＝重みが大きい方のチャネルに必ず 1 位候補があると誤仮定）は上限を
        // `≈100/61` と過大評価し、`BoostRule::new(..., 1.0)` を受理してしまっていた。
        // [`soft_boost_confirm_cap`] は `min(dense_weight, sparse_weight) /
        // (k_const + 1)` = `1/61 ≈ 0.0164`（弱い方のチャネルの寄与だけを仮定する
        // 安全側）を使うため、重みが大きい方のチャネルが空でも正しく拒否される
        // ことを確認する。
        let cfg = RrfConfig::new(60.0, 1.0, 100.0, 200).expect("valid cfg");
        let vectors: Vec<f32> = vec![1.0, 0.0, 0.0, 1.0];
        let ids = [1u64, 2u64];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 2,
        };
        // corpus には "cat"/"dog"/"bird"/"fish" しか含まれないため、"zzz" は
        // どちらの doc とも一致せず疎チャネルの結果は空になる。
        let index = SparseIndex::build(&[(1, "cat dog"), (2, "bird fish")]).expect("build index");
        let ids2: BTreeSet<u64> = [2].into_iter().collect();
        let rule = BoostRule::new(&ids2, MAX_BOOST_AMOUNT).expect("valid rule");
        let err = hybrid_search_boosted(&CpuScalarProvider, input, &index, "zzz", 2, &cfg, &[rule])
            .expect_err("must reject: heavier (sparse) channel is empty");
        assert!(matches!(err, HybridError::BoostSoftBoundExceeded { .. }));
    }

    #[test]
    fn hybrid_search_boosted_accepts_boost_on_empty_pool_as_no_op() {
        // 融合プールが両チャネルとも空（可視集合が空のため密・疎いずれも候補化
        // されない）の場合、`apply_soft_boost` の確定判定ループは走査対象の
        // `hits` 自体が空のため何も拒否しない（守るべき候補がそもそも存在しない
        // ため安全）。`cfg` だけで判定する早期チェック
        // （[`soft_boost_loose_upper_bound`]）を通過した後、この最終判定でも
        // 誤って拒否しないことを確認する回帰。
        let cfg = RrfConfig::new(60.0, 100.0, 1.0, 200).expect("valid cfg");
        let vectors: Vec<f32> = vec![];
        let ids: [u64; 0] = [];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 1,
        };
        let index = SparseIndex::build(&[(1, "cat dog"), (2, "bird fish")]).expect("build index");
        let boost_ids: BTreeSet<u64> = [2].into_iter().collect();
        let rule = BoostRule::new(&boost_ids, MAX_BOOST_AMOUNT).expect("valid rule");
        // 可視集合が空のため密チャネルは常に空（`CpuScalarProvider` は
        // `input.ids.is_empty()` で早期に空を返す）。疎チャネルも `visible_ids` が
        // 空集合のため `search_within` の可視集合フィルタで空になる。
        let result =
            hybrid_search_boosted(&CpuScalarProvider, input, &index, "cat", 1, &cfg, &[rule]);
        let hits = result.expect("empty pool must not be rejected");
        assert!(hits.is_empty());
    }

    // --- Issue #310: 同点順位規約（`TieRank`）・境界同点グループ完全化 ---

    #[test]
    fn rrf_fuse_group_end_tie_rank_assigns_group_tail_rank_to_all_members() {
        // dense 側 3 件が全て同点（score=1.0）の場合、`TieRank::GroupEnd`
        // （modified competition ranking）はグループ末尾の順位（=3）を全員に
        // 割り当てる。寄与は `weight / (k_const + 3)` = `1.0 / 63.0` で 3 件とも
        // 同一になり、その後の同点タイブレーク（id 昇順）で並ぶ。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).unwrap();
        assert_eq!(cfg.tie_rank(), TieRank::GroupEnd);
        let dense = [hit(1, 1.0), hit(2, 1.0), hit(3, 1.0)];
        let out = rrf_fuse(&dense, &[], &cfg).expect("fuse ok");
        let expected_score = 1.0 / 63.0;
        for h in &out {
            assert!((h.score - expected_score).abs() < 1e-12);
        }
        assert_eq!(out.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn rrf_fuse_positional_tie_rank_matches_legacy_behavior() {
        // `TieRank::Positional` は従来の位置順位挙動と bit 一致する: 同点でも
        // 列内の位置（1-based）をそのまま順位にするため、id=1 は寄与
        // `1.0/61.0`、id=2 は `1.0/62.0`、id=3 は `1.0/63.0` になり互いに異なる。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10)
            .unwrap()
            .with_tie_rank(TieRank::Positional);
        let dense = [hit(1, 1.0), hit(2, 1.0), hit(3, 1.0)];
        let out = rrf_fuse(&dense, &[], &cfg).expect("fuse ok");
        let scores: Vec<f64> = out.iter().map(|h| h.score).collect();
        assert!((scores[0] - 1.0 / 61.0).abs() < 1e-12);
        assert!((scores[1] - 1.0 / 62.0).abs() < 1e-12);
        assert!((scores[2] - 1.0 / 63.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_fuse_group_end_tie_rank_leaves_distinct_scores_unaffected() {
        // 同点を含まない列では `GroupEnd` と `Positional` の結果は一致する
        // （各要素が独立したグループを構成し、グループ末尾＝自身の位置になるため）。
        let cfg_group_end = RrfConfig::new(60.0, 1.0, 1.0, 10).unwrap();
        let cfg_positional = cfg_group_end.with_tie_rank(TieRank::Positional);
        let dense = [hit(1, 3.0), hit(2, 2.0), hit(3, 1.0)];
        let a = rrf_fuse(&dense, &[], &cfg_group_end).expect("fuse ok");
        let b = rrf_fuse(&dense, &[], &cfg_positional).expect("fuse ok");
        assert_eq!(a, b);
    }

    /// テスト専用ヘルパ: [`TieBoundary::Resolved`] であることを検証しつつ内側の
    /// `Vec<T>` を取り出す。`Undetermined` だった場合はパニックさせてテストを
    /// 失敗させる（本関数はテストコードのみから呼ばれ、production コードの
    /// untrusted 入力経路には現れない）。
    fn expect_resolved<T: std::fmt::Debug>(out: TieBoundary<T>) -> Vec<T> {
        match out {
            TieBoundary::Resolved(v) => v,
            TieBoundary::Undetermined(v) => {
                panic!("expected Resolved, got Undetermined({v:?})")
            }
        }
    }

    /// [`expect_resolved`] と対になる、[`TieBoundary::Undetermined`] 専用の
    /// テストヘルパ。
    fn expect_undetermined<T: std::fmt::Debug>(out: TieBoundary<T>) -> Vec<T> {
        match out {
            TieBoundary::Undetermined(v) => v,
            TieBoundary::Resolved(v) => {
                panic!("expected Undetermined, got Resolved({v:?})")
            }
        }
    }

    #[test]
    fn complete_boundary_tie_group_returns_input_unchanged_when_no_boundary() {
        // `dense.len() <= pool_depth` の場合は境界そのものが存在しないためそのまま
        // `Resolved` で返す。
        let dense = vec![hit(1, 3.0), hit(2, 2.0)];
        let out = expect_resolved(complete_boundary_tie_group(dense.clone(), 5, false));
        assert_eq!(out, dense);
    }

    #[test]
    fn complete_boundary_tie_group_truncates_when_boundary_does_not_split_a_tie() {
        // 境界（pool_depth=2）の直後（3 番目）のスコアが境界末尾（2 番目）と異なる
        // ため、同点グループを分断していない。従来どおり先頭 `pool_depth` 件へ
        // 切り詰めて `Resolved` で返す。
        let dense = vec![hit(1, 3.0), hit(2, 2.0), hit(3, 1.0)];
        let out = expect_resolved(complete_boundary_tie_group(dense, 2, false));
        assert_eq!(out, vec![hit(1, 3.0), hit(2, 2.0)]);
    }

    #[test]
    fn complete_boundary_tie_group_includes_full_group_when_boundary_splits_a_tie() {
        // 境界（pool_depth=2）の 2 番目・3 番目が同点（score=2.0）で、4 番目は
        // 非同点（score=1.0）のためグループ終端が取得済み範囲内で確定できる。
        // グループ全体（id=1,2,3）を含めて `Resolved` で返す（`exhaustive` に
        // 関わらず確定できるケース）。
        let dense = vec![hit(1, 2.0), hit(2, 2.0), hit(3, 2.0), hit(4, 1.0)];
        let out = expect_resolved(complete_boundary_tie_group(dense, 2, false));
        assert_eq!(out, vec![hit(1, 2.0), hit(2, 2.0), hit(3, 2.0)]);
    }

    #[test]
    fn complete_boundary_tie_group_is_undetermined_when_tail_is_unconfirmed_and_not_exhaustive() {
        // 取得済み範囲（4 件）の最後まで同点（score=2.0）が続き、`exhaustive=false`
        // （取得範囲を超えてなお可視集合にデータが残りうる）のためグループの終端を
        // 確定できない。Issue #320 codex-review P1 指摘対応: 位置ベースの
        // `pool_depth` 件切り詰め（ID 昇順で最小 ID だけが残る id 依存バイアス）は
        // せず、`Undetermined` として観測できた同点グループの全メンバーをそのまま
        // 返す。呼び出し元（[`hybrid_search_boosted`]）が再取得するか、再取得の
        // 余地がなければこの列を最終結果として受理する。
        let dense = vec![hit(1, 2.0), hit(2, 2.0), hit(3, 2.0), hit(4, 2.0)];
        let out = expect_undetermined(complete_boundary_tie_group(dense.clone(), 2, false));
        assert_eq!(
            out, dense,
            "終端未確定でも観測できた同点グループの全メンバーを保持する（部分採用しない）"
        );
    }

    #[test]
    fn complete_boundary_tie_group_includes_group_when_tail_reaches_end_and_exhaustive() {
        // 取得済み範囲の最後まで同点が続くのは上記の未確定ケースと同じだが、
        // `exhaustive=true`（`fetch_k` が可視集合全体を覆っており取得済み範囲の
        // 末尾が真の終端だと確定できる）の場合はグループを除外せず全体を含めて
        // `Resolved` で返す（Issue #310 実装時に確認した回帰の直接固定: `fetch_k`
        // が可視集合サイズと一致する場合に常に除外へ倒れると、確定できるはずの
        // グループまで失う）。
        let dense = vec![hit(1, 2.0), hit(2, 2.0), hit(3, 2.0), hit(4, 2.0)];
        let out = expect_resolved(complete_boundary_tie_group(dense.clone(), 2, true));
        assert_eq!(out, dense);
    }

    #[test]
    fn complete_boundary_tie_group_is_id_independent() {
        // 同一の同点集合を id の割り当てだけ入れ替えても、`Undetermined` として
        // 観測できるグループの要素数（id 集合の大きさ）は変わらない（Issue #310 の
        // 目的である id 依存バイアスの除去を固定する）。
        let dense_a = vec![hit(1, 2.0), hit(2, 2.0), hit(3, 2.0), hit(9, 1.0)];
        let dense_b = vec![hit(10, 2.0), hit(20, 2.0), hit(30, 2.0), hit(90, 1.0)];
        let out_a = expect_resolved(complete_boundary_tie_group(dense_a, 2, false));
        let out_b = expect_resolved(complete_boundary_tie_group(dense_b, 2, false));
        assert_eq!(out_a.len(), 3);
        assert_eq!(out_b.len(), 3);
    }

    #[test]
    fn complete_boundary_tie_group_by_supports_f64_scored_doc_type() {
        // Issue #310 codex-review P1 指摘（threadId PRRT_kwDOUAKASM6dbmAw）対応:
        // `complete_boundary_tie_group`（`CandidateHit`・`score: f32` 専用）を
        // 汎用化した `complete_boundary_tie_group_by` が、疎側 `ScoredDoc`
        // （`score: f64`）でも同じ完全化ロジックを適用できることを固定する。
        let docs = vec![doc(1, 2.0), doc(2, 2.0), doc(3, 2.0), doc(4, 1.0)];
        let out = expect_resolved(complete_boundary_tie_group_by(docs, 2, false, |d| d.score));
        assert_eq!(out, vec![doc(1, 2.0), doc(2, 2.0), doc(3, 2.0)]);
    }

    #[test]
    fn complete_boundary_tie_group_is_undetermined_when_fetch_capped_at_pool_depth_and_not_exhaustive(
    ) {
        // `items.len() == pool_depth` かつ非 `exhaustive`（境界＝末尾要素が未取得
        // 候補と同点かどうか比較対象なしに判定できないケース。[`hybrid_search_boosted`]
        // の再取得ループが [`MAX_FETCH_K`]・可視集合の大きさで頭打ちになった場合に
        // 相当する。本テストでは境界処理だけを直接検証するため
        // `pool_depth == items.len()` で模擬する）は `Undetermined` を返す
        // （Issue #320 codex-review P1 指摘対応: 丸ごと除外すると全件同点コーパスで
        // 結果が空になる回帰があった）。
        let dense = vec![hit(1, 2.0), hit(2, 2.0), hit(3, 2.0)];
        let out = expect_undetermined(complete_boundary_tie_group(dense.clone(), 3, false));
        assert_eq!(
            out, dense,
            "拡張取得できず境界の同点を確定できない場合も観測範囲を保持する"
        );
    }

    #[test]
    fn complete_boundary_tie_group_keeps_tail_when_fetch_capped_at_pool_depth_but_exhaustive() {
        // 上記テストと対になるケース: `dense.len() == pool_depth` でも
        // `exhaustive == true`（取得済み範囲がそのまま真の終端だと確定できる）なら
        // 末尾の同点グループを除外せず `Resolved` でそのまま返す。
        let dense = vec![hit(1, 2.0), hit(2, 2.0), hit(3, 2.0)];
        let out = expect_resolved(complete_boundary_tie_group(dense.clone(), 3, true));
        assert_eq!(out, dense);
    }

    #[test]
    fn hybrid_search_boosted_sparse_tie_group_across_pool_boundary_is_id_independent() {
        // Issue #310 codex-review P1 指摘（threadId PRRT_kwDOUAKASM6dbmAw）の回帰
        // 固定: 疎チャネル（BM25）も密チャネルと同様に境界の同点グループを完全化
        // する。6 件すべてが同一テキスト（BM25 同点）・同一密ベクトル（内積同点）
        // のコーパスに対し `pool_depth=2` で検索すると、初期 `fetch_k`
        // （`min(2*2, 6)=4`）は可視 6 件に満たず非 exhaustive のため境界の同点
        // グループを確定できないが、Issue #320 codex-review P1 指摘対応の再取得
        // ループ（`fetch_k` を倍増して再フェッチ）が `fetch_k=6` まで伸ばし、
        // 可視集合全体を覆って `exhaustive=true` になるため確定できる（密・疎とも
        // 6 件全体を保持）。修正前は疎側が `search_within` 内部の doc_id 昇順
        // タイブレークで id 依存に 2 件だけ生き残っていた。id の割り当てを入れ替えて
        // も結果件数（空にならないこと）が変わらないことを確認する。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 2).unwrap();
        let vectors: Vec<f32> = vec![1.0; 6];
        let query = [1.0f32];

        let ids_a: Vec<u64> = vec![1, 2, 3, 4, 5, 6];
        let docs_a: Vec<(u64, &str)> = ids_a.iter().map(|&id| (id, "cat")).collect();
        let index_a = SparseIndex::build(&docs_a).expect("build ok");
        let input_a = SearchInput {
            ids: &ids_a,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 2,
        };
        let out_a = hybrid_search(&CpuScalarProvider, input_a, &index_a, "cat", 2, &cfg)
            .expect("search ok");

        let ids_b: Vec<u64> = vec![101, 202, 303, 404, 505, 606];
        let docs_b: Vec<(u64, &str)> = ids_b.iter().map(|&id| (id, "cat")).collect();
        let index_b = SparseIndex::build(&docs_b).expect("build ok");
        let input_b = SearchInput {
            ids: &ids_b,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 2,
        };
        let out_b = hybrid_search(&CpuScalarProvider, input_b, &index_b, "cat", 2, &cfg)
            .expect("search ok");

        assert_eq!(out_a.len(), out_b.len());
        assert_eq!(
            out_a.len(),
            2,
            "境界の同点グループを確定できなくても観測範囲（pool_depth 件）は保持され空にならない"
        );
    }

    #[test]
    fn hybrid_search_retention_pool_scores_are_permutation_invariant() {
        // Issue #320 codex-review P1 指摘対応の意図（境界同点グループの完全化が
        // 位置ベースの部分採用に頼らないこと）を、id の割り当てを変えても融合
        // スコアが変わらないことで固定する。ここで検証するのは「取得済み同点
        // グループの融合スコアが id に依存しない」こと（`TieRank::GroupEnd` が
        // グループ全体を見て順位を決めるための前提）である。「同点 m 件から `k`
        // 件へ絞り込む際にどの id が生き残るか」は本関数ドキュメント（`truncate(k)`
        // は id 昇順で確定的）が定める既存の別契約であり、Issue #310/#320 の
        // 対象（境界完全化がグループの一部だけを取り込んでしまう問題）ではない
        // ため、本テストでは意図的にアサーションしない。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 2).unwrap();
        let vectors: Vec<f32> = vec![1.0; 6];
        let query = [1.0f32];

        let ids_a: Vec<u64> = vec![1, 2, 3, 4, 5, 6];
        let docs_a: Vec<(u64, &str)> = ids_a.iter().map(|&id| (id, "cat")).collect();
        let index_a = SparseIndex::build(&docs_a).expect("build ok");
        let input_a = SearchInput {
            ids: &ids_a,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 2,
        };
        let out_a = hybrid_search(&CpuScalarProvider, input_a, &index_a, "cat", 2, &cfg)
            .expect("search ok");

        let ids_b: Vec<u64> = vec![101, 202, 303, 404, 505, 606];
        let docs_b: Vec<(u64, &str)> = ids_b.iter().map(|&id| (id, "cat")).collect();
        let index_b = SparseIndex::build(&docs_b).expect("build ok");
        let input_b = SearchInput {
            ids: &ids_b,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 2,
        };
        let out_b = hybrid_search(&CpuScalarProvider, input_b, &index_b, "cat", 2, &cfg)
            .expect("search ok");

        assert_eq!(out_a.len(), 2);
        assert_eq!(out_b.len(), 2);
        let scores_a: Vec<f64> = out_a.iter().map(|h| h.score).collect();
        let scores_b: Vec<f64> = out_b.iter().map(|h| h.score).collect();
        assert_eq!(
            scores_a, scores_b,
            "6 件全件同点コーパスの融合スコアは id 割り当てに依存しない"
        );
        assert!(
            (scores_a[0] - scores_a[1]).abs() < 1e-12,
            "TieRank::GroupEnd により同点グループ内の全メンバーは同一の融合スコア（グループ末尾順位）を持つ"
        );
    }

    #[test]
    fn rrf_fuse_with_limits_rejects_dense_limit_above_max_fetch_k() {
        // `dense_limit`（`sparse_limit` も同様）は独自の fail-closed 検証を持つ:
        // 境界同点グループ完全化（Issue #310・Issue #320）の再取得ループが
        // `fetch_k` を `pool_depth` 超まで伸ばすことがあるため上限は
        // `MAX_POOL_DEPTH` ではなく `MAX_FETCH_K`。それを超える上限は構造体
        // リテラル相当の検証迂回になるため拒否する。
        let cfg = RrfConfig::default();
        let err = rrf_fuse_with_limits(&[], MAX_FETCH_K + 1, &[], 1, &cfg).unwrap_err();
        assert_eq!(err, HybridError::InvalidConfig);
    }

    /// [`SearchProvider`] の契約違反（`pool_depth` 境界より後ろ、しかし取得した
    /// `fetch_k` 件の範囲内で順序契約〔スコア降順〕に違反する）を模したモック
    /// provider。`complete_boundary_tie_group` が境界で切り詰める範囲の**外側**
    /// （＝切り詰め後には残らない末尾）に契約違反を仕込む（codex-review P1 指摘・
    /// threadId PRRT_kwDOUAKASM6dbhNv 対応の回帰固定）。
    struct TailUnsortedProvider;
    impl SearchProvider for TailUnsortedProvider {
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            // pool_depth=2 の境界は先頭 2 件（score=3, 2）。3 件目以降（score=1, 4）は
            // 降順契約に違反する（1 の次に 4 が来ている）が、境界完全化の切り詰めで
            // ちょうど除去される位置に置く。
            Ok(vec![
                CandidateHit { id: 1, score: 3.0 },
                CandidateHit { id: 2, score: 2.0 },
                CandidateHit { id: 3, score: 1.0 },
                CandidateHit { id: 4, score: 4.0 },
            ])
        }
    }

    #[test]
    fn hybrid_search_rejects_dense_provider_tail_contract_violation_beyond_pool_boundary() {
        // codex-review P1 指摘（threadId PRRT_kwDOUAKASM6dbhNv）の回帰固定:
        // `complete_boundary_tie_group` は `dense_hits`（拡張取得列）を境界で
        // 切り詰めるが、切り詰められて消える末尾部分の契約違反（順序崩れ・重複 id・
        // 非有限スコア）が検証を迂回して正常な検索結果として受理されてはならない
        // （fail-closed 方針。coding-rust.md）。`pool_depth=2` で provider が
        // `[score=3, 2, 1, 4]` を返すケース（末尾の `1 → 4` が降順契約に違反）で、
        // 境界完全化の切り詰め後は `[3, 2]` のみが `rrf_fuse_with_limits` へ渡り
        // 契約違反が見逃されていた（修正前は Ok を返してしまう）。修正後は拡張列
        // 全体を切り詰め前に検証し `UnsortedInput` で拒否する。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 2).unwrap();
        let index = SparseIndex::build(&[(1, "dummy")]).expect("build ok");
        let ids = [1u64, 2, 3, 4];
        let vectors = [1.0f32, 1.0, 1.0, 1.0];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 1,
        };
        let err =
            hybrid_search(&TailUnsortedProvider, input, &index, "nomatch", 2, &cfg).unwrap_err();
        assert_eq!(err, HybridError::UnsortedInput);
    }

    #[test]
    fn hybrid_search_boosted_dense_tie_group_across_pool_boundary_is_id_independent() {
        // 統合レベル: 密チャネルの同点グループが `pool_depth` 境界を跨ぐ構成で、
        // id の割り当てを入れ替えても融合結果の id 集合が変わらないことを確認する
        // （`complete_boundary_tie_group`＋`fetch_k` 拡張が本関数経由でも効くことの
        // 固定）。`CpuScalarProvider` は内積スコアを返すため、単位ベクトルと
        // 直交・非直交の組み合わせで同点グループを作る。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 2).unwrap();
        let index = SparseIndex::build(&[(1, "unrelated")]).expect("build ok");
        // 全件が dim=1・vector=1.0 で内積スコアが全て同一（同点グループがプール
        // 全体を覆う）になるよう構成する。
        let ids: Vec<u64> = vec![1, 2, 3, 4];
        let vectors: Vec<f32> = vec![1.0, 1.0, 1.0, 1.0];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 1,
        };
        let out = hybrid_search(&CpuScalarProvider, input, &index, "nomatch", 2, &cfg)
            .expect("search ok");
        let out_ids: std::collections::BTreeSet<u64> = out.iter().map(|h| h.id).collect();
        assert_eq!(
            out_ids.len(),
            out.len(),
            "決定的な id 昇順タイブレークで重複なし"
        );
    }

    #[test]
    fn hybrid_search_boosted_iterated_dense_tie_group_is_bit_stable() {
        // 決定性回帰: 同じ入力で 20 回繰り返し呼んでも常に同一の出力になる
        // （`BTreeMap` 累積・`sort_by` 安定ソートに依存する既存の決定性契約が
        // `TieRank::GroupEnd`・境界完全化の追加後も保たれることを固定する）。
        let cfg = RrfConfig::new(60.0, 1.0, 1.0, 2).unwrap();
        let index = SparseIndex::build(&[(1, "unrelated")]).expect("build ok");
        let ids: Vec<u64> = vec![1, 2, 3, 4];
        let vectors: Vec<f32> = vec![1.0, 1.0, 1.0, 1.0];
        let query = [1.0f32];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 1,
            query: &query,
            k: 2,
        };
        let first = hybrid_search(&CpuScalarProvider, input, &index, "nomatch", 2, &cfg)
            .expect("search ok");
        for _ in 0..20 {
            let input = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: 1,
                query: &query,
                k: 2,
            };
            let repeat = hybrid_search(&CpuScalarProvider, input, &index, "nomatch", 2, &cfg)
                .expect("search ok");
            assert_eq!(first, repeat);
        }
    }
}
