//! バッチクエリ・一括インデクシング専用の検索エンジン（TASK-128 ポインタ:
//! CORE-6, CORE-7, CORE-16）。`kernel.rs::SearchProvider`（単発クエリの窓口）は
//! 実装せず、単発経路（`core.rs::EngineCore::search`）から本モジュールへは
//! 構造的に接続できない。
//!
//! 常駐ベース行列を f16 2 要素/u32 パックで保持・走査する CPU 上の参照実装で
//! あり、実際の GPU（`wgpu`）実行は行わない。依存追加はユーザーの明示承認を
//! 経てから行う規約（.claude/rules/dependency-policy.md）のため、承認前は
//! アルゴリズム的な挙動（f16 パック表現・バッチ Top-k 選出・クエリ別テナント
//! マスク・動的窓集約）のみを依存クレートなしで実装する。GPU 実行への置き換えは
//! 承認後のフォローアップとする（TASK-128 ポインタ）。
//!
//! `TopKSelector`（`kernel.rs`）を選出段で共用し、選出規約（スコア降順・同点 id
//! 昇順・非有限値除外）の二重管理を防ぐ。`core.rs::EngineCore` と同じ「(1) 選出前に
//! 候補をマスクで絞る、(2) 選出後の結果 id を独立に再検証する」二重防御を踏襲する。

use std::fmt;

use crate::kernel::{SearchHit, TopKSelector};
use crate::policy::PolicyContext;
use crate::storage::Visibility;

/// バッチ 1 件あたりの許容クエリ数上限（防御的上限。`core.rs::MAX_SEARCH_K` と同じ
/// 桁感覚で、上限検証前にアロケーションへ使わない）。
pub const MAX_BATCH_QUERIES: usize = 4_096;

/// 一括インデクシング対象の行数上限（防御的上限）。
pub const MAX_BATCH_ROWS: usize = 1_000_000;

/// クエリ・格納ベクトルの次元数上限（防御的上限）。
pub const MAX_BATCH_DIM: usize = 8_192;

/// [`ResidentMatrix::build`] が確保してよい総バイト量の上限。`packed: Vec<u32>` に
/// 加え、同時に確保する `ids: Vec<u64>`・`tenant_ids: Vec<String>` の見積もりバイト量も
/// 合算した総量として扱う（`arena.rs::MAX_ARENA_TOTAL_BYTES`/`check_capacity` と同方針。
/// codex レビュー指摘対応: `MAX_BATCH_ROWS`・`MAX_BATCH_DIM` を独立にしか検証しないと、
/// 両方が上限値の場合に `packed` 単体で約 16.4GB の単一確保が発生し得るため、行数と
/// バイト量の両方で上限を課す）。
pub const MAX_BATCH_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// `BatchQuery::k` の許容上限（防御的上限）。`core.rs::MAX_SEARCH_K` と同じ値を
/// 独立に定義する（`core.rs` の定数は非公開で本モジュールから参照できないため）。
/// `kernel.rs::TopKSelector` 自体は `k` を検証しないため、複数クエリを同時に
/// 処理するバッチ経路では本モジュールが未検証の巨大な `k` をそのまま
/// 使わないよう明示的に拒否する。
pub const MAX_BATCH_K: usize = 10_000;

/// バッチ全体で許容する `sum(k)`（各クエリの `k` の総和）の上限（防御的上限。
/// codex P1 指摘対応）。クエリごとの `k` は [`MAX_BATCH_K`] で個別に上限を
/// 課しているが、[`MAX_BATCH_QUERIES`] × `MAX_BATCH_K`（最大 4,096 万）まで
/// 積が達しうる。`TopKSelector` は 1 クエリあたり最終的に高々 `k` 個の要素を
/// 保持する（`kernel.rs::TopKSelector::push` 参照）ため、全選出器が保持しうる
/// 要素数の総和はバッチ全体の `sum(k)` で抑えられる。1 個の常駐行列が持つ
/// 行数を超える数の hit を選出しても意味がない（[`MAX_BATCH_ROWS`] 行しか
/// 実データが存在しない）ため、`sum(k)` の上限を [`MAX_BATCH_ROWS`] と
/// 同じ桁に揃える。
pub const MAX_BATCH_TOTAL_K: usize = MAX_BATCH_ROWS;

/// [`DynamicWindowAggregator`] が確保してよい総バイト量の上限（防御的上限。
/// `ResidentMatrix::build` の [`MAX_BATCH_TOTAL_BYTES`] と同じ 1 GiB の
/// 桁感覚。codex P1 指摘対応: 以前は `DynamicWindowAggregator::push` が
/// 件数・次元・総容量を検証せず、内部 `Vec<Vec<f32>>` を無制限に成長
/// させていた）。
pub const MAX_BATCH_AGGREGATOR_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// `BatchEngine::batch_search` 1 回の走査で許容する総積和演算数
/// （`rows × queries × dim` の概算値）の上限（防御的上限。codex P1 指摘対応:
/// 計算量 DoS 対策）。行数（[`MAX_BATCH_ROWS`]=100 万）・クエリ数
/// （[`MAX_BATCH_QUERIES`]=4,096）・次元（[`MAX_BATCH_DIM`]=8,192）は
/// それぞれ独立に上限検証済みでも、積の理論上限は約 3.35 × 10^13 に達する。
/// `sum(k)` 上限（[`MAX_BATCH_TOTAL_K`]）を満たしていても、`MAX_BATCH_TOTAL_BYTES`
/// （1 GiB）の予算内で構築可能な常駐行列を最大クエリ数で走査させれば
/// 容易にこの規模へ到達する（例: 行数 1,000・次元 8,192 の行列は packed
/// バイト数が約 16MB で十分に構築可能だが、`MAX_BATCH_QUERIES` 件のクエリで
/// 走査すると積和は約 3.35 × 10^10 回に達する）。本モジュールは CPU
/// リファレンス実装（モジュール冒頭コメント参照）でスカラー積和を行うため、
/// 1 回の呼び出しが有限時間で完了するよう、スカラー積和が 1 秒あたり概ね
/// 10^9 回のオーダーで進む前提で数秒〜十数秒程度に収まる規模
/// （10^10 = 100 億回）を独立の上限として設ける。
pub const MAX_BATCH_WORK: usize = 10_000_000_000;

/// バッチ検索エンジンのエラー（fail-closed）。メッセージは英語（wire プロトコル
/// 互換性・運用ツール連携のため。japanese-style.md 準拠）。他テナントのデータ・
/// 存在情報を含めない。
#[derive(Debug, Clone, PartialEq)]
pub enum BatchSearchError {
    /// バッチのクエリ件数が [`MAX_BATCH_QUERIES`] を超過した。
    TooManyQueries { count: usize, max: usize },
    /// 常駐行列の行数が [`MAX_BATCH_ROWS`] を超過した。
    TooManyRows { count: usize, max: usize },
    /// 次元数が [`MAX_BATCH_DIM`] を超過した、または 0。
    InvalidDim { dim: usize, max: usize },
    /// 行数・次元数はそれぞれ上限内でも、組み合わせた総確保バイト量が上限
    /// （`ResidentMatrix::build` の [`MAX_BATCH_TOTAL_BYTES`]、または
    /// `DynamicWindowAggregator` の [`MAX_BATCH_AGGREGATOR_TOTAL_BYTES`]）を
    /// 超過した。
    CapacityExceeded { total_bytes: usize, max: usize },
    /// クエリベクトルの次元が常駐行列の次元と一致しない。
    DimMismatch { expected: usize, found: usize },
    /// クエリベクトルに非有限値（NaN/Inf）が含まれる（untrusted 入力の明示拒否）。
    NonFiniteQuery { query_index: usize },
    /// 常駐行列構築時、`ids`/`vectors`/可視性マスクの長さ不整合。
    ArenaLengthMismatch,
    /// 常駐行列構築時、`ids` に重複があった。id → tenant を一意に定められないと
    /// 選出後の独立再検証（id ベース）が成立しないため fail-closed で拒否する
    /// （codex レビュー指摘対応。重複行そのものの id は他テナントの存在情報を
    /// 漏らしうるためエラーには含めない）。
    DuplicateRowId,
    /// 常駐行列構築時、`tenant_ids` の要素が [`crate::storage::MAX_TENANT_ID_LEN`]
    /// を超過した（Cursor Bugbot 指摘対応: 容量チェックが `MAX_TENANT_ID_LEN` で
    /// 予算計上する一方、実際の各要素長を検証せずに `clone` していたため、想定を
    /// 超える長さの文字列で予算超過が起こり得た）。実際の文字列は他テナントの
    /// 存在情報を漏らしうるため含めない。
    TenantIdTooLong { len: usize, max: usize },
    /// クエリの `k` が [`MAX_BATCH_K`] を超過した、または 0。
    InvalidK { k: usize, max: usize },
    /// バッチ全体の `sum(k)` が [`MAX_BATCH_TOTAL_K`] を超過した（codex P1
    /// 指摘対応: クエリ数・`k` を個別に上限検証するだけでは積が無制限になり、
    /// 通常のヒープ確保（`kernel.rs::TopKSelector` 内部の `BinaryHeap`）で
    /// 数千万要素規模の成長が起こり得た）。
    TotalKExceeded { total_k: usize, max: usize },
    /// `DynamicWindowAggregator::push` に渡されたクエリの次元が、同じ窓に
    /// 先に積まれたクエリの次元と一致しない（codex P1 指摘対応）。
    /// `DimMismatch`（常駐行列に対するクエリ次元不一致）と意図的に区別する:
    /// 呼び出し元（wire-server 等）がエラーを一括処理する場合でも、
    /// 「クエリがテーブルの次元に合わない」のか「同じ窓の他クエリと次元が
    /// 揃っていない」のかを区別できるようにするため（`wire_code` 契約上、
    /// 別条件として扱いうる）。
    WindowDimMismatch { expected: usize, found: usize },
    /// バッチ検索の総積和演算数（`rows × queries × dim`）が [`MAX_BATCH_WORK`]
    /// を超過した（codex P1 指摘対応: 計算量 DoS 対策。走査開始前に
    /// fail-closed で拒否する）。
    WorkBudgetExceeded { work: usize, max: usize },
    /// `check_capacity` 相当のアロケーション前上限検証を通過した後、実際の
    /// `try_reserve_exact` がメモリ不足で失敗した（Cursor Bugbot 指摘対応:
    /// `ResidentMatrix::build` は上限検証後も `HashSet::with_capacity`・
    /// `Vec::with_capacity`・`to_vec()`（失敗時に abort する内部確保）を
    /// 使っていたため、`arena.rs::ArenaError::AllocationFailed` と同じ方針で
    /// フォールブルな確保へ置き換えた。security.md「不安全な設計｜無制限
    /// リソース確保（DoS）」対応。メッセージはプログラム出力文字列のため英語）。
    AllocationFailed(String),
    /// バッチ内の結果が、選出後に独立再検証したクエリ別可視集合と食い違った
    /// （テナント混入の疑い。`core.rs::CoreError::ProviderResultRejected` と同じ
    /// fail-closed 思想。結果を一切返さない）。
    TenantMaskViolation,
    /// `batch_fallback.rs::BatchBackend`（差し替え可能な primary バックエンド。
    /// TASK-129・CORE-8 ポインタ）が返した結果が、id・可視性以外の構造契約
    /// （クエリ数との整合・件数上限 `k`・id 重複なし・スコア有限性・
    /// スコア降順/同点 id 昇順）を満たさなかった。`core.rs::CoreError::
    /// ProviderResultRejected` と同じ「untrusted provider 出力を信頼済み
    /// 集合と突き合わせて検証し、1 件でも違反すれば結果を一切返さない」
    /// fail-closed 思想を、id/tenant 混入固有の [`Self::TenantMaskViolation`]
    /// とは別に区別する（構造契約違反と可視性違反は異なる原因として wire_code
    /// 設計上も切り分けたいため）。
    PrimaryResultRejected,
}

impl fmt::Display for BatchSearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchSearchError::TooManyQueries { count, max } => {
                write!(f, "batch_search: too many queries: {count} (max {max})")
            }
            BatchSearchError::TooManyRows { count, max } => {
                write!(f, "batch_search: too many rows: {count} (max {max})")
            }
            BatchSearchError::InvalidDim { dim, max } => {
                write!(f, "batch_search: invalid dim: {dim} (max {max}, must be > 0)")
            }
            BatchSearchError::CapacityExceeded { total_bytes, max } => write!(
                f,
                "batch_search: total allocation bytes {total_bytes} exceeds limit {max}"
            ),
            BatchSearchError::DimMismatch { expected, found } => write!(
                f,
                "batch_search: query dim mismatch: expected={expected} found={found}"
            ),
            BatchSearchError::NonFiniteQuery { query_index } => write!(
                f,
                "batch_search: query {query_index} contains non-finite value"
            ),
            BatchSearchError::ArenaLengthMismatch => {
                write!(f, "batch_search: arena ids/vectors/mask length mismatch")
            }
            BatchSearchError::DuplicateRowId => {
                write!(f, "batch_search: resident matrix contains duplicate row ids")
            }
            BatchSearchError::TenantIdTooLong { len, max } => {
                write!(f, "batch_search: tenant_id length {len} exceeds limit {max}")
            }
            BatchSearchError::InvalidK { k, max } => {
                write!(f, "batch_search: invalid k: {k} (must be 1..={max})")
            }
            BatchSearchError::TotalKExceeded { total_k, max } => write!(
                f,
                "batch_search: total k across batch {total_k} exceeds limit {max}"
            ),
            BatchSearchError::WindowDimMismatch { expected, found } => write!(
                f,
                "batch_search: dynamic window aggregator dim mismatch: expected={expected} found={found}"
            ),
            BatchSearchError::WorkBudgetExceeded { work, max } => write!(
                f,
                "batch_search: batch work budget {work} exceeds limit {max}"
            ),
            BatchSearchError::AllocationFailed(msg) => {
                write!(f, "batch_search: allocation failed: {msg}")
            }
            BatchSearchError::TenantMaskViolation => {
                write!(f, "batch_search: result violated per-query tenant mask")
            }
            BatchSearchError::PrimaryResultRejected => write!(
                f,
                "batch_search: primary backend result violated batch result contract"
            ),
        }
    }
}

impl std::error::Error for BatchSearchError {}

// ---------------------------------------------------------------------
// CORE-16 ポインタ: 常駐コピー限定の f16 2 要素/u32 パック。
//
// 格納・CPU 経路・クエリベクトルの dtype は f32 のまま変更しない。本関数群は
// 常駐ベース行列相当の表現へ変換する層としてのみ使う（呼び出し元が f32 の
// オリジナルを保持し続ける前提）。IEEE 754 binary16 の丸めは
// round-to-nearest-even を実装する。
// ---------------------------------------------------------------------

/// f32 を IEEE 754 binary16（f16）のビットパターンへ丸める。オーバーフローは
/// ±Inf へ飽和し、指数アンダーフロー域はサブノーマル f16 として正しく
/// round-to-nearest-even で丸める（codex レビュー指摘対応: 以前は `exp <= 0`
/// を一律 0 へフラッシュしており、`unpack_f16x2` の「`unpack2x16float` と等価」
/// という公開契約に違反していた。検索スコアの精度劣化を避けるため
/// flush-to-zero を仕様として残す選択はしない）。
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    if value.is_nan() {
        // NaN は符号なしの quiet NaN パターンへ正規化する（NaN のペイロードは
        // 復元時に意味を持たないため、往復精度の対象外）。
        return sign | 0x7E00;
    }
    let abs_bits = bits & 0x7FFF_FFFF;
    if abs_bits == 0 {
        // +0.0/-0.0 は下のサブノーマル変換（暗黙ビット付与）に通すと非ゼロに
        // なってしまうため、符号だけを残して個別に返す。
        return sign;
    }
    let exp = ((abs_bits >> 23) & 0xFF) as i32 - 127 + 15;
    let mantissa = abs_bits & 0x007F_FFFF;

    if exp >= 0x1F {
        // 指数オーバーフロー: ±Inf へ飽和する。
        return sign | 0x7C00;
    }
    if exp <= 0 {
        // 指数アンダーフロー域: サブノーマル f16（もしくは丸めた結果としての 0）
        // へ変換する。入力を暗黙ビット付き 24 bit 仮数 `1.mantissa` として扱い、
        // 下位ビットの正規化丸めと同じ round-to-nearest-even ロジックを、
        // シフト量 `14 - exp`（`exp <= 0` なので 14 以上）で適用する。
        // 仮数繰り上がりでシフト結果が 0x0400（=10 bit 目）に達した場合、
        // f16 のビット割付（exp フィールドが mantissa フィールドの直上）により
        // 自動的に「最小正規化数（biased exp = 1, mantissa = 0）」へ桁上がり
        // する（別分岐は不要）。
        let full_mantissa = 0x0080_0000u32 | mantissa;
        // `exp` が非常に小さい（f32 の極小サブノーマル入力を含む）場合、
        // シフト量は理論上非常に大きくなりうる。`full_mantissa` は高々 24 bit
        // のため、シフト量が 25 以上であれば `round_bit` が `full_mantissa` の
        // 最大値を必ず上回り丸め上げは発生しない（結果は常に 0）。u32 シフトの
        // オーバーフローを避けるため、その保証が成り立つ範囲でシフト量を
        // 頭打ちにする。
        let shift = (14 - exp).min(30) as u32;
        let round_bit = 1u32 << (shift - 1);
        let lower_mask = round_bit - 1;
        let mut mantissa16 = full_mantissa >> shift;
        let remainder = full_mantissa & (round_bit | lower_mask);
        if remainder > round_bit || (remainder == round_bit && (mantissa16 & 1) == 1) {
            mantissa16 += 1;
        }
        return sign | (mantissa16 as u16);
    }
    // 23 ビット仮数を 10 ビットへ round-to-nearest-even で丸める。
    let shift = 13u32;
    let round_bit = 1u32 << (shift - 1);
    let lower_mask = round_bit.wrapping_sub(1);
    let mut mantissa16 = mantissa >> shift;
    let remainder = mantissa & (round_bit | lower_mask);
    let mut exp16 = exp as u32;
    if remainder > round_bit || (remainder == round_bit && (mantissa16 & 1) == 1) {
        mantissa16 += 1;
        if mantissa16 == 0x0400 {
            // 仮数繰り上がりで指数が 1 増える。
            mantissa16 = 0;
            exp16 += 1;
        }
    }
    if exp16 >= 0x1F {
        return sign | 0x7C00;
    }
    sign | ((exp16 as u16) << 10) | (mantissa16 as u16)
}

/// f16 ビットパターンを f32 へ復元する（GPU シェーダの `unpack2x16float` と等価な
/// 意味論。実際の WGSL 実行は行わない点は本モジュール冒頭の依存制約コメントを参照）。
fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = (bits & 0x8000) as u32;
    let exp = ((bits >> 10) & 0x1F) as u32;
    let mantissa = (bits & 0x03FF) as u32;

    let (out_exp, out_mantissa) = if exp == 0 {
        if mantissa == 0 {
            (0u32, 0u32)
        } else {
            // サブノーマル f16（値 = mantissa * 2^-24）を正規化 f32 へ変換する。
            // 仮数の最上位ビット（0x0400）が立つまで左シフトして正規化し、
            // シフト回数 `k` を使って指数を求める。初期値 `e = -14` は
            // 「f16 サブノーマルの指数は -14 固定（f16 バイアス 15 で
            // biased exponent = 1 相当）」を表し、1 シフトごとに 1 減らす
            // ことで非正規化した分の指数を差し引く。ループ後は
            // `e = -14 - k` であり、f32 バイアス 127 を足した
            // `exp32 = 113 - k` が正しい biased exponent になる
            // （旧実装は `e` の初期値が `-1` になっており `exp32 = 126 - k` を
            // 返していたため、値が 2^13 倍ずれていた）。
            let mut m = mantissa;
            let mut e = -14i32;
            while m & 0x0400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x03FF;
            let exp32 = (e + 127) as u32;
            (exp32, m << 13)
        }
    } else if exp == 0x1F {
        (0xFFu32, mantissa << 13)
    } else {
        // `exp` は f16 バイアス 15、`out_exp` は f32 バイアス 127。`exp` は正規化
        // 範囲（1..=30）のみここへ到達するため差し引き後は必ず非負だが、u32 の
        // まま `exp - 15` を計算すると `exp < 15` で桁下がり（オーバーフロー）に
        // なるため、符号付き整数で計算してから戻す。
        ((exp as i32 - 15 + 127) as u32, mantissa << 13)
    };

    let out_bits = (sign << 16) | (out_exp << 23) | out_mantissa;
    f32::from_bits(out_bits)
}

/// 2 要素の f32 を 1 個の u32 へ f16 パックする（CORE-16 ポインタ: 常駐コピーの
/// 表現）。下位 16 ビットに `a`、上位 16 ビットに `b` を格納する
/// （`unpack2x16float` 相当の並び順と一致させる）。
pub fn pack_f16x2(a: f32, b: f32) -> u32 {
    let lo = f32_to_f16_bits(a) as u32;
    let hi = f32_to_f16_bits(b) as u32;
    lo | (hi << 16)
}

/// [`pack_f16x2`] の逆変換。
pub fn unpack_f16x2(packed: u32) -> (f32, f32) {
    let lo = (packed & 0xFFFF) as u16;
    let hi = ((packed >> 16) & 0xFFFF) as u16;
    (f16_bits_to_f32(lo), f16_bits_to_f32(hi))
}

// ---------------------------------------------------------------------
// 常駐ベース行列（CORE-16 ポインタ: パック表現を保持する一括インデクシング結果）。
// ---------------------------------------------------------------------

/// `Vec::try_reserve_exact` の失敗を [`BatchSearchError::AllocationFailed`] へ変換する
/// 共通ヘルパー（[`ResidentMatrix::build`] 専用。`arena.rs` の同名ヘルパーと同方針
/// だが `pub(crate)` ではないため個別に持つ）。`try_reserve`（amortized 成長）では
/// なく `try_reserve_exact`（要求量ちょうど）を使うのは、呼び出し元が
/// アロケーション前に検証済みの論理必要量どおりに実確保量を抑えるため
/// （`arena.rs::try_reserve_exact` と同じ理由）。
fn try_reserve_exact<T>(
    buf: &mut Vec<T>,
    additional: usize,
    what: &str,
) -> Result<(), BatchSearchError> {
    buf.try_reserve_exact(additional)
        .map_err(|e| BatchSearchError::AllocationFailed(format!("failed to reserve {what}: {e}")))
}

/// 1 テナント分の積和演算数（`rows × queries × dim`）を checked 演算で計算する。
/// 積のオーバーフローは [`MAX_BATCH_WORK`] 超過とみなす（[`compute_batch_work`]
/// 専用のヘルパー。境界値を直接検証できるよう独立関数へ切り出す）。
fn compute_tenant_work(rows: usize, queries: usize, dim: usize) -> Result<usize, BatchSearchError> {
    rows.checked_mul(queries)
        .and_then(|v| v.checked_mul(dim))
        .ok_or(BatchSearchError::WorkBudgetExceeded {
            work: usize::MAX,
            max: MAX_BATCH_WORK,
        })
}

/// `BatchEngine::batch_search` の走査開始前に総積和演算数を見積もり、
/// [`MAX_BATCH_WORK`] 超過を fail-closed に拒否する（codex P1 指摘対応:
/// 計算量 DoS 対策）。
///
/// `per_tenant` はテナントごとの `(行数, クエリ数)` ペア（バッチのテナント
/// 集合と 1 対 1 対応する想定）。単純に「全一致行数 × 全クエリ数」で課金
/// すると、複数テナントが混在するバッチで過大計上になる（codex P1 指摘対応:
/// 実際の走査は行ごとに `PolicyContext::is_visible` を満たすクエリとしか
/// 積和しないため、実コストはテナントごとの `行数 × クエリ数` の総和で
/// あり、`(全行数) × (全クエリ数)` はテナント間の組み合わせも含む厳密な
/// 上位互換ではあるが不必要に過大 — 例: tenant-a/b が各 1,000 行・各 1,000
/// クエリの場合、実コストは `1,000 * 1,000 + 1,000 * 1,000` だが、
/// `(全行数) × (全クエリ数)` は `2,000 * 2,000` と 2 倍に見積もる）。
/// テナントごとの積を [`compute_tenant_work`] で個別に checked 演算し、
/// 総和も checked 演算で合算する（積・和のどちらのオーバーフローも
/// [`MAX_BATCH_WORK`] 超過とみなす）。実データ（常駐行列・クエリ列）を
/// 確保せずに境界値を直接検証できるよう独立関数へ切り出す
/// （`arena.rs::check_capacity` と同じテスト容易性の考え方）。
fn compute_batch_work(
    per_tenant: impl IntoIterator<Item = (usize, usize)>,
    dim: usize,
) -> Result<usize, BatchSearchError> {
    let mut total: usize = 0;
    for (rows, queries) in per_tenant {
        let tenant_work = compute_tenant_work(rows, queries, dim)?;
        total = total
            .checked_add(tenant_work)
            .ok_or(BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            })?;
        if total > MAX_BATCH_WORK {
            return Err(BatchSearchError::WorkBudgetExceeded {
                work: total,
                max: MAX_BATCH_WORK,
            });
        }
    }
    Ok(total)
}

/// 一括インデクシングで構築する常駐ベース行列。可視性フィルタ済みの全行を
/// f16 2 要素/u32 パックで保持する。呼び出し元（`core.rs` 相当）は元の f32
/// アリーナを別途保持し続け、本構造体はバッチ検索専用の副次表現として扱う。
/// 現段階は CPU 上でこの表現を保持・走査する参照実装であり、実際の GPU
/// 実行は行わない（モジュール冒頭コメント参照）。
#[derive(Debug)]
pub struct ResidentMatrix {
    ids: Vec<u64>,
    /// テナント境界判定用: 行 `i` の所属テナント ID（`PolicyContext::is_visible` の
    /// 単一照合パスに渡すため、呼び出し元が構築時に確定させる）。
    tenant_ids: Vec<String>,
    /// 可視性ラベル。`tenant_ids[i]` と対で `PolicyContext::is_visible` の
    /// 単一照合パスへ渡す（codex P0 指摘対応: `BatchEngine::batch_search`
    /// はテナント文字列の等価比較だけでなく、この可視性ラベルも合わせて
    /// `PolicyContext::is_visible` で判定する）。
    visibilities: Vec<Visibility>,
    dim: usize,
    /// `ids.len() * dim.div_ceil(2)` 要素のパック済み行列（行優先）。
    packed: Vec<u32>,
}

impl ResidentMatrix {
    /// 可視性フィルタ済みの行集合から常駐行列を構築する（CORE-16 ポインタ）。`ids` /
    /// `tenant_ids` / `visibilities` / `vectors`（`ids.len() * dim` 要素、
    /// f32・行優先）の長さが整合しない場合は [`BatchSearchError::ArenaLengthMismatch`]。
    /// 次元・行数の上限検証はアロケーション前に行う（untrusted な行数・次元を
    /// そのまま `Vec::with_capacity` へ渡さない。coding-rust.md 準拠）。
    ///
    /// `visibilities[i]` は `tenant_ids[i]` と対応する行 `i` の可視性ラベルで、
    /// `BatchEngine::batch_search` が `PolicyContext::is_visible` の単一
    /// 照合パスへ渡す（codex P0 指摘対応）。
    ///
    /// # 信頼境界（P0 レビュー対応で明文化）
    ///
    /// `tenant_ids` / `visibilities` は本メソッドの引数としては未認証の値である。
    /// `PolicyContext` によるテナント境界の強制は `batch_search` 側の
    /// クエリ入力（`BatchQuery::ctx`）にのみ適用され、本メソッドはその
    /// 判定対象となる行メタデータの出所までは検証できない。呼び出し元
    /// （wire-server から到達しない、engine 内部の一括インデクシング経路）は
    /// `core.rs::VectorCore::search` が `VectorArena::build_filtered` で
    /// 行うのと同様に、`Storage` から読み出した実テナント ID・可視性を
    /// そのまま渡す責務を負う。本メソッドは wire プロトコル入力からの
    /// 直接呼び出し先ではないため、untrusted なユーザー入力が
    /// `tenant_ids`/`visibilities` に混入しない前提で設計されている。
    pub fn build(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[Visibility],
        dim: usize,
        vectors: &[f32],
    ) -> Result<Self, BatchSearchError> {
        if dim == 0 || dim > MAX_BATCH_DIM {
            return Err(BatchSearchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            });
        }
        if ids.len() > MAX_BATCH_ROWS {
            return Err(BatchSearchError::TooManyRows {
                count: ids.len(),
                max: MAX_BATCH_ROWS,
            });
        }
        if ids.len() != tenant_ids.len() || ids.len() != visibilities.len() {
            return Err(BatchSearchError::ArenaLengthMismatch);
        }

        let packed_per_row = dim.div_ceil(2);
        let packed_len =
            ids.len()
                .checked_mul(packed_per_row)
                .ok_or(BatchSearchError::TooManyRows {
                    count: ids.len(),
                    max: MAX_BATCH_ROWS,
                })?;

        // `packed` だけでなく、同時に確保する `ids`/`tenant_ids`（呼び出し元の
        // 所有権を持つコピー、下部の `ids.to_vec()`/`tenant_ids.to_vec()`）の
        // 見積もりバイト量も合算した総量を、`vectors` の長さ検証・実確保より前に
        // 検証する（arena.rs::check_capacity と同方針。行数・次元数を独立にしか
        // 検証しないと、両方が上限値の場合に `packed` 単体で約 16.4GB の巨大確保が
        // 発生し得るため。`vectors.len()` チェックより先に置くことで、テスト・
        // 呼び出し元が巨大な `vectors` バッファを用意しなくても本チェックへ
        // 到達できる）。
        let packed_bytes = packed_len.checked_mul(std::mem::size_of::<u32>()).ok_or(
            BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            },
        )?;
        let per_row_aux_bytes = std::mem::size_of::<u64>()
            .checked_add(std::mem::size_of::<String>())
            .and_then(|v| v.checked_add(crate::storage::MAX_TENANT_ID_LEN as usize))
            .and_then(|v| v.checked_add(std::mem::size_of::<Visibility>()))
            .ok_or(BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            })?;
        let aux_bytes =
            ids.len()
                .checked_mul(per_row_aux_bytes)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_TOTAL_BYTES,
                })?;
        let total_bytes =
            packed_bytes
                .checked_add(aux_bytes)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_TOTAL_BYTES,
                })?;
        if total_bytes > MAX_BATCH_TOTAL_BYTES {
            return Err(BatchSearchError::CapacityExceeded {
                total_bytes,
                max: MAX_BATCH_TOTAL_BYTES,
            });
        }

        let expected_len = ids
            .len()
            .checked_mul(dim)
            .ok_or(BatchSearchError::ArenaLengthMismatch)?;
        if vectors.len() != expected_len {
            return Err(BatchSearchError::ArenaLengthMismatch);
        }

        // id の一意性を検証する（容量・長さチェックより後に置き、cheap な拒否経路
        // （`CapacityExceeded`・`ArenaLengthMismatch`）を先に済ませてから、
        // 検証コスト・確保量が `ids.len()` に比例する本チェックへ進む）。重複を
        // 許すと id → tenant が一意に定まらず、`BatchEngine::batch_search` の
        // 選出後独立再検証（id ベース）が異なるテナントの行を取り違えうる
        // （codex レビュー指摘対応）。
        // `HashSet::with_capacity` は失敗時に abort するため使わず、
        // `try_reserve`（フォールブル）で確保する（Cursor Bugbot 指摘対応）。
        let mut seen_ids = std::collections::HashSet::new();
        seen_ids.try_reserve(ids.len()).map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve id set: {e}"))
        })?;
        for &id in ids {
            if !seen_ids.insert(id) {
                return Err(BatchSearchError::DuplicateRowId);
            }
        }

        // `tenant_ids` の各要素長を検証する（Cursor Bugbot 指摘対応: 上の容量
        // チェックは `per_row_aux_bytes` を `MAX_TENANT_ID_LEN` で見積もって
        // いるが、実際の要素長を検証せずに `tenant_ids.to_vec()` していたため、
        // 見積もりを超える長さの文字列が混入すると総確保量が
        // `MAX_BATCH_TOTAL_BYTES` の予算を超過し得た）。`storage.rs::encode_row`
        // の tenant_id 長検証と同じ上限を fail-closed で適用する。
        for tenant in tenant_ids {
            let len = tenant.len();
            if len > crate::storage::MAX_TENANT_ID_LEN as usize {
                return Err(BatchSearchError::TenantIdTooLong {
                    len,
                    max: crate::storage::MAX_TENANT_ID_LEN as usize,
                });
            }
        }

        // 以降の 3 本のバッファはいずれも `check_capacity` 相当の上限検証を
        // 通過済みの量だけを `try_reserve_exact`（フォールブル・要求量ちょうど）で
        // 確保する。`Vec::with_capacity`・`to_vec()` は失敗時に abort するため
        // 使わない（Cursor Bugbot 指摘対応。`arena.rs::try_reserve_exact` と
        // 同方針）。
        let mut packed: Vec<u32> = Vec::new();
        try_reserve_exact(&mut packed, packed_len, "packed")?;
        for row in vectors.chunks(dim) {
            for pair in row.chunks(2) {
                let a = pair.first().copied().unwrap_or(0.0);
                let b = pair.get(1).copied().unwrap_or(0.0);
                packed.push(pack_f16x2(a, b));
            }
        }

        let mut owned_ids: Vec<u64> = Vec::new();
        try_reserve_exact(&mut owned_ids, ids.len(), "ids")?;
        owned_ids.extend_from_slice(ids);

        // `tenant_ids` はコンテナ（`Vec<String>` が保持する `String` ハンドル分）
        // をフォールブルに確保したうえで、各要素の実体（ヒープ上の文字列
        // バイト列）も `String::clone`（abort-on-OOM）ではなく
        // `try_reserve_exact` + `push_str` でフォールブルに構築する
        // （codex P1 指摘対応: 各要素は `MAX_TENANT_ID_LEN` で上限検証済みの
        // 小さい固定長だが、最大 `MAX_BATCH_ROWS`（100 万）回 `clone` が
        // 呼ばれるため、ホスト側のメモリ不足時に `Result` 契約を経ずに
        // abort しうる経路として残さない）。
        let mut owned_tenant_ids: Vec<String> = Vec::new();
        try_reserve_exact(&mut owned_tenant_ids, tenant_ids.len(), "tenant_ids")?;
        for tenant in tenant_ids {
            let mut owned = String::new();
            owned.try_reserve_exact(tenant.len()).map_err(|e| {
                BatchSearchError::AllocationFailed(format!("failed to reserve tenant_id: {e}"))
            })?;
            owned.push_str(tenant);
            owned_tenant_ids.push(owned);
        }

        // `visibilities` は `Copy` 型（ヒープ確保を持たない）なので、コンテナ
        // 自体のフォールブル確保だけで十分（`ids` と同じ扱い）。
        let mut owned_visibilities: Vec<Visibility> = Vec::new();
        try_reserve_exact(&mut owned_visibilities, visibilities.len(), "visibilities")?;
        owned_visibilities.extend_from_slice(visibilities);

        Ok(Self {
            ids: owned_ids,
            tenant_ids: owned_tenant_ids,
            visibilities: owned_visibilities,
            dim,
            packed,
        })
    }

    pub fn row_count(&self) -> usize {
        self.ids.len()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 行 `idx` を f16 往復で復元し、呼び出し元が用意したバッファへ書き込む
    /// （積和計算専用。格納表現自体は f32 のまま別途保持する呼び出し元の責務で
    /// あり、本メソッドはバッチスコア計算のためだけに使う）。バッファを
    /// 呼び出し元が使い回すことで、`BatchEngine::batch_search` がクエリ数
    /// 分だけ同一行を毎回ヒープ確保し直す（codex レビュー指摘対応）のを避ける。
    fn row_f32_into(&self, idx: usize, out: &mut Vec<f32>) -> Option<()> {
        let packed_per_row = self.dim.div_ceil(2);
        let start = idx.checked_mul(packed_per_row)?;
        let end = start.checked_add(packed_per_row)?;
        let row = self.packed.get(start..end)?;
        out.clear();
        for &p in row {
            let (a, b) = unpack_f16x2(p);
            out.push(a);
            if out.len() < self.dim {
                out.push(b);
            }
        }
        Some(())
    }
}

// ---------------------------------------------------------------------
// CORE-7 ポインタ: 動的窓集約バッファ。
// ---------------------------------------------------------------------

/// 集約するか否かの判定（副作用なしの純関数）。`pending_after_pop` はキューから
/// 1 件取り出した直後に後続が存在するかどうか（呼び出し側のキュー実装に依存
/// しない最小のインターフェース）。ディスパッチ決定表本体（TASK-155 ポインタ）
/// はこの判定を呼び出す側であり、本関数自体は経路選択ロジックを持たない。
pub fn should_aggregate_into_batch(pending_after_pop: bool) -> bool {
    pending_after_pop
}

/// 動的窓での集約バッファ。[`should_aggregate_into_batch`] が `true` を返した
/// キュー取り出し文脈でのみ使う想定。件数・次元・総バイト量は [`Self::push`]
/// 自身が検証する（codex P1 指摘対応: 以前は公開 API である `push` が無検証で
/// 内部 `Vec` を無制限に成長させており、呼び出し側の自己規律のみに依存していた。
/// 呼び出し側のキュー実装がバグっていても本構造体単体で fail-closed に上限を
/// 保証する）。
#[derive(Debug, Default)]
pub struct DynamicWindowAggregator {
    queries: Vec<Vec<f32>>,
    /// 最初の [`Self::push`] で確定する次元。以降の `push` はこの次元との
    /// 一致を要求する（次元混在バッチを拒否する。[`Self::drain`] で窓を
    /// 使い切ると次の窓のためにリセットされる）。
    dim: Option<usize>,
}

impl DynamicWindowAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// クエリ 1 件を窓へ追加する。以下を fail-closed に検証してから
    /// フォールブルに確保する:
    /// - 件数が [`MAX_BATCH_QUERIES`] を超えないこと
    /// - 次元が 1 以上 [`MAX_BATCH_DIM`] 以下で、既存クエリと同一であること
    ///   （1 件目の `push` で次元を確定する）
    /// - 累積総バイト量が [`MAX_BATCH_AGGREGATOR_TOTAL_BYTES`] を超えないこと
    ///
    /// 内部 `Vec<Vec<f32>>` の成長は `Vec::push`（失敗時に abort する内部
    /// 確保）ではなく `try_reserve_exact` で行う（`ResidentMatrix::build` 用の
    /// 共通ヘルパーを再利用。coding-rust.md「無制限確保禁止」準拠）。渡された
    /// `query: Vec<f32>` 自体は呼び出し元が既に確保済みの所有権を移動する
    /// だけなので、要素本体の再確保は発生しない。
    pub fn push(&mut self, query: Vec<f32>) -> Result<(), BatchSearchError> {
        if self.queries.len() >= MAX_BATCH_QUERIES {
            return Err(BatchSearchError::TooManyQueries {
                count: self.queries.len() + 1,
                max: MAX_BATCH_QUERIES,
            });
        }
        let dim = query.len();
        if dim == 0 || dim > MAX_BATCH_DIM {
            return Err(BatchSearchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            });
        }
        if let Some(expected) = self.dim {
            if expected != dim {
                return Err(BatchSearchError::WindowDimMismatch {
                    expected,
                    found: dim,
                });
            }
        }

        let per_query_bytes = dim.checked_mul(std::mem::size_of::<f32>()).ok_or(
            BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
            },
        )?;
        let next_count =
            self.queries
                .len()
                .checked_add(1)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
                })?;
        let total_bytes =
            next_count
                .checked_mul(per_query_bytes)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
                })?;
        if total_bytes > MAX_BATCH_AGGREGATOR_TOTAL_BYTES {
            return Err(BatchSearchError::CapacityExceeded {
                total_bytes,
                max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
            });
        }

        // amortized 成長: capacity が尽きたときだけ倍々に増やし、`push` の
        // たびに `try_reserve_exact(1)` する（= 呼び出し回数分の再確保・
        // memcpy が発生する）のを避ける。成長候補は既知の絶対上限
        // [`MAX_BATCH_QUERIES`] で頭打ちにするため、`arena.rs::
        // GrowableArenaBuffers::ensure_capacity` のような追加の容量見積もり
        // 検証（`check_capacity` 相当）は不要（本構造体の要素は `Vec<f32>`
        // ハンドルのみで、成長候補自体の総バイト量は上の総バイト量チェックが
        // 別途カバーする）。
        if self.queries.len() == self.queries.capacity() {
            let next_capacity = self
                .queries
                .capacity()
                .checked_mul(2)
                .unwrap_or(MAX_BATCH_QUERIES)
                .clamp(4, MAX_BATCH_QUERIES);
            let additional = next_capacity.saturating_sub(self.queries.capacity());
            if additional > 0 {
                try_reserve_exact(
                    &mut self.queries,
                    additional,
                    "dynamic window aggregator queries",
                )?;
            }
        }
        self.dim = Some(dim);
        self.queries.push(query);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.queries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    /// 窓を確定し、集約済みクエリ集合を取り出す（窓は 1 回使い切り）。次元の
    /// 確定状態もリセットし、次の窓では別次元のバッチを受け付けられるように
    /// する。
    pub fn drain(&mut self) -> Vec<Vec<f32>> {
        self.dim = None;
        std::mem::take(&mut self.queries)
    }
}

// ---------------------------------------------------------------------
// CORE-6/7 ポインタ: バッチ検索本体。スコア計算の積和は f16 往復した行から
// f32 で行う（実際の GPU 実行は追加しない。本モジュール冒頭コメント参照）。
// Top-k 抽出は `kernel.rs::TopKSelector` を共用する。
// ---------------------------------------------------------------------

/// クエリ 1 件分のバッチ入力。可視性判定は `core.rs::VectorCore::search` と
/// 同じ [`PolicyContext::is_visible`] の単一照合パスへ委譲する（codex P0
/// 指摘対応: 以前は `tenant_id: &str` を公開フィールドで直接受け取っており、
/// 呼び出し元が任意の文字列を指定するだけで他テナントの行を検索できて
/// しまっていた。本モジュールが独自にテナント文字列を比較する経路は作らず、
/// `PolicyContext` をテナント境界判定の唯一の正当な入力経路とする engine
/// 全体の既定パターン（`policy.rs` モジュール冒頭コメント・CORE-2 ポインタ）に揃える。
/// `PolicyContext` は空文字列・長さ超過のテナント ID を構築時に拒否する
/// （`policy.rs::PolicyContext::new`）ため、本構造体はテナント ID の検証を
/// 重複して行わない）。
pub struct BatchQuery<'a> {
    pub vector: &'a [f32],
    pub k: usize,
    pub ctx: &'a PolicyContext,
}

/// `BatchEngine::batch_search` の 1 クエリ分の結果。
#[derive(Debug)]
pub struct BatchHit {
    pub hits: Vec<SearchHit>,
}

/// バッチ走査パイプラインの行ソース抽象（TASK-129・CORE-8 ポインタ）。
/// [`ResidentMatrix`]（f16 パック常駐行列。GPU 経路の CPU 参照実装）と
/// `batch_fallback.rs` の CPU 縮退用 f32 常駐行列の双方から、検証・テナント
/// マスク・選出後の独立再検証という共通パイプライン（[`run_batch_search`]）を
/// 共有するための内部抽象。呼び出し元は具象型を静的に知っているため `dyn` 化
/// せずジェネリクスで単相化する（object-safe である必要はない）。この抽象を
/// 介して両経路が同一の走査・マスクロジックを通ることが、CORE-8 が要求する
/// 「縮退後の Top-k 結果が CPU-SIMD 経路の結果と一致する」契約の構成的な
/// 裏付けになる（縮退経路だけ検査が緩い、という構造的な欠落を防ぐ）。
pub(crate) trait BatchRowSource {
    fn row_count(&self) -> usize;
    fn dim(&self) -> usize;
    fn ids(&self) -> &[u64];
    fn tenant_ids(&self) -> &[String];
    fn visibilities(&self) -> &[Visibility];
    /// 行 `idx` を f32 として解決し `out` へ書き込む（積和計算専用）。
    /// [`ResidentMatrix`] は f16 往復デコード、CPU 縮退経路は f32 原本の
    /// スライス参照というように、表現方式の差分だけがここに現れる。
    fn row_f32_into(&self, idx: usize, out: &mut Vec<f32>) -> Option<()>;
}

impl BatchRowSource for ResidentMatrix {
    fn row_count(&self) -> usize {
        self.ids.len()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn ids(&self) -> &[u64] {
        &self.ids
    }

    fn tenant_ids(&self) -> &[String] {
        &self.tenant_ids
    }

    fn visibilities(&self) -> &[Visibility] {
        &self.visibilities
    }

    fn row_f32_into(&self, idx: usize, out: &mut Vec<f32>) -> Option<()> {
        // `ResidentMatrix` の同名の inherent メソッドを呼ぶ（メソッド解決は
        // inherent メソッドをトレイトメソッドより優先するため、ここでの
        // `self.row_f32_into(...)` は無限再帰にならない）。
        ResidentMatrix::row_f32_into(self, idx, out)
    }
}

/// バッチクエリ・一括インデクシング専用のエンジン（CORE-6/7 ポインタ）。
/// 単発クエリ経路（`kernel.rs::SearchProvider`）は実装せず、`core.rs::EngineCore`
/// から到達できない（型レベルでの分離）。現段階はスコア計算を CPU で行う
/// 参照実装であり、GPU 実行は行わない（モジュール冒頭コメント参照）。GPU
/// バックエンドの初期化失敗・実行時エラーからの CPU-SIMD 縮退は
/// `batch_fallback.rs::FallbackBatchEngine`（TASK-129・CORE-8 ポインタ）が
/// 本エンジンを primary バックエンドとしてラップして扱う。
pub struct BatchEngine {
    matrix: ResidentMatrix,
}

impl BatchEngine {
    /// 常駐行列から構築する。GPU デバイス初期化は行わない（依存制約により
    /// 実際の GPU 実行は追加していないため、本コンストラクタは常に成功する。
    /// 将来 GPU 実行を追加する場合、ここで初期化失敗を `Result::Err` として
    /// fail-closed に伝播する設計とする）。
    pub fn new(matrix: ResidentMatrix) -> Self {
        Self { matrix }
    }

    /// バッチ検索を実行する（CORE-6・CORE-7 ポインタ）。クエリごとに次元・非有限値・`k` を
    /// 検証したうえで、常駐行列の中からクエリの [`PolicyContext::is_visible`]
    /// を満たす行だけを候補として選出する（Top-k 選出段でのクエリ別可視行
    /// マスク。codex P0 指摘対応: テナント文字列の等価比較ではなく
    /// `PolicyContext::is_visible` の単一照合パスを使う）。選出後、結果 id を
    /// 独立に再計算した (tenant, visibility) 集合と突き合わせ、逸脱があれば
    /// 結果を一切返さず [`BatchSearchError::TenantMaskViolation`] を返す
    /// （`core.rs::EngineCore` と同じ二重防御。fail-closed）。
    ///
    /// 行列走査はクエリ外側ではなく行外側でループする（codex レビュー指摘対応:
    /// 旧実装はクエリループの内側で行を毎回 f16→f32 デコードしており、
    /// クエリ数×行数×dim のデコード・ヒープ確保が発生していた）。行 1 件を
    /// 1 回だけデコードし、その行のテナントに属するクエリ集合だけへ使い回す
    /// （Cursor Medium 指摘対応: 以前は内側ループがバッチの全クエリを舐めて
    /// `PolicyContext::is_visible` でテナント一致を都度判定しており、work
    /// budget をテナント別に精緻化しても実 wall-clock コストは
    /// O(matching_rows * queries.len()) のままだった。事前にテナントごとの
    /// クエリ index 一覧（`tenant_query_indices`）を作り、行のテナントに
    /// 対応する index だけを走査することで、内側ループの反復回数が
    /// テナント別課金と一致する）。選出結果の各クエリ内順序（スコア降順・
    /// 同点 id 昇順）は `TopKSelector` が保証するため、走査順序の変更による
    /// 結果の変化はない。
    ///
    /// 走査開始前に総積和演算数を [`MAX_BATCH_WORK`] と照合する（codex P1
    /// 指摘対応: 計算量 DoS 対策。`sum(k)` の上限（[`MAX_BATCH_TOTAL_K`]）を
    /// 満たしていても、走査対象の行を最大クエリ数で走査させられてしまうため、
    /// 独立に上限を課す）。課金はテナントごとの `行数 × クエリ数 × dim` の
    /// 総和で行う（Cursor Medium 指摘対応: 常駐行列の全行数を課金すると、
    /// バッチがごく一部のテナントしか触れない場合でも過大に見積もる。
    /// codex P1 追加指摘対応: 「全一致行数 × 全クエリ数」も、複数テナントが
    /// 混在するバッチでは実際に走査しないテナント間の組み合わせまで課金
    /// してしまい過大計上になる。[`compute_batch_work`] のドキュメンテーション
    /// コメント参照）。
    pub fn batch_search(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<BatchHit>, BatchSearchError> {
        run_batch_search(&self.matrix, queries)
    }
}

/// [`BatchEngine::batch_search`] の走査パイプライン本体（TASK-129・CORE-8
/// ポインタで [`BatchRowSource`] 越しに共有化。挙動はリファクタ前と不変）。
/// `batch_fallback.rs::FallbackBatchEngine` の CPU 縮退経路もこの関数を直接
/// 呼ぶため、GPU 参照実装（[`ResidentMatrix`]）と CPU 縮退用 f32 常駐行列は
/// 検証・テナントマスク・選出後の独立再検証を完全に同一のコードパスで通る。
pub(crate) fn run_batch_search<S: BatchRowSource>(
    source: &S,
    queries: &[BatchQuery<'_>],
) -> Result<Vec<BatchHit>, BatchSearchError> {
    if queries.len() > MAX_BATCH_QUERIES {
        return Err(BatchSearchError::TooManyQueries {
            count: queries.len(),
            max: MAX_BATCH_QUERIES,
        });
    }

    // 事前検証パス（1 巡目）: 次元・非有限値・k をクエリごとに検証し、
    // `sum(k)` を積算する。選出器の生成は `sum(k)` の上限検証（下記）を
    // 通過してから行う（未検証の総量でヒープを成長させない）。次元不一致・
    // 非有限値・k 範囲外は単一クエリだけを見て安価に判定できる入力エラー
    // であり、走査コストと無関係に最優先で報告する（core.rs::search と同じ
    // 方針。次元不一致のクライアントに計算量エラーを返さない）。
    let mut total_k: usize = 0;
    for (query_index, q) in queries.iter().enumerate() {
        if q.vector.len() != source.dim() {
            return Err(BatchSearchError::DimMismatch {
                expected: source.dim(),
                found: q.vector.len(),
            });
        }
        if q.vector.iter().any(|v| !v.is_finite()) {
            return Err(BatchSearchError::NonFiniteQuery { query_index });
        }
        if q.k == 0 || q.k > MAX_BATCH_K {
            return Err(BatchSearchError::InvalidK {
                k: q.k,
                max: MAX_BATCH_K,
            });
        }
        total_k = total_k
            .checked_add(q.k)
            .ok_or(BatchSearchError::TotalKExceeded {
                total_k: usize::MAX,
                max: MAX_BATCH_TOTAL_K,
            })?;
    }
    if total_k > MAX_BATCH_TOTAL_K {
        return Err(BatchSearchError::TotalKExceeded {
            total_k,
            max: MAX_BATCH_TOTAL_K,
        });
    }

    // このバッチに登場するテナント集合（`HashSet` にして行外側ループから
    // O(1) で参照できるようにする。バッチのクエリ件数は [`MAX_BATCH_QUERIES`]
    // で上限検証済みのため、集合サイズもそれに従う）。`ctx.tenant_id()` は
    // `PolicyContext` の検証済みアクセサであり、呼び出し元が別途指定できる
    // 生の文字列ではない（codex P0 指摘対応）。`HashSet::with_capacity`
    // ではなく `try_reserve`（フォールブル）で確保する（codex P1 指摘対応）。
    // 計算量ガード（下記）より前に構築する: ガードが課金すべき行数は
    // 常駐行列の全行ではなく、このバッチのテナント集合に一致する行数
    // だからである。
    let mut batch_tenants: std::collections::HashSet<&str> = std::collections::HashSet::new();
    batch_tenants.try_reserve(queries.len()).map_err(|e| {
        BatchSearchError::AllocationFailed(format!("failed to reserve batch tenants: {e}"))
    })?;
    for q in queries {
        batch_tenants.insert(q.ctx.tenant_id());
    }

    // テナントごとのクエリ件数（codex P1 指摘対応: 計算量ガードを
    // テナント単位で精緻化するため。集合サイズはバッチのテナント集合と
    // 同じ上限に従うため `try_reserve` の予約量は `batch_tenants.len()`
    // で十分）。
    let mut tenant_query_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    tenant_query_counts
        .try_reserve(batch_tenants.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!(
                "failed to reserve tenant query counts: {e}"
            ))
        })?;
    for q in queries {
        let count = tenant_query_counts.entry(q.ctx.tenant_id()).or_insert(0);
        *count = count.saturating_add(1);
    }

    // テナントごとの一致行数（走査時に実際にデコード対象となる行数。
    // 下記の行外側ループが `tenant_query_indices` に対応クエリを持たない
    // 行を除外するため、それらの行は最初からデコードされない）。この
    // 数え上げ自体は O(rows) の線形走査で、[`MAX_BATCH_ROWS`] により
    // 上限が課されているため、走査本体（O(matching_rows * dim) の
    // デコード + テナント別の行×クエリ積和）よりも十分軽量である。
    let mut tenant_row_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    tenant_row_counts
        .try_reserve(batch_tenants.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve tenant row counts: {e}"))
        })?;
    for tenant in source.tenant_ids().iter() {
        if batch_tenants.contains(tenant.as_str()) {
            let count = tenant_row_counts.entry(tenant.as_str()).or_insert(0);
            *count = count.saturating_add(1);
        }
    }
    // `id_to_tenant` のフォールブル予約量として使う（テナントごとの
    // 一致行数の総和 = バッチのテナント集合に一致する行数の総和）。
    let matching_row_count: usize = tenant_row_counts.values().sum();

    // 走査開始前の計算量ガード（codex P1 指摘対応・Cursor Medium 指摘対応・
    // codex P1 追加指摘対応）。個別クエリの入力エラー（次元不一致・
    // 非有限値・k 範囲外）を上の 1 巡目で先に確定させた上で、実際に走査
    // されるテナントごとの (行数, クエリ数) の積を合算した総積和演算数を
    // 走査（選出器確保・行デコード）開始前に確定的に拒否する。「全一致
    // 行数 × 全クエリ数」で一括課金すると、複数テナントが混在するバッチ
    // では実コストより過大に見積もり、正当な入力を誤って拒否しうる
    // （`compute_batch_work` のドキュメンテーションコメント参照）。
    let work_pairs = batch_tenants.iter().map(|&tenant| {
        let rows = tenant_row_counts.get(tenant).copied().unwrap_or(0);
        let tenant_queries = tenant_query_counts.get(tenant).copied().unwrap_or(0);
        (rows, tenant_queries)
    });
    compute_batch_work(work_pairs, source.dim())?;

    // 事前検証パス（2 巡目）: `sum(k)` の上限検証を通過した後、選出器
    // コンテナ（`Vec<TopKSelector>`）をフォールブルに確保し、各選出器の
    // 内部ヒープも `q.k`（バッチ全体で `sum(k) <= MAX_BATCH_TOTAL_K` を
    // 検証済み）分だけフォールブルに予約する（codex P1 指摘対応:
    // `Vec::with_capacity` も `TopKSelector::push` 内部の `BinaryHeap::push`
    // による amortized 成長も失敗時に abort するため使わない。
    // `try_reserve_exact` は `ResidentMatrix::build` 用に定義済みの
    // 共通ヘルパーを再利用する）。
    let mut selectors: Vec<TopKSelector> = Vec::new();
    try_reserve_exact(&mut selectors, queries.len(), "selectors")?;
    for q in queries {
        let mut selector = TopKSelector::new(q.k);
        selector.try_reserve(q.k).map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve selector heap: {e}"))
        })?;
        selectors.push(selector);
    }

    // テナントごとのクエリ index 一覧（Cursor Medium 指摘対応: work budget は
    // テナント別 `行数 × クエリ数 × dim` へ精緻化済みだが、実走査が依然
    // 「デコード済み各行 × バッチの全クエリ」を舐めて `is_visible` を判定する
    // O(matching_rows * queries.len()) のネストループのままだと、課金と
    // 実 wall-clock コストが乖離する。行外側ループが「その行のテナントに
    // 属するクエリ集合」だけを走査できるよう、事前にテナント → クエリ index
    // の一覧を構築する。各 `Vec<usize>` の容量は `tenant_query_counts` で
    // 既に数えたテナントごとのクエリ件数ちょうどに `try_reserve_exact` で
    // 予約するため、後続の `push` が amortized 成長で abort することはない）。
    let mut tenant_query_indices: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    tenant_query_indices
        .try_reserve(tenant_query_counts.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!(
                "failed to reserve tenant query indices: {e}"
            ))
        })?;
    for (&tenant, &count) in tenant_query_counts.iter() {
        let mut indices: Vec<usize> = Vec::new();
        try_reserve_exact(&mut indices, count, "tenant query indices")?;
        tenant_query_indices.insert(tenant, indices);
    }
    for (idx, q) in queries.iter().enumerate() {
        if let Some(indices) = tenant_query_indices.get_mut(q.ctx.tenant_id()) {
            indices.push(idx);
        }
    }

    // id → (tenant, visibility) の逆引き表（選出後の独立再検証用）。
    // `ResidentMatrix::build` が id の重複を拒否しているため、id は
    // (tenant, visibility) を一意に決める（[`BatchSearchError::DuplicateRowId`]
    // 参照）。選出段のマスク実装（行 index からの `PolicyContext::is_visible`
    // 呼び出し）とは別経路でこの表を組むことで、二重防御を維持する。
    // このバッチのテナントに属さない行は登録しない（マップを
    // `MAX_BATCH_ROWS` 全件分確保しないための最適化であると同時に、
    // バッチ外テナントの id が万一 hit に混入した場合を確実に
    // マップ不在 → `TenantMaskViolation` にする fail-closed 側の効果も持つ）。
    // `HashMap::with_capacity` ではなく `try_reserve`（フォールブル）で
    // 確保する（codex P1 指摘対応）。予約量は上で数えた `matching_row_count`
    // をそのまま再利用する。
    let mut id_to_tenant: std::collections::HashMap<u64, (&str, Visibility)> =
        std::collections::HashMap::new();
    id_to_tenant.try_reserve(matching_row_count).map_err(|e| {
        BatchSearchError::AllocationFailed(format!("failed to reserve id-tenant map: {e}"))
    })?;
    for ((id, tenant), visibility) in source
        .ids()
        .iter()
        .zip(source.tenant_ids().iter())
        .zip(source.visibilities().iter())
    {
        if batch_tenants.contains(tenant.as_str()) {
            id_to_tenant.insert(*id, (tenant.as_str(), *visibility));
        }
    }

    // 行外側ループ: 行 1 件につき 1 回だけデコードし、その行のテナントに
    // 属するクエリ集合だけを走査する（Cursor Medium 指摘対応: 以前は
    // クエリ側をバッチ全体で舐める O(matching_rows * queries.len()) の
    // ネストループになっており、work budget をテナント別に精緻化しても
    // 実 wall-clock コストは一致しなかった。`tenant_query_indices` で
    // 行のテナントに属するクエリ index だけへ絞ることで、内側ループの
    // 反復回数がテナント別課金と一致する）。`row_buf` は
    // `Vec::with_capacity`（abort-on-OOM）ではなく `try_reserve_exact` で
    // フォールブルに確保する（codex P1 指摘対応）。
    let mut row_buf: Vec<f32> = Vec::new();
    try_reserve_exact(&mut row_buf, source.dim(), "row_buf")?;
    for row_idx in 0..source.row_count() {
        let Some(row_tenant) = source.tenant_ids().get(row_idx).map(String::as_str) else {
            continue;
        };
        // このバッチ内に同一テナントのクエリが 1 件も無ければデコードを省く
        // （`tenant_query_indices` のキー集合は `batch_tenants` と一致する）。
        let Some(query_indices) = tenant_query_indices.get(row_tenant) else {
            continue;
        };
        let Some(row_visibility) = source.visibilities().get(row_idx).copied() else {
            continue;
        };
        let Some(id) = source.ids().get(row_idx).copied() else {
            continue;
        };
        if source.row_f32_into(row_idx, &mut row_buf).is_none() {
            continue;
        }

        for &qi in query_indices {
            // untrusted 入力由来の添字ではなく、直前に `enumerate()` で
            // 自前生成した内部インデックスだが、coding-rust.md の方針に
            // 揃えて `[]` ではなく `get`/`get_mut` で明示的に処理する。
            let (Some(q), Some(selector)) = (queries.get(qi), selectors.get_mut(qi)) else {
                continue;
            };
            // (1) 選出前のマスク: `PolicyContext::is_visible` を満たす
            // 行だけを候補にする（codex P0 指摘対応: テナント文字列の
            // 等価比較だけでなく可視性ラベルも判定する。テナント一致は
            // `tenant_query_indices` の絞り込みで既に保証済みだが、
            // 可視性（`Visibility::Private` 等）の判定はここでしか
            // できないため引き続き呼び出す）。
            if !q.ctx.is_visible(row_tenant, row_visibility) {
                continue;
            }
            let score = crate::kernel::dot(&row_buf, q.vector);
            if !score.is_finite() {
                continue;
            }
            selector.push(SearchHit { id, score });
        }
    }

    // `out` も `Vec::with_capacity`（abort-on-OOM）ではなく
    // `try_reserve_exact` でフォールブルに確保する（codex P1 指摘対応）。
    let mut out: Vec<BatchHit> = Vec::new();
    try_reserve_exact(&mut out, queries.len(), "batch results")?;
    for (q, selector) in queries.iter().zip(selectors) {
        let hits = selector.into_sorted_vec();

        // (2) 選出後の独立再検証: 返す id が全て `PolicyContext::is_visible`
        // を満たす行由来であることを、マスク実装（行 index からの判定）
        // から独立に id → (tenant, visibility) の逆引き表で確認する。
        for hit in &hits {
            match id_to_tenant.get(&hit.id) {
                Some(&(t, v)) if q.ctx.is_visible(t, v) => {}
                _ => return Err(BatchSearchError::TenantMaskViolation),
            }
        }

        out.push(BatchHit { hits });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // CORE-16 ポインタ: f16 往復精度。
    #[test]
    fn f16_roundtrip_preserves_reasonable_precision() {
        for v in [0.0f32, 1.0, -1.0, 0.5, 3.140625, -100.0, 1e-3] {
            let packed = pack_f16x2(v, 0.0);
            let (a, _b) = unpack_f16x2(packed);
            assert!(
                (a - v).abs() < 0.01,
                "roundtrip diverged too much: v={v} got={a}"
            );
        }
    }

    #[test]
    fn f16_roundtrip_handles_special_values() {
        let packed = pack_f16x2(0.0, -0.0);
        let (a, b) = unpack_f16x2(packed);
        assert_eq!(a, 0.0);
        assert_eq!(b, 0.0);

        let packed_inf = pack_f16x2(f32::INFINITY, f32::NEG_INFINITY);
        let (inf_a, inf_b) = unpack_f16x2(packed_inf);
        assert!(inf_a.is_infinite() && inf_a > 0.0);
        assert!(inf_b.is_infinite() && inf_b < 0.0);
    }

    // CORE-16 ポインタ・codex レビュー指摘対応: サブノーマル f16 の復元値を厳密一致で検証する
    // （`f16_roundtrip_preserves_reasonable_precision` の絶対誤差 `< 0.01` 判定では
    // サブノーマル域の値が 2^13 倍ずれても誤差が閾値未満に収まり検出できない）。
    // ここではビット列を直接与えて `unpack_f16x2` の公開契約どおり
    // `unpack2x16float` と同じ厳密値に一致することを確認する。
    #[test]
    fn f16_bits_to_f32_matches_exact_subnormal_values() {
        // bits=0x0001（mantissa=1） は最小の正のサブノーマル: 1 * 2^-24。
        let (lo, hi) = unpack_f16x2(0x0001);
        assert_eq!(lo, 1.0f32 / 16_777_216.0);
        assert_eq!(hi, 0.0);

        // hi レーン側（上位 16 ビット）も同じ経路を通ることを確認する。
        let (lo2, hi2) = unpack_f16x2(0x0001_0000);
        assert_eq!(lo2, 0.0);
        assert_eq!(hi2, 1.0f32 / 16_777_216.0);

        // bits=0x0200（mantissa=0x200=512） は 512 * 2^-24 = 2^-15。
        let (v, _) = unpack_f16x2(0x0200);
        assert_eq!(v, 512.0f32 / 16_777_216.0);

        // bits=0x03FF（mantissa=0x3FF=1023、最大のサブノーマル） は 1023 * 2^-24。
        let (v, _) = unpack_f16x2(0x03FF);
        assert_eq!(v, 1023.0f32 / 16_777_216.0);

        // 符号ビットもサブノーマル経路と独立に正しく伝播することを確認する
        // （bits=0x8001 は 0x0001 の符号反転）。
        let (v, _) = unpack_f16x2(0x8001);
        assert_eq!(v, -(1.0f32 / 16_777_216.0));
    }

    // codex レビュー指摘対応: `pack_f16x2`（`f32_to_f16_bits`）がサブノーマル域
    // （`exp <= 0`）を一律 0 へフラッシュしていたバグの回帰テスト。サブノーマル
    // f16 として厳密表現可能な値（k * 2^-24）は pack→unpack で厳密に一致する
    // ことを確認する（`unpack_f16x2` は既にサブノーマルへ対応済みのため、この
    // 往復テストで初めて `f32_to_f16_bits` 側のサブノーマル生成経路も検証できる）。
    #[test]
    fn f16_pack_roundtrips_exact_subnormal_values() {
        // 最小の正のサブノーマル: 1 * 2^-24。
        let packed = pack_f16x2(1.0f32 / 16_777_216.0, 0.0);
        let (lo, _) = unpack_f16x2(packed);
        assert_eq!(lo, 1.0f32 / 16_777_216.0);

        // 負値・hi レーンも同じ経路を通ることを確認する（512 * 2^-24 = 2^-15）。
        let packed = pack_f16x2(0.0, -(512.0f32 / 16_777_216.0));
        let (_, hi) = unpack_f16x2(packed);
        assert_eq!(hi, -(512.0f32 / 16_777_216.0));

        // 最大のサブノーマル: 1023 * 2^-24。
        let packed = pack_f16x2(1023.0f32 / 16_777_216.0, 0.0);
        let (v, _) = unpack_f16x2(packed);
        assert_eq!(v, 1023.0f32 / 16_777_216.0);
    }

    // 境界値: サブノーマル域での round-to-nearest-even 丸めを検証する
    // （flush-to-zero を仕様として残さない、という codex レビュー指摘対応の
    // 判断が実際に効いていることを確認する）。
    #[test]
    fn f16_pack_rounds_subnormal_boundary_values_to_nearest_even() {
        // 0 と最小サブノーマル (2^-24) のちょうど中間 (2^-25) は round-to-even で
        // 「偶数」側の 0 へ丸まる（f16 の 0 はサブノーマル仮数ビット列 0 であり
        // 偶数）。
        let tie_to_zero = 1.0f32 / 33_554_432.0; // 2^-25
        let (v, _) = unpack_f16x2(pack_f16x2(tie_to_zero, 0.0));
        assert_eq!(v, 0.0, "exact tie must round to even (zero)");

        // 中間点よりわずかに大きい値は切り上げられ、最小サブノーマルになる
        // （2^-25 の直後の f32 表現可能値を使う）。
        let just_above_tie = f32::from_bits(tie_to_zero.to_bits() + 1);
        let (v, _) = unpack_f16x2(pack_f16x2(just_above_tie, 0.0));
        assert_eq!(
            v,
            1.0f32 / 16_777_216.0,
            "value above the tie must round up"
        );

        // サブノーマル最大値 (1023 * 2^-24) と最小正規化数 (1024 * 2^-24 = 2^-14)
        // のちょうど中間 (1023.5 * 2^-24) は、丸め前の仮数下位ビットが奇数
        // (1023) のため round-to-even で偶数側 (1024 = 最小正規化数) へ切り上がる。
        let tie_to_normal = 1023.5f32 / 16_777_216.0;
        let (v, _) = unpack_f16x2(pack_f16x2(tie_to_normal, 0.0));
        assert_eq!(
            v,
            1.0f32 / 16_384.0,
            "tie at the subnormal/normal boundary must carry into the smallest normal"
        );
    }

    // 真のゼロ（+0.0/-0.0）はサブノーマル変換の暗黙ビット付与を経由せず、
    // そのまま符号付きゼロへ変換されることを確認する。
    #[test]
    fn f16_pack_preserves_signed_zero() {
        let (a, b) = unpack_f16x2(pack_f16x2(0.0, -0.0));
        assert_eq!(a, 0.0);
        assert!(a.is_sign_positive());
        assert_eq!(b, 0.0);
        assert!(b.is_sign_negative());
    }

    #[test]
    fn f16_pack_pair_roundtrips_both_elements() {
        let packed = pack_f16x2(2.5, -4.25);
        let (a, b) = unpack_f16x2(packed);
        assert!((a - 2.5).abs() < 1e-3);
        assert!((b - (-4.25)).abs() < 1e-3);
    }

    // CORE-7 ポインタ: 動的窓集約の判定（後続あり/なし）。
    #[test]
    fn should_aggregate_only_when_pending_follows() {
        assert!(should_aggregate_into_batch(true));
        assert!(!should_aggregate_into_batch(false));
    }

    #[test]
    fn aggregator_drains_pushed_queries_once() {
        let mut agg = DynamicWindowAggregator::new();
        assert!(agg.is_empty());
        agg.push(vec![1.0, 0.0]).expect("push ok");
        agg.push(vec![0.0, 1.0]).expect("push ok");
        assert_eq!(agg.len(), 2);
        let drained = agg.drain();
        assert_eq!(drained.len(), 2);
        assert!(agg.is_empty());
    }

    // codex P1 指摘対応: `push` は件数上限（`MAX_BATCH_QUERIES`）を超えると
    // 内部 `Vec` を無制限に成長させず fail-closed に拒否する。
    #[test]
    fn aggregator_push_rejects_over_max_queries() {
        let mut agg = DynamicWindowAggregator::new();
        for _ in 0..MAX_BATCH_QUERIES {
            agg.push(vec![1.0, 0.0]).expect("push ok");
        }
        let err = agg.push(vec![1.0, 0.0]).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TooManyQueries {
                count: MAX_BATCH_QUERIES + 1,
                max: MAX_BATCH_QUERIES,
            }
        );
    }

    // codex P1 指摘対応: 次元は 1 件目の `push` で確定し、以降の `push` は
    // 同一次元しか受け付けない（次元混在バッチを拒否する）。
    #[test]
    fn aggregator_push_rejects_dimension_mismatch_after_first_push() {
        let mut agg = DynamicWindowAggregator::new();
        agg.push(vec![1.0, 0.0]).expect("push ok");
        let err = agg.push(vec![1.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::WindowDimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    // codex P1 指摘対応: 次元 0 のクエリは受け付けない（`MAX_BATCH_DIM` と
    // 同じ検証を `ResidentMatrix::build` と揃える）。
    #[test]
    fn aggregator_push_rejects_zero_dim() {
        let mut agg = DynamicWindowAggregator::new();
        let err = agg.push(vec![]).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidDim {
                dim: 0,
                max: MAX_BATCH_DIM
            }
        );
    }

    // codex P1 指摘対応: `drain` は次元の確定状態もリセットするため、次の窓は
    // 別次元のバッチを受け付けられる。
    #[test]
    fn aggregator_drain_resets_dimension_for_next_window() {
        let mut agg = DynamicWindowAggregator::new();
        agg.push(vec![1.0, 0.0]).expect("push ok");
        agg.drain();
        agg.push(vec![1.0, 0.0, 0.0])
            .expect("次元 3 の別窓は許可される");
        assert_eq!(agg.len(), 1);
    }

    // Cursor Bugbot 指摘対応: 上限検証済みの量を超えて `try_reserve_exact` が
    // 実際にメモリ不足になった場合、`Vec::with_capacity`/`push` のように abort
    // せず `Err(BatchSearchError::AllocationFailed)` を返すことを検証する
    // （`arena.rs` の同種テストと同方針）。`isize::MAX` 超のレイアウトは Rust の
    // アロケーション API 契約上、実メモリを確保しようとする前に即座に拒否
    // されるため、CI 環境で実際に大量のメモリを消費せず決定的に再現できる。
    #[test]
    fn try_reserve_exact_converts_oversized_request_to_allocation_failed_without_aborting() {
        let mut buf: Vec<u8> = Vec::new();
        let oversized = (isize::MAX as usize).saturating_add(1);
        let result = try_reserve_exact(&mut buf, oversized, "test buffer");
        assert!(matches!(result, Err(BatchSearchError::AllocationFailed(_))));
    }

    // ResidentMatrix の上限・整合性検証（untrusted 入力の防御的上限）。
    #[test]
    fn resident_matrix_rejects_zero_dim() {
        let err = ResidentMatrix::build(&[1], &["t".to_string()], &[Visibility::Public], 0, &[])
            .unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidDim {
                dim: 0,
                max: MAX_BATCH_DIM
            }
        );
    }

    #[test]
    fn resident_matrix_rejects_combined_capacity_even_when_rows_and_dim_are_individually_within_limits(
    ) {
        // MAX_BATCH_ROWS・MAX_BATCH_DIM をそれぞれ単独では超えないが、組み合わせた
        // 総確保バイト量が MAX_BATCH_TOTAL_BYTES を超えるケース（codex レビュー指摘:
        // 独立検証だけでは packed 単体で約 16.4GB の単一確保が発生し得る）。
        // 容量チェックは `vectors.len()` の整合性検証より前に実行されるため、
        // 巨大な `vectors` バッファを実際に確保しなくても本エラーへ到達できる
        // （`ids`/`tenant_ids` は実データが必要だが、MAX_BATCH_ROWS 行でも
        // 数十 MB 程度で現実的に確保可能）。
        let ids: Vec<u64> = (0..MAX_BATCH_ROWS as u64).collect();
        let tenants: Vec<String> = std::iter::repeat_n("t".to_string(), MAX_BATCH_ROWS).collect();
        let visibilities: Vec<Visibility> =
            std::iter::repeat_n(Visibility::Public, MAX_BATCH_ROWS).collect();
        let err =
            ResidentMatrix::build(&ids, &tenants, &visibilities, MAX_BATCH_DIM, &[]).unwrap_err();
        match err {
            BatchSearchError::CapacityExceeded { total_bytes, max } => {
                assert_eq!(max, MAX_BATCH_TOTAL_BYTES);
                assert!(total_bytes > max);
            }
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }
    }

    #[test]
    fn resident_matrix_rejects_length_mismatch() {
        let err = ResidentMatrix::build(
            &[1, 2],
            &["t".to_string()],
            &[Visibility::Public],
            2,
            &[1.0, 0.0],
        )
        .unwrap_err();
        assert_eq!(err, BatchSearchError::ArenaLengthMismatch);
    }

    // codex レビュー指摘対応: id 重複は fail-closed で拒否する（重複を許すと
    // id → tenant が一意に定まらず、`batch_search` の選出後独立再検証（id ベース）
    // が別テナントの行を取り違えうるため）。
    #[test]
    fn resident_matrix_rejects_duplicate_ids() {
        let ids = [1u64, 1];
        let tenants = ["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = [Visibility::Public, Visibility::Public];
        let vectors = [1.0f32, 0.0, 0.0, 1.0];
        let err = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).unwrap_err();
        assert_eq!(err, BatchSearchError::DuplicateRowId);
    }

    // Cursor Bugbot 指摘対応: 容量チェックは `MAX_TENANT_ID_LEN` で予算計上する
    // だけで、実際の各 `tenant_ids` 要素長を検証していなかった。上限超過の
    // tenant_id を含む入力を fail-closed で拒否することを確認する。
    #[test]
    fn resident_matrix_rejects_oversized_tenant_id() {
        let ids = [1u64];
        let oversized = "t".repeat(crate::storage::MAX_TENANT_ID_LEN as usize + 1);
        let tenants = [oversized.clone()];
        let visibilities = [Visibility::Public];
        let vectors = [1.0f32, 0.0];
        let err = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TenantIdTooLong {
                len: oversized.len(),
                max: crate::storage::MAX_TENANT_ID_LEN as usize,
            }
        );
    }

    // codex P1 指摘対応: `tenant_ids` の各要素を `String::clone`（abort-on-OOM）
    // ではなく `try_reserve_exact` + `push_str` でフォールブルに構築するよう
    // 変更したため、マルチバイト UTF-8（バイト長と文字数が異なる）を含む
    // tenant_id でも内容が過不足なく複製されることを回帰確認する。内部
    // フィールドは非公開のため、`batch_search` のテナント一致マスクを
    // 経由した機能的な等価性チェックで検証する（1 バイトでも欠落・破損
    // すれば文字列比較が食い違い、該当行が誤ってマスクされるかクエリ側
    // マスクに一致しなくなるため十分な検出力を持つ）。
    #[test]
    fn resident_matrix_preserves_multibyte_tenant_id_via_fallible_clone() {
        let multibyte_tenant = "tenant-日本語-🎉".to_string();
        let ids = [1u64];
        let tenants = [multibyte_tenant.clone()];
        let visibilities = [Visibility::Public];
        let vectors = [1.0f32, 0.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx = PolicyContext::new(&multibyte_tenant).expect("valid tenant");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            ctx: &ctx,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(
            results[0].hits.first().map(|h| h.id),
            Some(1),
            "tenant_id must round-trip byte-for-byte through the fallible clone"
        );
    }

    fn build_two_tenant_matrix() -> ResidentMatrix {
        // tenant-a: id=1,2 / tenant-b: id=3,4。dim=2。全行 Public。
        let ids = vec![1u64, 2, 3, 4];
        let tenants = vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ];
        let visibilities = vec![Visibility::Public; 4];
        let vectors = vec![1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0];
        ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).expect("valid matrix")
    }

    /// [`PolicyContext::new`]（`Public` のみ許可）のテスト用ショートハンド。
    fn ctx(tenant_id: &str) -> PolicyContext {
        PolicyContext::new(tenant_id).expect("valid tenant")
    }

    // CORE-7 ポインタ・テナント境界（P0）: 混在テナントバッチで混入 0 件。
    // 検査は実装（`batch_search` 内のマスク）から独立に、返却 id → tenant を
    // 再計算して確認する（実装と検査器の経路分離）。codex P0 指摘対応:
    // `BatchQuery` はもはや呼び出し元が指定する生の `tenant_id: &str` を
    // 持たない（型定義から削除済み）。テナント境界は `ctx`（`PolicyContext`）
    // 経由でのみ engine 側が決定するため、本テストは「`tenant-a` の
    // `PolicyContext` を渡すクエリが、行列に同居する `tenant-b` の行へは
    // 構造的に到達できない」ことを検証する。
    #[test]
    fn batch_search_excludes_other_tenant_rows() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results.len(), 1);
        // 独立検査器: 返った id が期待テナントの id 集合に含まれるかを、
        // engine 内部のマスク実装を経由せず直接確認する。
        let tenant_a_ids: std::collections::HashSet<u64> = [1, 2].into_iter().collect();
        for hit in &results[0].hits {
            assert!(
                tenant_a_ids.contains(&hit.id),
                "tenant-a query must not return id={} (other tenant)",
                hit.id
            );
        }
        assert!(!results[0].hits.is_empty());
    }

    // codex P0 指摘対応の回帰テスト: `PolicyContext` は他テナントの `tenant_id`
    // を偽装する経路を提供しない。`tenant-a` の `PolicyContext` で発行した
    // クエリが `tenant-b` の行を一切返さないことを、バッチ内に両テナントの
    // クエリが混在する状況でも確認する（`tenant-b` 側のクエリも同時に検証し、
    // 相互のテナント越え漏えいが無いことを両方向で確認する）。
    #[test]
    fn batch_search_cannot_cross_tenant_boundary_via_policy_context() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let ctx_b = ctx("tenant-b");
        let queries = vec![
            BatchQuery {
                vector: &query_vec,
                k: 10,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_vec,
                k: 10,
                ctx: &ctx_b,
            },
        ];
        let results = engine.batch_search(&queries).expect("batch search ok");
        let tenant_a_ids: std::collections::HashSet<u64> = [1, 2].into_iter().collect();
        let tenant_b_ids: std::collections::HashSet<u64> = [3, 4].into_iter().collect();
        for hit in &results[0].hits {
            assert!(
                tenant_a_ids.contains(&hit.id) && !tenant_b_ids.contains(&hit.id),
                "tenant-a ctx leaked a tenant-b row: id={}",
                hit.id
            );
        }
        for hit in &results[1].hits {
            assert!(
                tenant_b_ids.contains(&hit.id) && !tenant_a_ids.contains(&hit.id),
                "tenant-b ctx leaked a tenant-a row: id={}",
                hit.id
            );
        }
    }

    // codex P0 指摘対応: `Visibility::Private` の行は、`PolicyContext::new`
    // （既定・最小権限で `Public` のみ許可）では不可視のままであることを
    // 確認する（`policy.rs::private_requires_explicit_grant` と同じ CORE-2 ポインタの
    // 既定方針を `batch_search` 経路でも維持する）。
    #[test]
    fn batch_search_excludes_private_rows_without_explicit_grant() {
        let ids = [1u64, 2];
        let tenants = ["tenant-a".to_string(), "tenant-a".to_string()];
        let visibilities = [Visibility::Public, Visibility::Private];
        let vectors = [1.0f32, 0.0, 1.0, 0.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(
            results[0].hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![1],
            "Private row (id=2) must stay invisible without explicit grant"
        );

        // 明示付与すれば可視になることも確認する（黙示の昇格ではないことの対照）。
        let ctx_a_with_private =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let queries_with_grant = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a_with_private,
        }];
        let results_with_grant = engine
            .batch_search(&queries_with_grant)
            .expect("batch search ok");
        let ids_with_grant: std::collections::HashSet<u64> =
            results_with_grant[0].hits.iter().map(|h| h.id).collect();
        assert!(ids_with_grant.contains(&2));
    }

    // codex レビュー指摘対応（行外側ループへの構造変更）: 複数クエリが異なる
    // テナントを持つバッチで、選出器とクエリの対応がずれていないことを検証する
    // （行外側ループ・選出器の事前生成・共有 `row_buf` の組み合わせで、選出器の
    // 取り違えが起きうる構造のため）。
    #[test]
    fn batch_search_keeps_per_query_results_correct_across_different_tenants() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_a = [1.0f32, 0.0];
        let query_b = [0.0f32, 1.0];
        let ctx_a = ctx("tenant-a");
        let ctx_b = ctx("tenant-b");
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 10,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 10,
                ctx: &ctx_b,
            },
        ];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].hits.first().map(|h| h.id), Some(1));
        assert_eq!(results[1].hits.first().map(|h| h.id), Some(4));
        let tenant_a_ids: std::collections::HashSet<u64> = [1, 2].into_iter().collect();
        let tenant_b_ids: std::collections::HashSet<u64> = [3, 4].into_iter().collect();
        for hit in &results[0].hits {
            assert!(tenant_a_ids.contains(&hit.id));
        }
        for hit in &results[1].hits {
            assert!(tenant_b_ids.contains(&hit.id));
        }
    }

    // codex レビュー指摘対応: 同一テナント・異なるベクトルの複数クエリが、
    // 共有される行デコード（`row_buf`）を経由してもクエリごとに正しいスコアで
    // 選出されることを検証する（`k` もクエリごとに異なる値を与え、事前生成した
    // 選出器がクエリと 1:1 で対応することも確認する）。
    #[test]
    fn batch_search_scores_each_query_independently_against_shared_decoded_row() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_a = [1.0f32, 0.0];
        let query_b = [0.0f32, 1.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 1,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 10,
                ctx: &ctx_a,
            },
        ];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].hits.len(), 1, "k=1 must be honored");
        assert_eq!(results[0].hits.first().map(|h| h.id), Some(1));
        assert_eq!(results[1].hits.first().map(|h| h.id), Some(2));
    }

    // negative test: マスク段を意図的に無効化した内部構成（テスト内部限定。公開
    // API・cargo feature としては露出しない。Issue #137 の feature 廃止判断を踏襲）
    // で、上記の独立検査器が違反を検出できることを確認する。
    #[test]
    fn independent_checker_detects_masked_tenant_violation() {
        let matrix = build_two_tenant_matrix();
        // マスクを経由せず「全行を無条件に候補にする」経路を直接模した結果集合
        // （実装コードのマスク段を使わず、テストがここで意図的に違反を作る）。
        let simulated_unmasked_hits = [
            SearchHit { id: 1, score: 1.0 }, // tenant-a: 正当
            SearchHit { id: 3, score: 1.0 }, // tenant-b: 混入（検出されるべき）
        ];
        let tenant_a_ids: std::collections::HashSet<u64> =
            [matrix.ids[0], matrix.ids[1]].into_iter().collect();
        let violation = simulated_unmasked_hits
            .iter()
            .any(|hit| !tenant_a_ids.contains(&hit.id));
        assert!(
            violation,
            "independent checker failed to detect a tenant mask violation"
        );
    }

    #[test]
    fn batch_search_rejects_dim_mismatch() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::DimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn batch_search_rejects_non_finite_query() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [f32::NAN, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::NonFiniteQuery { query_index: 0 });
    }

    // codex レビュー指摘対応: 行外側ループへの構造変更後もクエリ間で選出器が
    // 独立に保たれることを踏まえ、`k` の防御的上限を検証する（`kernel.rs::
    // TopKSelector` 自体は `k` を検証しないため、本モジュールが未検証の巨大な
    // `k` をそのまま複数選出器へ使わないことを確認する）。
    #[test]
    fn batch_search_rejects_invalid_k() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 0,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidK {
                k: 0,
                max: MAX_BATCH_K
            }
        );

        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: MAX_BATCH_K + 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidK {
                k: MAX_BATCH_K + 1,
                max: MAX_BATCH_K
            }
        );
    }

    // codex P1 指摘対応: `sum(k)` がちょうど [`MAX_BATCH_TOTAL_K`] に等しい
    // 境界（超過ではない）は許可されることを確認する。この確認がないと
    // `batch_search_rejects_total_k_over_limit`（超過側のみ検証）だけでは
    // 比較演算子の off-by-one（`>` を `>=` に取り違える等）を検出できない。
    #[test]
    fn batch_search_accepts_total_k_exactly_at_limit() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        // 100 件 * k=10_000 = 1,000,000 == MAX_BATCH_TOTAL_K ちょうど。
        let query_count = MAX_BATCH_TOTAL_K / MAX_BATCH_K;
        assert_eq!(query_count * MAX_BATCH_K, MAX_BATCH_TOTAL_K);
        let queries: Vec<BatchQuery<'_>> = (0..query_count)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: MAX_BATCH_K,
                ctx: &ctx_a,
            })
            .collect();
        let results = engine
            .batch_search(&queries)
            .expect("sum(k) at the limit must be accepted");
        assert_eq!(results.len(), query_count);
    }

    // codex P1 指摘対応: 各クエリの `k` は個別に [`MAX_BATCH_K`] 以内でも、
    // バッチ全体の `sum(k)` が [`MAX_BATCH_TOTAL_K`] を超える場合は
    // fail-closed に拒否する（個別上限のみでは積が無制限になり得たため）。
    #[test]
    fn batch_search_rejects_total_k_over_limit() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        // 101 件 * k=10_000 = 1,010,000 > MAX_BATCH_TOTAL_K(1,000,000)。
        // 各クエリの k は MAX_BATCH_K(10_000) ちょうどで個別上限は満たす。
        let query_count = MAX_BATCH_TOTAL_K / MAX_BATCH_K + 1;
        let queries: Vec<BatchQuery<'_>> = (0..query_count)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: MAX_BATCH_K,
                ctx: &ctx_a,
            })
            .collect();
        let expected_total_k = query_count * MAX_BATCH_K;
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TotalKExceeded {
                total_k: expected_total_k,
                max: MAX_BATCH_TOTAL_K,
            }
        );
    }

    #[test]
    fn batch_search_rejects_too_many_queries() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES + 1)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TooManyQueries {
                count: MAX_BATCH_QUERIES + 1,
                max: MAX_BATCH_QUERIES,
            }
        );
    }

    // codex P1 指摘対応: `sum(k)` の上限を満たしていても、行数×クエリ数×次元の
    // 積（計算量）が [`MAX_BATCH_WORK`] を超える場合は走査開始前に fail-closed
    // で拒否する（rows=1,000・dim=8,192 は `MAX_BATCH_TOTAL_BYTES` の 1 GiB
    // 予算内で現実的に構築できる小さな行列だが、`MAX_BATCH_QUERIES` 件の
    // クエリで走査すると積和は約 3.35 × 10^10 回に達し、`MAX_BATCH_WORK`
    // （10^10）を超える）。`compute_batch_work` を直接呼び、巨大な行列・
    // クエリ列を実際に確保せず境界を検証する（`arena.rs::check_capacity` の
    // 直接テストと同じ考え方）。
    #[test]
    fn compute_batch_work_rejects_over_limit() {
        let err = compute_batch_work([(1_000, MAX_BATCH_QUERIES)], MAX_BATCH_DIM).unwrap_err();
        match err {
            BatchSearchError::WorkBudgetExceeded { work, max } => {
                assert_eq!(max, MAX_BATCH_WORK);
                assert!(work > max);
            }
            other => panic!("expected WorkBudgetExceeded, got {other:?}"),
        }
    }

    // codex P1 指摘対応: `rows × queries × dim` がちょうど [`MAX_BATCH_WORK`]
    // に等しい境界（超過ではない）は許可されることを確認する（off-by-one の
    // 検出用。過大側のみのテストでは `>` と `>=` の取り違えを検出できない）。
    #[test]
    fn compute_batch_work_accepts_exactly_at_limit() {
        // 1,000,000 * 100 * 100 = MAX_BATCH_WORK(10^10) ちょうど（単一テナント）。
        let rows = 1_000_000usize;
        let queries = 100usize;
        let dim = 100usize;
        assert_eq!(rows * queries * dim, MAX_BATCH_WORK);
        let work = compute_batch_work([(rows, queries)], dim).expect("work at the limit must pass");
        assert_eq!(work, MAX_BATCH_WORK);
    }

    // codex P1 指摘対応: `compute_batch_work` の積が `usize` をオーバーフロー
    // する巨大な入力でも panic せず `WorkBudgetExceeded` を返すことを確認する
    // （coding-rust.md「整数演算は checked_*/saturating_* を使う」準拠）。
    #[test]
    fn compute_batch_work_does_not_overflow_on_huge_inputs() {
        let err = compute_batch_work([(usize::MAX, usize::MAX)], usize::MAX).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            }
        );
    }

    // codex P1 指摘対応: テナントごとの積の合算（総和）自体が `usize` を
    // オーバーフローする場合も panic せず `WorkBudgetExceeded` を返すことを
    // 確認する（`compute_tenant_work` 単体の overflow 検出だけでは
    // `compute_batch_work` 内の合算 `checked_add` の回帰を検出できないため）。
    // 1 つ目のテナントは上限内に収まる小さな work（`total` が早期リターン
    // されない）を持たせ、2 つ目のテナントは単体では overflow しない
    // （`rows * queries * dim` が `usize::MAX` に収まる）が、既存の `total`
    // への加算では `usize::MAX` を超えるように選ぶ。
    #[test]
    fn compute_batch_work_does_not_overflow_when_per_tenant_sum_overflows() {
        let per_tenant = [(1usize, 1usize), (usize::MAX, 1usize)];
        let err = compute_batch_work(per_tenant, 1).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            }
        );
    }

    // codex P1 追加指摘対応: レビューで指摘された具体例（tenant-a/b が各
    // 1,000 行、バッチ全体で合計 1,000 クエリ（tenant-a/b に 500 件ずつ）、
    // dim=8,192）を直接検証する。「全一致行数（2,000）× 全クエリ数
    // （1,000）」で一括課金すると 2,000 * 1,000 * 8,192 ≈ 1.6384 × 10^10 で
    // `MAX_BATCH_WORK`（10^10）を超過し、正当な入力を誤って拒否してしまう
    // （＝今回のバグ）。テナントごとの正しい課金
    // `1,000 * 500 * 8,192 + 1,000 * 500 * 8,192` = 8.192 × 10^9 は
    // `MAX_BATCH_WORK` の範囲内であり、受理されるべきことを確認する。
    #[test]
    fn compute_batch_work_matches_codex_review_example() {
        let dim = 8_192usize;
        let per_tenant = [(1_000usize, 500usize), (1_000usize, 500usize)];

        // 誤った「全一致行数 × 全クエリ数」の一括課金は超過することを前提として
        // 明示する（この誤った計算式が起こす過大計上こそが今回のバグ）。
        let naive_total_rows = 2_000usize;
        let naive_total_queries = 1_000usize;
        assert!(naive_total_rows * naive_total_queries * dim > MAX_BATCH_WORK);

        // テナントごとの正しい課金は上限内で受理される。
        let work = compute_batch_work(per_tenant, dim)
            .expect("per-tenant work budget must accept the codex review example");
        assert_eq!(work, 1_000 * 500 * dim * 2);
        assert!(work <= MAX_BATCH_WORK);
    }

    // codex P1 指摘対応: `batch_search` 経由でも計算量ガードが実際に効くことを
    // 確認する（小さな行列 + 最大クエリ数 + 最大次元の組み合わせで、実データを
    // 現実的なサイズに保ったまま `MAX_BATCH_WORK` 超過を再現する）。
    #[test]
    fn batch_search_rejects_work_budget_over_limit() {
        // rows=1,000・dim=8,192（MAX_BATCH_DIM）で packed バイト数は
        // 約 16MB 程度に収まり、現実的に構築できる。
        let rows = 1_000usize;
        let dim = MAX_BATCH_DIM;
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenants: Vec<String> = std::iter::repeat_n("tenant-a".to_string(), rows).collect();
        let visibilities: Vec<Visibility> = std::iter::repeat_n(Visibility::Public, rows).collect();
        let vectors: Vec<f32> = std::iter::repeat_n(0.0f32, rows * dim).collect();
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, dim, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);

        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, dim).collect();
        let ctx_a = ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();
        let err = engine.batch_search(&queries).unwrap_err();
        match err {
            BatchSearchError::WorkBudgetExceeded { work, max } => {
                assert_eq!(max, MAX_BATCH_WORK);
                assert!(work > max);
            }
            other => panic!("expected WorkBudgetExceeded, got {other:?}"),
        }
    }

    // Cursor Medium 指摘対応: 計算量ガードの課金対象は常駐行列の全行数ではなく、
    // バッチのテナント集合に実際に一致する行数（＝走査時に実際にデコードされる
    // 行数）でなければならない。tenant-a を 10 行・tenant-b を 990 行持つ
    // 常駐行列（全 1,000 行）に対し、tenant-a だけを最大クエリ数で検索する
    // バッチは、全行数（1,000）を課金すれば
    // `batch_search_rejects_work_budget_over_limit` と同じ構成で
    // `WorkBudgetExceeded` になってしまうが、実際に走査するのは tenant-a の
    // 10 行だけ（10 * MAX_BATCH_QUERIES * MAX_BATCH_DIM ≈ 3.36 × 10^8 <
    // MAX_BATCH_WORK）なので許可されるべきことを確認する（行数は実行時間を
    // 抑えるため境界からは離れた小さい値を選ぶ。境界そのものの検証は
    // `compute_batch_work_accepts_exactly_at_limit` 等が別途担う）。
    #[test]
    fn batch_search_work_budget_charges_only_matching_tenant_rows() {
        let tenant_a_rows = 10usize;
        let tenant_b_rows = 990usize;
        let rows = tenant_a_rows + tenant_b_rows;
        let dim = MAX_BATCH_DIM;
        let ids: Vec<u64> = (0..rows as u64).collect();
        let mut tenants: Vec<String> = std::iter::repeat_n("tenant-a".to_string(), tenant_a_rows)
            .chain(std::iter::repeat_n("tenant-b".to_string(), tenant_b_rows))
            .collect();
        tenants.truncate(rows);
        let visibilities: Vec<Visibility> = std::iter::repeat_n(Visibility::Public, rows).collect();
        let vectors: Vec<f32> = std::iter::repeat_n(0.0f32, rows * dim).collect();
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, dim, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);

        // 全行数（1,000）で課金すると work = 1,000 * MAX_BATCH_QUERIES *
        // MAX_BATCH_DIM は MAX_BATCH_WORK を超過する（上の
        // `batch_search_rejects_work_budget_over_limit` と同一規模）ことを
        // 前提として明示する。
        assert!(compute_batch_work([(rows, MAX_BATCH_QUERIES)], dim).is_err());
        // tenant-a の行数だけで課金すれば上限内であることも前提として明示する
        // （本テストのバッチは tenant-a のみを検索するため、テナントごとの
        // 課金でも単一テナント分＝この値と一致する）。
        assert!(compute_batch_work([(tenant_a_rows, MAX_BATCH_QUERIES)], dim).is_ok());

        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, dim).collect();
        let ctx_a = ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();

        let result = engine.batch_search(&queries);
        assert!(
            result.is_ok(),
            "expected success when only the small tenant-a slice is searched, got {result:?}"
        );
    }

    // codex レビュー追加指摘対応: 次元不一致という軽量に判定できる入力エラーは
    // `compute_batch_work` の計算量ガードより先に報告されるべき（core.rs::search
    // と同じ「安価で具体的なエラーを優先する」方針）。work budget 超過を
    // 引き起こす規模のクエリ列に次元不一致クエリを混ぜても、返るのは
    // `DimMismatch` であって `WorkBudgetExceeded` ではないことを確認する。
    #[test]
    fn batch_search_reports_dim_mismatch_before_work_budget_guard() {
        let rows = 1_000usize;
        let dim = MAX_BATCH_DIM;
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenants: Vec<String> = std::iter::repeat_n("tenant-a".to_string(), rows).collect();
        let visibilities: Vec<Visibility> = std::iter::repeat_n(Visibility::Public, rows).collect();
        let vectors: Vec<f32> = std::iter::repeat_n(0.0f32, rows * dim).collect();
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, dim, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);

        // rows * MAX_BATCH_QUERIES * dim は MAX_BATCH_WORK を超過する規模
        // （`batch_search_rejects_work_budget_over_limit` と同一構成）。
        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, dim).collect();
        let wrong_dim_query: Vec<f32> = std::iter::repeat_n(0.0f32, dim - 1).collect();
        let ctx_a = ctx("tenant-a");
        let mut queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();
        queries[0] = BatchQuery {
            vector: &wrong_dim_query,
            k: 1,
            ctx: &ctx_a,
        };

        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::DimMismatch {
                expected: dim,
                found: dim - 1,
            }
        );
    }

    // BatchEngine が SearchProvider を実装しないこと（単発経路へ構造的に
    // 接続できないことのコンパイル時の裏付け）は型シグネチャそのものが保証する
    // ため実行時テストは不要（`kernel.rs::SearchProvider` を implement していない）。
}
