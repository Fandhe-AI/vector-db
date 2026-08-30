//! 検索カーネルのリランキング層（TASK-107。対象ビヘイビア: SEARCH-6, SEARCH-7, SEARCH-8）。
//!
//! `hybrid.rs`（TASK-103）が生成する候補プール（[`crate::hybrid::HybridHit`] の列。
//! 既定 `pool_depth = 200`）を受け取り、最終上位 `final_k` 件へ再順位付けする層。
//! `hybrid.rs`・`sparse.rs` と同様に storage・catalog・policy とは結線しない純粋関数的な
//! 層として追加する。RLS 相当の可視性判定はこの層より上（`core.rs` 相当）で完結して
//! いる前提であり、[`rerank_candidates`] へ渡す `candidates`（[`RerankCandidate`] の
//! 列）は呼び出し元があらかじめ可視行のみへ縮約済みであることを契約とする。
//!
//! リランカー方式（クロスエンコーダ等）の最終選定は spec 上オーナー判断（TASK-107 は
//! 共同タスク）であり、外部 ML 推論クレートの導入は依存承認制
//! （`.claude/rules/dependency-policy.md`）に抵触するため、本モジュールでは方式を
//! 差し替え可能にする [`Reranker`] trait（object-safe。[`crate::kernel::SearchProvider`]
//! と同じ `&self` のみの制約）を公開 API の中心に据える。依存追加なしで動く決定的な
//! 参照実装を 2 種（[`IdentityReranker`]・[`LexicalOverlapReranker`]）同梱するが、
//! いずれも本命方式ではなく方式確定までの暫定実装である。
//!
//! `hybrid.rs` の provider・index 結果検証と同じ理由で、[`Reranker`] trait object は
//! 型で「入力候補 id 集合の部分集合のみを返す」ことを強制できない別個のオブジェクトの
//! ため、[`rerank_candidates`] は出力を検証し、契約違反（候補外 id・重複・順序契約
//! 違反・非有限スコア・件数超過）を 1 件でも検知したら部分的に受理せず
//! [`RerankError`] で検索全体を拒否する（fail-closed。事後フィルタは件数差から
//! 不可視データの有無が漏れる経路になるため使わない）。
//!
//! SEARCH-6（候補プール品質の前提）・SEARCH-7（リランキング層本体）は本モジュールが
//! 対応する。SEARCH-8（効果測定の追跡）は TASK-108（Issue #39）が担当し、本モジュールは
//! その効果を測定・主張しない（`crates/engine/tests/rerank.rs` の `//!` も参照）。
//! `VectorCore` trait への統合・SQL 表層統合は後続タスクの管轄でありここでは扱わない。

use std::fmt;

use crate::sparse::tokenize;

/// リランキング設定の上限。`hybrid.rs::MAX_POOL_DEPTH` と同桁を採用し、未検証の
/// 巨大な値がそのままアロケーションサイズへ伝播することを防ぐ
/// （coding-rust.md「無制限確保禁止」）。
const MAX_POOL_DEPTH: usize = 10_000;

/// `query_text` のバイト長上限。[`LexicalOverlapReranker::rerank`] は `query_text` を
/// `tokenize()`（`String`・`BTreeSet` を確保する）へ渡すため、`sparse.rs::MAX_QUERY_BYTES`
/// と同じ理由・同じ値で `tokenize()` を呼ぶ前に `query_text.len()`（アロケーション不要）
/// で判定し、[`rerank_candidates`] の入口で fail-closed に拒否する。
const MAX_QUERY_TEXT_BYTES: usize = 16 * 1024;

/// 候補 1 件（`RerankCandidate::text`）のバイト長上限。`sparse.rs::MAX_DOC_BYTES` と
/// 同じ理由・同じ値を採用する。候補件数自体は [`MAX_POOL_DEPTH`]（`cfg.pool_depth()`）で
/// 別途上限があるため、この上限と合わせて候補側の走査・アロケーションコストの総量を
/// 有界に保つ（[`MAX_CANDIDATE_TEXT_BYTES`] は 1 件あたり、[`MAX_POOL_DEPTH`] は件数の
/// 上限で、互いに独立な検証のため両方が必要）。
const MAX_CANDIDATE_TEXT_BYTES: usize = 1024 * 1024;

/// 候補テキスト（`RerankCandidate::text`）の合計バイト長上限。`sparse.rs::MAX_CORPUS_BYTES`
/// と同じ理由: [`MAX_CANDIDATE_TEXT_BYTES`]（1 件あたり）・[`MAX_POOL_DEPTH`]（件数）は
/// それぞれ独立な上限のため、両方の上限値ちょうどの入力（1 MiB × 10,000 件 ≈ 10 GiB）を
/// 同時に許すと `tokenize()` の総コストが無制限に近い規模へ増幅しうる（CPU DoS）。
/// `sparse.rs` の値をそのまま踏襲する。
const MAX_TOTAL_CANDIDATE_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// リランク対象の候補 1 件（[`crate::hybrid::HybridHit`] から融合スコアと文書テキストを
/// 添えて渡す入力表現）。`text` はリランカーの素性計算専用であり、SQL・プラン文字列の
/// 組み立てには使わない（coding-rust.md「untrusted 入力の扱い」）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankCandidate<'a> {
    pub id: u64,
    pub fused_score: f64,
    pub text: &'a str,
}

/// 再順位付け後の検索結果 1 件。[`crate::hybrid::HybridHit`] とスコア尺度が異なるため
/// 型を分ける（`hybrid.rs` が `kernel::CandidateHit`/`sparse::ScoredDoc` と型を分けた方針と
/// 同じ）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankedHit {
    pub id: u64,
    pub score: f64,
}

/// リランキング層の設定（本モジュールの既定値。関連: TASK-107）。
///
/// - `pool_depth`: 受理する候補件数の上限（既定 200 = `hybrid::RrfConfig::default()` の
///   `pool_depth` と整合）。
/// - `final_k`: 再順位付け後に返す件数（既定 20）。
///
/// フィールドは非 `pub`（private）とし、[`RerankConfig::new`] による検証済み構築のみを
/// 許可する（`hybrid::RrfConfig` と同じ構築パターン）。構造体リテラルでの直接構築を
/// 許すと検証を迂回でき、`final_k = 0` や `pool_depth` 超過の `final_k` が黙って通る
/// fail-open な経路になりうる（security.md「不安全な設計」）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankConfig {
    pool_depth: usize,
    final_k: usize,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            pool_depth: 200,
            final_k: 20,
        }
    }
}

impl RerankConfig {
    /// 検証付きコンストラクタ。`pool_depth` は `1..=MAX_POOL_DEPTH`、`final_k` は
    /// `1..=pool_depth` であることを構築時に検証し、違反は `Err`（fail-closed）。
    pub fn new(pool_depth: usize, final_k: usize) -> Result<Self, RerankError> {
        if pool_depth == 0 || pool_depth > MAX_POOL_DEPTH {
            return Err(RerankError::InvalidConfig);
        }
        if final_k == 0 || final_k > pool_depth {
            return Err(RerankError::InvalidConfig);
        }
        Ok(Self {
            pool_depth,
            final_k,
        })
    }

    /// 受理する候補件数の上限（検証済み: `1..=MAX_POOL_DEPTH`）。
    pub fn pool_depth(&self) -> usize {
        self.pool_depth
    }

    /// 再順位付け後に返す件数（検証済み: `1..=pool_depth`）。
    pub fn final_k(&self) -> usize {
        self.final_k
    }
}

/// [`rerank_candidates`]・[`Reranker`] 実装が返すエラー。fail-closed（曖昧な場合は
/// 拒否側に倒す。coding-rust.md）。
#[derive(Debug, Clone, PartialEq)]
pub enum RerankError {
    /// [`RerankConfig::new`] の検証違反。
    InvalidConfig,
    /// [`rerank_candidates`] に渡された `final_k` が `0` または `cfg.pool_depth()` 超過。
    InvalidK,
    /// 入力候補（`candidates`）の件数が `cfg.pool_depth()` を超えていた。
    /// `hybrid.rs::HybridError::TooManyCandidates` と同じ理由で、有限性・順序・重複の
    /// 検証（いずれもアロケーションを伴いうる）より先に検証する。
    TooManyCandidates { len: usize, max: usize },
    /// 入力候補に同一 id が複数回出現した（呼び出し元の契約違反）。
    DuplicateId,
    /// 入力候補が融合スコア降順・同点 id 昇順の順位契約に従っていなかった
    /// （[`crate::hybrid::HybridHit`] の順序契約と同じ）。
    UnsortedInput,
    /// 入力候補の `fused_score` が非有限（NaN・Inf）だった。
    NonFiniteScore,
    /// [`Reranker`] 実装（trait object）が、渡した candidates の id 集合に含まれない
    /// id を 1 件でも返した（契約違反）。`hybrid.rs::HybridError::ProviderResultRejected`
    /// と同じ理由で、事後フィルタではなく検索全体を拒否する（部分除外は件数差から
    /// 候補外データの存在情報が漏れる経路になる）。
    ForeignId,
    /// [`Reranker`] 実装が出力内で同一 id を複数回返した。
    DuplicateOutputId,
    /// [`Reranker`] 実装の出力が非有限スコアを含む、またはスコア降順・同点 id 昇順の
    /// 順序契約に違反していた。
    InvalidOutputOrder,
    /// [`Reranker`] 実装の出力件数が要求した `final_k` を超えていた。
    OversizedResult { len: usize, max: usize },
    /// `query_text` のバイト長が [`MAX_QUERY_TEXT_BYTES`] を超える。`reranker.rerank()`
    /// （字句一致系実装は `tokenize()` を呼ぶ）へ渡す前に、[`rerank_candidates`] の
    /// 入口で `query_text.len()`（アロケーション不要）により判定し fail-closed に
    /// 拒否する（`sparse.rs::SparseError::QueryTooLong` と同じ理由）。
    QueryTextTooLong { len: usize, max: usize },
    /// 候補（`RerankCandidate::text`）のバイト長が [`MAX_CANDIDATE_TEXT_BYTES`] を
    /// 超える。`reranker.rerank()` を呼ぶ前に候補の走査（`text.len()`。アロケーション
    /// 不要）で判定し fail-closed に拒否する（`sparse.rs::SparseError::DocTooLong` と
    /// 同じ理由）。
    CandidateTextTooLong { id: u64, len: usize, max: usize },
    /// 候補テキスト（`RerankCandidate::text`）の合計バイト長が
    /// [`MAX_TOTAL_CANDIDATE_TEXT_BYTES`] を超える。`reranker.rerank()`
    /// （`tokenize()` を候補ごとに呼びうる）を呼ぶ前に `checked_add` による累計で
    /// 判定し fail-closed に拒否する（`sparse.rs::SparseError::CorpusTooLarge` と
    /// 同じ理由: 候補ごとの上限だけでは合計コストが無制限に増幅しうる）。
    TotalCandidateTextTooLong { total: usize, max: usize },
}

impl fmt::Display for RerankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RerankError::InvalidConfig => write!(f, "invalid rerank config"),
            RerankError::InvalidK => write!(f, "invalid final_k for reranking"),
            RerankError::TooManyCandidates { len, max } => write!(
                f,
                "rerank candidate list too long: {len} candidates (max {max})"
            ),
            RerankError::DuplicateId => write!(f, "duplicate id in rerank candidate input"),
            RerankError::UnsortedInput => {
                write!(f, "rerank candidate input is not sorted by rank contract")
            }
            RerankError::NonFiniteScore => {
                write!(f, "rerank candidate input contains a non-finite score")
            }
            RerankError::ForeignId => {
                write!(f, "reranker returned an id outside the candidate id set")
            }
            RerankError::DuplicateOutputId => {
                write!(f, "reranker returned a duplicate id in its output")
            }
            RerankError::InvalidOutputOrder => write!(
                f,
                "reranker output violates the score-descending id-ascending order contract"
            ),
            RerankError::OversizedResult { len, max } => {
                write!(f, "reranker returned too many hits: {len} hits (max {max})")
            }
            RerankError::QueryTextTooLong { len, max } => {
                write!(f, "rerank query_text too long: {len} bytes (max {max})")
            }
            RerankError::CandidateTextTooLong { id, len, max } => write!(
                f,
                "rerank candidate text too long: id={id} {len} bytes (max {max})"
            ),
            RerankError::TotalCandidateTextTooLong { total, max } => write!(
                f,
                "rerank candidate text total too long: {total} bytes (max {max})"
            ),
        }
    }
}

impl std::error::Error for RerankError {}

/// リランキング方式を差し替え可能にする trait（object-safe・`&self` のみ。
/// [`crate::kernel::SearchProvider`] と同じ制約）。クロスエンコーダ等の本命方式は
/// この trait を実装して差し替える想定（オーナー判断・依存承認後に追加実装）。
///
/// 実装契約: `candidates` に含まれる id 以外を返してはならず、返す件数は `final_k`
/// 以下、スコアは有限、かつ出力は実装が算出した独自スコアの降順・同点 id 昇順で
/// 返さなければならない（[`rerank_candidates`] が [`RerankError::InvalidOutputOrder`]
/// で検証する。呼び出し元側での再ソートは行わない）。
pub trait Reranker: Send + Sync {
    /// `query_text` と `candidates`（高々 `final_k` 呼び出し元が要求する以上の件数を
    /// 含みうる）から再順位付け後の上位 `final_k` 件を返す。
    fn rerank(
        &self,
        query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError>;
}

/// [`rerank_candidates`] の入力検証ヘルパ。融合スコア降順・同点 id 昇順の順位契約
/// （`hybrid.rs::is_sorted_desc_id_asc` と同じ判定ロジック）に従っているかを判定する。
fn is_sorted_desc_id_asc(items: impl Iterator<Item = (f64, u64)>) -> bool {
    let mut prev: Option<(f64, u64)> = None;
    for (score, id) in items {
        if let Some((prev_score, prev_id)) = prev {
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

/// `query_text`・`candidates` のバイト長検証ヘルパ（[`MAX_QUERY_TEXT_BYTES`]・
/// [`MAX_CANDIDATE_TEXT_BYTES`]・[`MAX_TOTAL_CANDIDATE_TEXT_BYTES`]）。[`rerank_candidates`]
/// だけでなく [`LexicalOverlapReranker::rerank`] からも直接呼ぶ（公開 trait
/// [`Reranker`] は `rerank_candidates` を経由せず直接呼び出すことも型上できてしまうため、
/// `rerank_candidates` の入口検証だけでは trait 実装を直接呼ぶ経路で未検証の巨大な
/// テキストがそのまま `tokenize()` へ渡りうる。`tokenize()` を呼ぶ実装自身がこの
/// 検証を内部で行うことで、迂回できない構造にする）。件数は呼び出し元
/// （[`rerank_candidates`] の `TooManyCandidates` 検証、または [`MAX_POOL_DEPTH`] 自体）で
/// 別途上限があるため、ここでの走査コスト自体は有界。
fn validate_text_lengths(
    query_text: &str,
    candidates: &[RerankCandidate<'_>],
) -> Result<(), RerankError> {
    if query_text.len() > MAX_QUERY_TEXT_BYTES {
        return Err(RerankError::QueryTextTooLong {
            len: query_text.len(),
            max: MAX_QUERY_TEXT_BYTES,
        });
    }

    if let Some(c) = candidates
        .iter()
        .find(|c| c.text.len() > MAX_CANDIDATE_TEXT_BYTES)
    {
        return Err(RerankError::CandidateTextTooLong {
            id: c.id,
            len: c.text.len(),
            max: MAX_CANDIDATE_TEXT_BYTES,
        });
    }

    // 候補ごとの上限だけでは合計コストが無制限に増幅しうるため（[`MAX_TOTAL_CANDIDATE_TEXT_BYTES`]
    // のコメント参照）、`checked_add` で合計を求めて検証する。`usize` オーバーフローは
    // 未定義動作にせず、オーバーフローした時点で fail-closed に拒否する
    // （coding-rust.md「整数演算は checked_* / saturating_* を使う」）。
    let mut total: usize = 0;
    for c in candidates {
        total = total
            .checked_add(c.text.len())
            .ok_or(RerankError::TotalCandidateTextTooLong {
                total: usize::MAX,
                max: MAX_TOTAL_CANDIDATE_TEXT_BYTES,
            })?;
        if total > MAX_TOTAL_CANDIDATE_TEXT_BYTES {
            return Err(RerankError::TotalCandidateTextTooLong {
                total,
                max: MAX_TOTAL_CANDIDATE_TEXT_BYTES,
            });
        }
    }

    Ok(())
}

/// [`rerank_candidates`] の入力検証ヘルパ。`ids` の列に同一 id が複数回出現するかを
/// 判定する（`hybrid.rs::has_duplicate_id` と同じロジック）。
fn has_duplicate_id(ids: impl Iterator<Item = u64>) -> bool {
    let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return true;
        }
    }
    false
}

/// リランカーを呼び出し候補を再順位付けする入口（`hybrid_search` の出力を受け取り
/// `core.rs` 相当の呼び出し元へ最終上位を返す統合点。TASK-107）。
///
/// 入力検証（`reranker` 呼び出し前）: 候補件数 ≤ `cfg.pool_depth()`
/// （[`RerankError::TooManyCandidates`]）・`query_text` のバイト長 ≤
/// [`MAX_QUERY_TEXT_BYTES`]（[`RerankError::QueryTextTooLong`]）・候補ごとのテキストの
/// バイト長 ≤ [`MAX_CANDIDATE_TEXT_BYTES`]（[`RerankError::CandidateTextTooLong`]）・
/// 候補テキスト合計のバイト長 ≤ [`MAX_TOTAL_CANDIDATE_TEXT_BYTES`]
/// （[`RerankError::TotalCandidateTextTooLong`]）・スコア有限性
/// （[`RerankError::NonFiniteScore`]）・融合スコア降順同点 id 昇順
/// （[`RerankError::UnsortedInput`]）・id 重複なし（[`RerankError::DuplicateId`]）の順に
/// 検証する（`hybrid.rs::rrf_fuse` と同じ検証順序: 長さ→有限性→順序→重複。長さを最初に
/// 検証してから初めて残りの走査を許すのは coding-rust.md「長さフィールドは上限検証して
/// からアロケーションに使う」に従う。`query_text`・候補テキストの長さ検証は
/// [`validate_text_lengths`] を通じて `reranker.rerank()`（[`LexicalOverlapReranker`] 等の
/// 実装が `tokenize()` へ渡す）を呼ぶ前に行い、`sparse.rs::MAX_QUERY_BYTES`・
/// `MAX_DOC_BYTES`・`MAX_CORPUS_BYTES` と同じ理由で無制限なアロケーション・走査コストの
/// 増幅を防ぐ。[`Reranker`] は公開 trait のためこの `rerank_candidates` を経由しない
/// 直接呼び出しも型上ありうる。そのため同じ検証を [`LexicalOverlapReranker::rerank`]
/// 自身も内部で行い、直接呼び出しでも迂回できない構造にしている）。
///
/// `final_k` は `1..=cfg.pool_depth()` を検証し、`0` または超過は [`RerankError::InvalidK`]
/// （`hybrid.rs::hybrid_search` が `k` を `cfg.pool_depth()` 基準で検証するのと同じ理由:
/// `final_k` が `cfg.pool_depth()` を超えると候補プール自体がその件数を満たせないまま
/// 静かに縮退しうるため）。
///
/// 出力検証（`reranker` 呼び出し後）: `reranker.rerank()` は trait object であり
/// 「候補 id 集合の部分集合のみを返す」ことは型で強制されない別個のオブジェクトのため、
/// 出力の id が入力候補の id 集合に含まれない場合は 1 件でも
/// [`RerankError::ForeignId`] で拒否する（`hybrid.rs` の provider 結果検証と同じ
/// fail-closed の方向。事後フィルタは件数差から候補外データの存在情報が漏れる経路に
/// なるため使わない）。あわせて出力内の id 重複（[`RerankError::DuplicateOutputId`]）・
/// スコア非有限または順序契約違反（[`RerankError::InvalidOutputOrder`]）・件数が
/// `final_k` を超過（[`RerankError::OversizedResult`]）をそれぞれ検証する。
pub fn rerank_candidates(
    reranker: &dyn Reranker,
    query_text: &str,
    candidates: &[RerankCandidate<'_>],
    cfg: &RerankConfig,
) -> Result<Vec<RerankedHit>, RerankError> {
    if cfg.final_k() == 0 || cfg.final_k() > cfg.pool_depth() {
        return Err(RerankError::InvalidK);
    }

    // 長さ検証を他のどの検証よりも先に行う（[`RerankError::TooManyCandidates`] の
    // ドキュメント参照）。以降の検証（有限性・ソート順・重複）はいずれも入力を線形
    // 走査するため、長さを検証せずに通すと契約違反の呼び出し元が `cfg.pool_depth()`
    // を大きく超える件数を渡した場合に無制限な走査コストを払うことになる。
    if candidates.len() > cfg.pool_depth() {
        return Err(RerankError::TooManyCandidates {
            len: candidates.len(),
            max: cfg.pool_depth(),
        });
    }

    // `query_text`・候補ごとのテキスト・候補テキスト合計のバイト長を `reranker.rerank()`
    // を呼ぶ（実装が `tokenize()` へ渡し `String`・`BTreeSet` を確保しうる）前に検証する
    // （[`validate_text_lengths`] のドキュメント参照。候補件数は直前の
    // `TooManyCandidates` 検証により `cfg.pool_depth()`（`MAX_POOL_DEPTH` 以下）に
    // 収まっているため、この走査コスト自体は有界）。
    validate_text_lengths(query_text, candidates)?;

    // スコアの有限性を順序検証より先に確認する（`hybrid.rs::rrf_fuse` と同じ理由:
    // `f64::total_cmp` は NaN にもビットパターン依存の全順序を与えるため、有限性を
    // 確認しないまま順序検証だけに頼ると NaN が偶然順序契約を満たすビットパターンで
    // 紛れ込んだ場合に検出できない）。
    if candidates.iter().any(|c| !c.fused_score.is_finite()) {
        return Err(RerankError::NonFiniteScore);
    }

    if !is_sorted_desc_id_asc(candidates.iter().map(|c| (c.fused_score, c.id))) {
        return Err(RerankError::UnsortedInput);
    }

    if has_duplicate_id(candidates.iter().map(|c| c.id)) {
        return Err(RerankError::DuplicateId);
    }

    let visible_ids: std::collections::BTreeSet<u64> = candidates.iter().map(|c| c.id).collect();

    let hits = reranker.rerank(query_text, candidates, cfg.final_k())?;

    // 長さ検証を可視性走査より先に行う（`hybrid.rs::hybrid_search` と同じ理由: 契約
    // 違反の reranker が `final_k` を大きく超える件数を返した場合に、直後の走査で
    // 不要な O(n) コストを払わずに済む）。
    if hits.len() > cfg.final_k() {
        return Err(RerankError::OversizedResult {
            len: hits.len(),
            max: cfg.final_k(),
        });
    }

    if hits.iter().any(|h| !h.score.is_finite()) {
        return Err(RerankError::InvalidOutputOrder);
    }
    if !is_sorted_desc_id_asc(hits.iter().map(|h| (h.score, h.id))) {
        return Err(RerankError::InvalidOutputOrder);
    }
    if has_duplicate_id(hits.iter().map(|h| h.id)) {
        return Err(RerankError::DuplicateOutputId);
    }
    // 事後フィルタ（候補外 id だけを黙って除外する）はしない: 候補外 id が
    // `final_k` の枠を占有していた場合、フィルタ後に正しい候補ヒットを復元できず、
    // 結果件数の差から候補外データの存在情報が外部へ漏れうる（`hybrid.rs` の
    // `ProviderResultRejected` と同じ理由）。1 件でも候補外 id が含まれていたら
    // 検索全体を拒否する（fail-closed）。
    if hits.iter().any(|h| !visible_ids.contains(&h.id)) {
        return Err(RerankError::ForeignId);
    }

    Ok(hits)
}

/// ベースラインの参照実装: 候補の入力順序（融合スコア降順）をそのまま保持して
/// `final_k` 件へ切り詰めるだけの恒等リランカー。TASK-108（Issue #39）における
/// 「リランキングなし構成」との前後比較の基準実装を兼ねる。
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityReranker;

impl Reranker for IdentityReranker {
    fn rerank(
        &self,
        _query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        Ok(candidates
            .iter()
            .take(final_k)
            .map(|c| RerankedHit {
                id: c.id,
                score: c.fused_score,
            })
            .collect())
    }
}

/// RRF の順位ベース融合（`weight / (k_const + rank)`）と同じ形式で、字句一致順位
/// （`query_text` と `candidate.text` のトークン重なり件数の多い順）を融合スコアの
/// 順位と再結合する参照実装。方式確定までの暫定実装（クロスエンコーダ等の本命方式は
/// オーナー判断・依存承認後に [`Reranker`] の別実装として追加する想定。SEARCH-8 の
/// 効果測定は TASK-108 = Issue #39 の管轄）。
///
/// 字句一致件数そのもの（尺度が候補ごとに不安定なスコア）ではなく順位のみを使う点は
/// `hybrid.rs` が RRF で採った設計判断（元スコアではなく順位ベースで統合する）を
/// 踏襲する。字句トークン化は [`crate::sparse::tokenize`]（TASK-102 実装済みの簡易
/// トークナイザ）を再利用する。
///
/// 既定重み（[`LexicalOverlapReranker::default`]）は `fused_weight:lexical_weight
/// = 3.0:1.0`（fused 優位。Issue #310 対応で変更）。大規模段実測（`crates/engine/
/// tests/rerank_recall.rs`）で `after_hits20`（388）≥ `baseline_hits20`（387）の
/// 非劣化を満たす。詳細は `docs/design/rerank-recall-regression.md` 参照。
#[derive(Debug, Clone, Copy)]
pub struct LexicalOverlapReranker {
    /// RRF 型融合のランク減衰定数（`hybrid::RrfConfig::k_const` と同じ役割）。
    k_const: f64,
    /// 融合スコアの順位（rank_by_fused_score）に対する重み。
    fused_weight: f64,
    /// 字句一致順位（rank_by_lexical_overlap）に対する重み。
    lexical_weight: f64,
}

impl Default for LexicalOverlapReranker {
    fn default() -> Self {
        // 既定重み 3.0:1.0（fused 優位）の採用根拠は本 struct のドキュメンテー
        // ションコメント（Issue #310）を参照。
        Self {
            k_const: 60.0,
            fused_weight: 3.0,
            lexical_weight: 1.0,
        }
    }
}

impl LexicalOverlapReranker {
    /// 検証付きコンストラクタ。`k_const`・重みが有限かつ正であることを検証する
    /// （`hybrid::RrfConfig::new` と同じ検証方針）。
    pub fn new(k_const: f64, fused_weight: f64, lexical_weight: f64) -> Result<Self, RerankError> {
        if !k_const.is_finite() || k_const <= 0.0 {
            return Err(RerankError::InvalidConfig);
        }
        if !fused_weight.is_finite() || fused_weight <= 0.0 {
            return Err(RerankError::InvalidConfig);
        }
        if !lexical_weight.is_finite() || lexical_weight <= 0.0 {
            return Err(RerankError::InvalidConfig);
        }
        Ok(Self {
            k_const,
            fused_weight,
            lexical_weight,
        })
    }
}

impl Reranker for LexicalOverlapReranker {
    fn rerank(
        &self,
        query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        // [`Reranker`] は公開 trait のため、[`rerank_candidates`] を経由せずこの
        // `rerank()` を直接呼び出す経路が型上ありうる（`rerank_candidates` の入口検証は
        // 迂回可能）。空文字列・短文の候補を大量に渡すとバイト長検証だけでは
        // 上限を超えないまま件数だけが無制限に増幅しうるため、`tokenize()`
        // （`String`・`BTreeSet` を確保する）や `Vec` 確保（`overlap_ranked`）より
        // 前に、この実装自身でも件数を [`MAX_POOL_DEPTH`] で検証する
        // （`rerank_candidates` の `TooManyCandidates` 検証と同じ契約。直接呼び出し
        // 経路では `cfg.pool_depth()` を持たないため [`MAX_POOL_DEPTH`] 自体を上限に
        // 使う）。
        if candidates.len() > MAX_POOL_DEPTH {
            return Err(RerankError::TooManyCandidates {
                len: candidates.len(),
                max: MAX_POOL_DEPTH,
            });
        }

        // この実装自身でも [`validate_text_lengths`] により長さを検証し、未検証の
        // 巨大な `query_text`・候補テキストがそのまま `tokenize()` へ渡る経路を構造的に
        // 防ぐ（`rerank_candidates` 経由の呼び出しでは二重検証になるが、コストは
        // 候補件数に対して線形かつ [`MAX_POOL_DEPTH`] で有界なため許容する）。
        validate_text_lengths(query_text, candidates)?;

        // クエリ側トークンは候補ごとの重なり計算で使い回すため一度だけ計算する。
        let query_tokens: std::collections::BTreeSet<String> =
            tokenize(query_text).into_iter().collect();

        // `rank_fused`（1-based）は候補配列の位置ではなく、`hybrid.rs::TieRank::
        // GroupEnd`（既定の同点順位規約）と同じグループ末尾順位で求める
        // （Issue #320 codex-review P1 指摘対応）。`candidates` は `fused_score` の
        // 同点集合を持ちうる（`hybrid.rs` の融合結果由来のため。同点の連続区間は
        // `rerank_candidates` が検証済みのソート順契約により必ず連続する）が、
        // 位置順位（`idx + 1`）のまま扱うと同点グループ内で融合側と異なる順位規約に
        // なり、`hybrid.rs` 側の GroupEnd 化（Issue #310）の効果がこの層で部分的に
        // 相殺される。グループ内の全メンバーへグループ末尾の 1-based 位置を割り当てる
        // （`hybrid.rs::accumulate_ranked` の `TieRank::GroupEnd` 分岐と同じ走査）。
        let mut rank_fused_by_idx = vec![0usize; candidates.len()];
        let mut group_idx = 0usize;
        while group_idx < candidates.len() {
            let group_score = candidates[group_idx].fused_score;
            let mut group_end = group_idx + 1;
            while group_end < candidates.len()
                && candidates[group_end].fused_score.total_cmp(&group_score)
                    == std::cmp::Ordering::Equal
            {
                group_end += 1;
            }
            for slot in rank_fused_by_idx.iter_mut().take(group_end).skip(group_idx) {
                *slot = group_end;
            }
            group_idx = group_end;
        }

        // (id, 融合スコア順位 rank_fused, 字句重なり件数) を候補順（＝融合スコア
        // 降順。`rerank_candidates` が事前検証済み）に集める。
        // `idx` は候補配列の長さ（`rerank_candidates` により高々 `pool_depth`
        // （`MAX_POOL_DEPTH` 以下）に上限がある）に収まるため `as f64` 変換は
        // 精度を失わない。
        let mut overlap_ranked: Vec<(u64, usize, usize)> = candidates
            .iter()
            .enumerate()
            .map(|(idx, c)| {
                let rank_fused = rank_fused_by_idx[idx];
                let doc_tokens: std::collections::BTreeSet<String> =
                    tokenize(c.text).into_iter().collect();
                let overlap = query_tokens.intersection(&doc_tokens).count();
                (c.id, rank_fused, overlap)
            })
            .collect();

        // 字句重なり件数の多い順（同点は候補順＝融合スコア降順を保つ安定ソート）で
        // 並べ替える。並べ替え後の位置（enumerate の rank_idx）がそのまま字句一致順位
        // rank_lexical（1-based）になるため、BTreeMap 経由の再引きは不要（id 集合の
        // 不一致による `expect` 到達不能パスをそもそも作らない）。
        overlap_ranked.sort_by_key(|entry| std::cmp::Reverse(entry.2));

        let mut scores: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
        for (rank_idx, (id, rank_fused, _)) in overlap_ranked.iter().enumerate() {
            let rank_lexical = rank_idx.saturating_add(1);
            let contribution_fused = self.fused_weight / (self.k_const + *rank_fused as f64);
            let contribution_lexical = self.lexical_weight / (self.k_const + rank_lexical as f64);
            let entry = scores.entry(*id).or_insert(0.0);
            *entry += contribution_fused + contribution_lexical;
        }

        if scores.values().any(|score| !score.is_finite()) {
            return Err(RerankError::NonFiniteScore);
        }

        let mut out: Vec<RerankedHit> = scores
            .into_iter()
            .map(|(id, score)| RerankedHit { id, score })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        out.truncate(final_k);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand<'a>(id: u64, fused_score: f64, text: &'a str) -> RerankCandidate<'a> {
        RerankCandidate {
            id,
            fused_score,
            text,
        }
    }

    #[test]
    fn rerank_config_default_matches_expected_values() {
        let cfg = RerankConfig::default();
        assert_eq!(cfg.pool_depth(), 200);
        assert_eq!(cfg.final_k(), 20);
    }

    #[test]
    fn rerank_config_rejects_invalid_values() {
        assert_eq!(
            RerankConfig::new(0, 1).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            RerankConfig::new(MAX_POOL_DEPTH + 1, 1).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            RerankConfig::new(10, 0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            RerankConfig::new(10, 11).unwrap_err(),
            RerankError::InvalidConfig
        );
    }

    #[test]
    fn lexical_overlap_reranker_rejects_invalid_values() {
        assert_eq!(
            LexicalOverlapReranker::new(0.0, 1.0, 1.0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            LexicalOverlapReranker::new(f64::NAN, 1.0, 1.0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            LexicalOverlapReranker::new(60.0, -1.0, 1.0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            LexicalOverlapReranker::new(60.0, 1.0, f64::INFINITY).unwrap_err(),
            RerankError::InvalidConfig
        );
    }

    #[test]
    fn identity_reranker_preserves_order_and_truncates() {
        let cfg = RerankConfig::new(10, 2).unwrap();
        let candidates = [cand(1, 3.0, "a"), cand(2, 2.0, "b"), cand(3, 1.0, "c")];
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        assert_eq!(
            hits,
            vec![
                RerankedHit { id: 1, score: 3.0 },
                RerankedHit { id: 2, score: 2.0 },
            ]
        );
    }

    #[test]
    fn lexical_overlap_reranker_surfaces_matching_document_to_top() {
        // 融合スコアでは 1 位の候補（id=1）を、クエリと字句一致する文書（id=2）が
        // 最終的に上回ることを確認する（SEARCH-7: 再順位付けの動作）。既定重み
        // （[`LexicalOverlapReranker::default`]。Issue #310 対応で fused 優位
        // 3.0:1.0 へ変更済み）でも融合順位 1 位の優位が大きく拮抗しうるため、
        // 字句一致信号を優勢にする重み構成（`lexical_weight` を大きく取る）で
        // 検証する。
        let cfg = RerankConfig::new(10, 3).unwrap();
        let candidates = [
            cand(1, 3.0, "unrelated content about nothing"),
            cand(2, 2.0, "vector search kernel reranking"),
            cand(3, 1.0, "also unrelated filler text"),
        ];
        let reranker = LexicalOverlapReranker::new(60.0, 1.0, 5.0).unwrap();
        let hits =
            rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
        assert_eq!(hits[0].id, 2, "lexically matching doc must surface to top");
    }

    #[test]
    fn lexical_overlap_reranker_is_deterministic() {
        let cfg = RerankConfig::new(10, 3).unwrap();
        let candidates = [
            cand(1, 3.0, "vector search kernel"),
            cand(2, 2.0, "vector search kernel"),
            cand(3, 1.0, "unrelated"),
        ];
        let reranker = LexicalOverlapReranker::default();
        let hits_a =
            rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
        let hits_b =
            rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
        assert_eq!(hits_a, hits_b);
    }

    #[test]
    fn lexical_overlap_reranker_tie_breaks_by_id_ascending() {
        // 真の同点スコアを作るため、rank_fused と rank_lexical が入れ替わる構成にする。
        // 既定（[`LexicalOverlapReranker::default`]。Issue #310 対応で fused 優位
        // 3.0:1.0）ではこの入れ替えだけで真の同点は作れないため、本テストの関心
        // （タイブレーク分岐そのもの）に絞って等重み（fused_weight = lexical_weight
        // = 1.0）を明示的に構築する。
        // id=5: rank_fused=1（融合スコア降順で先頭）・rank_lexical=2（字句重なり少）
        // id=6: rank_fused=2                       ・rank_lexical=1（字句重なり多）
        // スコア = weight/(k+rank_fused) + weight/(k+rank_lexical) は rank の組が
        // 入れ替わっているだけなので id=5・id=6 で厳密に一致し、同点タイブレーク
        // （id 昇順）分岐（本ファイル `LexicalOverlapReranker::rerank` の
        // `.then(a.id.cmp(&b.id))`）を実際に通過する。
        let cfg = RerankConfig::new(10, 2).unwrap();
        let candidates = [cand(5, 3.0, "alpha"), cand(6, 2.0, "alpha bravo")];
        let reranker = LexicalOverlapReranker::new(60.0, 1.0, 1.0).unwrap();
        let hits = rerank_candidates(&reranker, "alpha bravo", &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].score, hits[1].score, "must be a true score tie");
        assert_eq!(hits[0].id, 5, "tie must break by ascending id");
        assert_eq!(hits[1].id, 6);
    }

    #[test]
    fn rerank_candidates_rejects_k_zero_and_over_pool_depth() {
        let cfg_zero = RerankConfig::new(10, 1).unwrap();
        // final_k=0 は RerankConfig::new 自体が拒否するため、InvalidK の経路は
        // final_k > pool_depth のケースで確認する（RerankConfig::new は
        // 1..=pool_depth を検証するため、直接それを超える cfg は構築できない。
        // よって InvalidK は rerank_candidates 内の防御的二重検証として存在し、
        // ここでは RerankConfig::new 自体の拒否を確認する）。
        assert_eq!(
            RerankConfig::new(1, 0).unwrap_err(),
            RerankError::InvalidConfig
        );
        let candidates = [cand(1, 1.0, "a")];
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg_zero).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_rejects_input_exceeding_pool_depth() {
        let cfg = RerankConfig::new(1, 1).unwrap();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::TooManyCandidates { len: 2, max: 1 });
    }

    #[test]
    fn rerank_candidates_rejects_query_text_exceeding_max_bytes() {
        let cfg = RerankConfig::default();
        let oversized_query = "a".repeat(MAX_QUERY_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, "a")];
        let err =
            rerank_candidates(&IdentityReranker, &oversized_query, &candidates, &cfg).unwrap_err();
        assert_eq!(
            err,
            RerankError::QueryTextTooLong {
                len: MAX_QUERY_TEXT_BYTES + 1,
                max: MAX_QUERY_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn rerank_candidates_accepts_query_text_at_max_bytes_boundary() {
        let cfg = RerankConfig::default();
        let boundary_query = "a".repeat(MAX_QUERY_TEXT_BYTES);
        let candidates = [cand(1, 1.0, "a")];
        let hits =
            rerank_candidates(&IdentityReranker, &boundary_query, &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_rejects_candidate_text_exceeding_max_bytes() {
        let cfg = RerankConfig::default();
        let oversized_text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, oversized_text.as_str())];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(
            err,
            RerankError::CandidateTextTooLong {
                id: 1,
                len: MAX_CANDIDATE_TEXT_BYTES + 1,
                max: MAX_CANDIDATE_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn rerank_candidates_accepts_candidate_text_at_max_bytes_boundary() {
        let cfg = RerankConfig::default();
        let boundary_text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES);
        let candidates = [cand(1, 1.0, boundary_text.as_str())];
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_accepts_total_candidate_text_at_max_bytes_boundary() {
        // MAX_CANDIDATE_TEXT_BYTES 上限の候補を MAX_TOTAL_CANDIDATE_TEXT_BYTES ちょうどに
        // なる件数だけ並べても受理される（境界値。整数演算は各要素が上限一杯でも
        // オーバーフローしないことも合わせて確認する）。
        let doc_count = MAX_TOTAL_CANDIDATE_TEXT_BYTES / MAX_CANDIDATE_TEXT_BYTES;
        let cfg = RerankConfig::new(doc_count, 1).unwrap();
        let text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES);
        let candidates: Vec<RerankCandidate<'_>> = (0..doc_count as u64)
            .map(|i| cand(i, (doc_count - i as usize) as f64, text.as_str()))
            .collect();
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_rejects_total_candidate_text_exceeding_max_bytes() {
        // 候補 1 件あたりは MAX_CANDIDATE_TEXT_BYTES 以内でも、合計が
        // MAX_TOTAL_CANDIDATE_TEXT_BYTES を超えれば拒否する（sparse.rs::CorpusTooLarge
        // 相当。1 件あたりの上限のみでは合計コストが無制限に増幅しうるため）。
        let doc_count = MAX_TOTAL_CANDIDATE_TEXT_BYTES / MAX_CANDIDATE_TEXT_BYTES;
        let cfg = RerankConfig::new(doc_count + 1, 1).unwrap();
        let text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES);
        let mut candidates: Vec<RerankCandidate<'_>> = (0..doc_count as u64)
            .map(|i| cand(i, (doc_count - i as usize) as f64, text.as_str()))
            .collect();
        // 合計をちょうど MAX_TOTAL_CANDIDATE_TEXT_BYTES + 1 バイトにする 1 バイト候補を
        // 融合スコア最小として追加する。
        candidates.push(cand(doc_count as u64, -1.0, "a"));
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(
            err,
            RerankError::TotalCandidateTextTooLong {
                total: MAX_TOTAL_CANDIDATE_TEXT_BYTES + 1,
                max: MAX_TOTAL_CANDIDATE_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_oversized_query_text() {
        // codex-review P1 指摘対応: 公開 trait Reranker::rerank を rerank_candidates を
        // 経由せず直接呼び出しても、実装内部の validate_text_lengths 呼び出しにより
        // MAX_QUERY_TEXT_BYTES 検証が迂回されないことを確認する。
        let reranker = LexicalOverlapReranker::default();
        let oversized_query = "a".repeat(MAX_QUERY_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, "a")];
        let err = reranker
            .rerank(&oversized_query, &candidates, 1)
            .unwrap_err();
        assert_eq!(
            err,
            RerankError::QueryTextTooLong {
                len: MAX_QUERY_TEXT_BYTES + 1,
                max: MAX_QUERY_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_oversized_candidate_text() {
        // 同上（候補テキスト側）。tokenize() が呼ばれる前に拒否されることを確認する。
        let reranker = LexicalOverlapReranker::default();
        let oversized_text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, oversized_text.as_str())];
        let err = reranker.rerank("q", &candidates, 1).unwrap_err();
        assert_eq!(
            err,
            RerankError::CandidateTextTooLong {
                id: 1,
                len: MAX_CANDIDATE_TEXT_BYTES + 1,
                max: MAX_CANDIDATE_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_candidate_count_exceeding_max_pool_depth() {
        // codex-review P1 / Bugbot Medium 指摘対応: 公開 trait Reranker::rerank を
        // rerank_candidates を経由せず直接呼び出した場合、合計バイト長が
        // MAX_TOTAL_CANDIDATE_TEXT_BYTES を超えない短文候補を MAX_POOL_DEPTH 件超
        // 渡しても、tokenize()・Vec 確保（overlap_ranked）より前に件数超過として
        // 拒否されることを確認する。
        let reranker = LexicalOverlapReranker::default();
        let text = "a";
        let candidates: Vec<RerankCandidate<'_>> = (0..(MAX_POOL_DEPTH as u64 + 1))
            .map(|i| cand(i, -(i as f64), text))
            .collect();
        let err = reranker.rerank("q", &candidates, 1).unwrap_err();
        assert_eq!(
            err,
            RerankError::TooManyCandidates {
                len: MAX_POOL_DEPTH + 1,
                max: MAX_POOL_DEPTH,
            }
        );
    }

    #[test]
    fn rerank_candidates_rejects_non_finite_score() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, f64::NAN, "a")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::NonFiniteScore);
    }

    #[test]
    fn rerank_candidates_rejects_unsorted_input() {
        let cfg = RerankConfig::default();
        // スコア昇順（本来は降順であるべき契約に違反）。
        let candidates = [cand(1, 1.0, "a"), cand(2, 2.0, "b")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::UnsortedInput);
    }

    #[test]
    fn rerank_candidates_rejects_duplicate_input_id() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 2.0, "a"), cand(1, 1.0, "b")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::DuplicateId);
    }

    /// [`Reranker`] の契約違反（候補外 id を返す実装バグ）を模したモック。
    struct LeakyReranker;
    impl Reranker for LeakyReranker {
        fn rerank(
            &self,
            _query_text: &str,
            _candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            Ok(vec![RerankedHit {
                id: 999,
                score: 1.0,
            }])
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_returning_foreign_id() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 1.0, "a")];
        let err = rerank_candidates(&LeakyReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::ForeignId);
    }

    /// [`Reranker`] の契約違反（出力内で id を重複させる実装バグ）を模したモック。
    struct DuplicatingReranker;
    impl Reranker for DuplicatingReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            let id = candidates.first().map(|c| c.id).unwrap_or(0);
            Ok(vec![
                RerankedHit { id, score: 2.0 },
                RerankedHit { id, score: 1.0 },
            ])
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_returning_duplicate_output_id() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 1.0, "a")];
        let err = rerank_candidates(&DuplicatingReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::DuplicateOutputId);
    }

    /// [`Reranker`] の契約違反（要求した final_k を超える件数を返す実装バグ）を
    /// 模したモック。
    struct OverflowingReranker;
    impl Reranker for OverflowingReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            Ok(candidates
                .iter()
                .map(|c| RerankedHit {
                    id: c.id,
                    score: c.fused_score,
                })
                .collect())
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_exceeding_final_k() {
        let cfg = RerankConfig::new(10, 1).unwrap();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let err = rerank_candidates(&OverflowingReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::OversizedResult { len: 2, max: 1 });
    }

    /// [`Reranker`] の契約違反（非有限スコアを返す実装バグ）を模したモック。
    struct NonFiniteOutputReranker;
    impl Reranker for NonFiniteOutputReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            let id = candidates.first().map(|c| c.id).unwrap_or(0);
            Ok(vec![RerankedHit {
                id,
                score: f64::NAN,
            }])
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_non_finite_output_score() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 1.0, "a")];
        let err = rerank_candidates(&NonFiniteOutputReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::InvalidOutputOrder);
    }

    /// [`Reranker`] の契約違反（出力の順序契約に違反する実装バグ）を模したモック。
    struct UnsortedOutputReranker;
    impl Reranker for UnsortedOutputReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            // 候補は融合スコア降順で渡ってくるが、ここではあえてスコア昇順で返す。
            Ok(candidates
                .iter()
                .rev()
                .map(|c| RerankedHit {
                    id: c.id,
                    score: c.fused_score,
                })
                .collect())
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_unsorted_output() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let err = rerank_candidates(&UnsortedOutputReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::InvalidOutputOrder);
    }

    #[test]
    fn rerank_candidates_output_is_subset_of_input_pool() {
        // SEARCH-6 対応: リランカーはプールを拡張しない（出力 ⊆ 入力）ことの単体
        // レベルでの固定。
        let cfg = RerankConfig::new(200, 20).unwrap();
        assert_eq!(cfg.pool_depth(), 200);
        let candidates: Vec<RerankCandidate<'_>> =
            (0..5).map(|i| cand(i, (5 - i) as f64, "x")).collect();
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        let input_ids: std::collections::BTreeSet<u64> = candidates.iter().map(|c| c.id).collect();
        assert!(hits.iter().all(|h| input_ids.contains(&h.id)));
    }
}
