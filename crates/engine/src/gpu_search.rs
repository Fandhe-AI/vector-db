//! バッチクエリ・一括インデクシング専用の検索エンジン（TASK-128・対象ビヘイビア:
//! CORE-6, CORE-7, CORE-16）。`kernel.rs::SearchProvider`（単発クエリの窓口・CORE-13）
//! は実装しない。単発経路（`core.rs::EngineCore::search`）は本モジュールへ構造的に
//! 接続できず、`CORE-7` が定める「単発クエリは CPU-SIMD 固定」を型レベルで担保する。
//!
//! `Gpu` を冠する型名・モジュール名だが、現段階は CPU 上で GPU 常駐相当の
//! アルゴリズム的挙動を再現する **CPU リファレンス実装**である。実際の GPU
//! 実行（`wgpu`）は未導入で、下記「依存に関する制約」節が理由と導入条件を示す
//! （TASK-128 参照。命名は将来 `wgpu` 実行へ差し替える際の API 破壊を避けるため
//! 維持する）。
//!
//! # 依存に関する制約（重要・.claude/rules/dependency-policy.md）
//!
//! 元の設計は GPU 常駐ベース行列（`wgpu`/WGSL、CORE-16 の f16 2 要素/u32 パックを
//! `unpack2x16float` で復元）でのバッチスコア計算を行う想定だった。しかし本リポの
//! 依存追加は「必ずユーザーの明示承認を経てから行う」規約（dependency-policy.md）
//! であり、本コミットの実行過程ではその承認を得られない。安全側に倒し、実際の GPU
//! 実行（`wgpu` 依存の追加・デバイス初期化・WGSL コンパイル）は追加せず、CORE-6/7/16
//! が要求する **アルゴリズム的な挙動**（GPU 常駐相当の f16 パック表現・バッチ Top-k
//! 選出・クエリ別テナントマスク・動的窓集約）のみを依存クレートなしで実装する。
//! 実際の GPU（`wgpu`）実行への置き換えは、依存追加の承認後にフォローアップとして
//! 行う（このコミットの outOfScope・PR 本文に明記する）。
//!
//! `TopKSelector`（`kernel.rs`）を選出段で共用し、選出規約（スコア降順・同点 id
//! 昇順・非有限値除外）の二重管理を防ぐ。`core.rs::EngineCore` と同じ「(1) 選出前に
//! 候補をマスクで絞る、(2) 選出後の結果 id を独立に再検証する」二重防御を踏襲する
//! （CORE-7 のテナント境界要件）。

use std::fmt;

use crate::kernel::{SearchHit, TopKSelector};

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

/// バッチ検索エンジンのエラー（fail-closed）。メッセージは英語（wire プロトコル
/// 互換性・運用ツール連携のため。japanese-style.md 準拠）。他テナントのデータ・
/// 存在情報を含めない。
#[derive(Debug, Clone, PartialEq)]
pub enum GpuSearchError {
    /// バッチのクエリ件数が [`MAX_BATCH_QUERIES`] を超過した。
    TooManyQueries { count: usize, max: usize },
    /// 常駐行列の行数が [`MAX_BATCH_ROWS`] を超過した。
    TooManyRows { count: usize, max: usize },
    /// 次元数が [`MAX_BATCH_DIM`] を超過した、または 0。
    InvalidDim { dim: usize, max: usize },
    /// 行数・次元数はそれぞれ上限内でも、組み合わせた総確保バイト量が
    /// [`MAX_BATCH_TOTAL_BYTES`] を超過した。
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
}

impl fmt::Display for GpuSearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuSearchError::TooManyQueries { count, max } => {
                write!(f, "gpu_search: too many queries: {count} (max {max})")
            }
            GpuSearchError::TooManyRows { count, max } => {
                write!(f, "gpu_search: too many rows: {count} (max {max})")
            }
            GpuSearchError::InvalidDim { dim, max } => {
                write!(f, "gpu_search: invalid dim: {dim} (max {max}, must be > 0)")
            }
            GpuSearchError::CapacityExceeded { total_bytes, max } => write!(
                f,
                "gpu_search: total allocation bytes {total_bytes} exceeds limit {max}"
            ),
            GpuSearchError::DimMismatch { expected, found } => write!(
                f,
                "gpu_search: query dim mismatch: expected={expected} found={found}"
            ),
            GpuSearchError::NonFiniteQuery { query_index } => write!(
                f,
                "gpu_search: query {query_index} contains non-finite value"
            ),
            GpuSearchError::ArenaLengthMismatch => {
                write!(f, "gpu_search: arena ids/vectors/mask length mismatch")
            }
            GpuSearchError::DuplicateRowId => {
                write!(f, "gpu_search: resident matrix contains duplicate row ids")
            }
            GpuSearchError::TenantIdTooLong { len, max } => {
                write!(f, "gpu_search: tenant_id length {len} exceeds limit {max}")
            }
            GpuSearchError::InvalidK { k, max } => {
                write!(f, "gpu_search: invalid k: {k} (must be 1..={max})")
            }
            GpuSearchError::AllocationFailed(msg) => {
                write!(f, "gpu_search: allocation failed: {msg}")
            }
            GpuSearchError::TenantMaskViolation => {
                write!(f, "gpu_search: result violated per-query tenant mask")
            }
        }
    }
}

impl std::error::Error for GpuSearchError {}

// ---------------------------------------------------------------------
// CORE-16: GPU 常駐コピー限定の f16 2 要素/u32 パック。
//
// 格納・CPU 経路・クエリベクトルの dtype は f32 のまま変更しない。本関数群は
// 「GPU 常駐ベース行列」相当の表現へ変換する層としてのみ使う（呼び出し元が
// f32 のオリジナルを保持し続ける前提）。IEEE 754 binary16 の丸めは round-to-nearest-even
// を実装する。
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

/// 2 要素の f32 を 1 個の u32 へ f16 パックする（CORE-16: GPU 常駐コピーの表現）。
/// 下位 16 ビットに `a`、上位 16 ビットに `b` を格納する（WGSL `unpack2x16float` の
/// 並び順と一致させる）。
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
// GPU 常駐ベース行列（CORE-16 のパック表現を保持する一括インデクシング結果）。
// ---------------------------------------------------------------------

/// `Vec::try_reserve_exact` の失敗を [`GpuSearchError::AllocationFailed`] へ変換する
/// 共通ヘルパー（[`ResidentMatrix::build`] 専用。`arena.rs` の同名ヘルパーと同方針
/// だが `pub(crate)` ではないため個別に持つ）。`try_reserve`（amortized 成長）では
/// なく `try_reserve_exact`（要求量ちょうど）を使うのは、呼び出し元が
/// アロケーション前に検証済みの論理必要量どおりに実確保量を抑えるため
/// （`arena.rs::try_reserve_exact` と同じ理由）。
fn try_reserve_exact<T>(
    buf: &mut Vec<T>,
    additional: usize,
    what: &str,
) -> Result<(), GpuSearchError> {
    buf.try_reserve_exact(additional)
        .map_err(|e| GpuSearchError::AllocationFailed(format!("failed to reserve {what}: {e}")))
}

/// 一括インデクシングで構築する GPU 常駐相当のベース行列。可視性フィルタ済みの
/// 全行を f16 2 要素/u32 パックで保持する。呼び出し元（`core.rs` 相当）は元の f32
/// アリーナを別途保持し続け、本構造体はバッチ検索専用の副次表現として扱う。
/// 現段階は CPU 上でこの表現を保持・走査する CPU リファレンス実装であり、
/// 実際の GPU 常駐（`wgpu` バッファ）ではない（モジュール冒頭コメント参照）。
#[derive(Debug)]
pub struct ResidentMatrix {
    ids: Vec<u64>,
    /// テナント境界判定用: 行 `i` の所属テナント ID（`PolicyContext::is_visible` の
    /// 単一照合パスに渡すため、呼び出し元が構築時に確定させる）。
    tenant_ids: Vec<String>,
    dim: usize,
    /// `ids.len() * dim.div_ceil(2)` 要素のパック済み行列（行優先）。
    packed: Vec<u32>,
}

impl ResidentMatrix {
    /// 可視性フィルタ済みの行集合から常駐行列を構築する（CORE-16）。`ids` /
    /// `tenant_ids` / `vectors`（`ids.len() * dim` 要素、f32・行優先）の長さが
    /// 整合しない場合は [`GpuSearchError::ArenaLengthMismatch`]。次元・行数の
    /// 上限検証はアロケーション前に行う（untrusted な行数・次元をそのまま
    /// `Vec::with_capacity` へ渡さない。coding-rust.md 準拠）。
    pub fn build(
        ids: &[u64],
        tenant_ids: &[String],
        dim: usize,
        vectors: &[f32],
    ) -> Result<Self, GpuSearchError> {
        if dim == 0 || dim > MAX_BATCH_DIM {
            return Err(GpuSearchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            });
        }
        if ids.len() > MAX_BATCH_ROWS {
            return Err(GpuSearchError::TooManyRows {
                count: ids.len(),
                max: MAX_BATCH_ROWS,
            });
        }
        if ids.len() != tenant_ids.len() {
            return Err(GpuSearchError::ArenaLengthMismatch);
        }

        let packed_per_row = dim.div_ceil(2);
        let packed_len =
            ids.len()
                .checked_mul(packed_per_row)
                .ok_or(GpuSearchError::TooManyRows {
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
            GpuSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            },
        )?;
        let per_row_aux_bytes = std::mem::size_of::<u64>()
            .checked_add(std::mem::size_of::<String>())
            .and_then(|v| v.checked_add(crate::storage::MAX_TENANT_ID_LEN as usize))
            .ok_or(GpuSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            })?;
        let aux_bytes =
            ids.len()
                .checked_mul(per_row_aux_bytes)
                .ok_or(GpuSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_TOTAL_BYTES,
                })?;
        let total_bytes =
            packed_bytes
                .checked_add(aux_bytes)
                .ok_or(GpuSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_TOTAL_BYTES,
                })?;
        if total_bytes > MAX_BATCH_TOTAL_BYTES {
            return Err(GpuSearchError::CapacityExceeded {
                total_bytes,
                max: MAX_BATCH_TOTAL_BYTES,
            });
        }

        let expected_len = ids
            .len()
            .checked_mul(dim)
            .ok_or(GpuSearchError::ArenaLengthMismatch)?;
        if vectors.len() != expected_len {
            return Err(GpuSearchError::ArenaLengthMismatch);
        }

        // id の一意性を検証する（容量・長さチェックより後に置き、cheap な拒否経路
        // （`CapacityExceeded`・`ArenaLengthMismatch`）を先に済ませてから、
        // 検証コスト・確保量が `ids.len()` に比例する本チェックへ進む）。重複を
        // 許すと id → tenant が一意に定まらず、`GpuBatchEngine::batch_search` の
        // 選出後独立再検証（id ベース）が異なるテナントの行を取り違えうる
        // （codex レビュー指摘対応）。
        // `HashSet::with_capacity` は失敗時に abort するため使わず、
        // `try_reserve`（フォールブル）で確保する（Cursor Bugbot 指摘対応）。
        let mut seen_ids = std::collections::HashSet::new();
        seen_ids.try_reserve(ids.len()).map_err(|e| {
            GpuSearchError::AllocationFailed(format!("failed to reserve id set: {e}"))
        })?;
        for &id in ids {
            if !seen_ids.insert(id) {
                return Err(GpuSearchError::DuplicateRowId);
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
                return Err(GpuSearchError::TenantIdTooLong {
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
                GpuSearchError::AllocationFailed(format!("failed to reserve tenant_id: {e}"))
            })?;
            owned.push_str(tenant);
            owned_tenant_ids.push(owned);
        }

        Ok(Self {
            ids: owned_ids,
            tenant_ids: owned_tenant_ids,
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
    /// 呼び出し元が使い回すことで、`GpuBatchEngine::batch_search` がクエリ数
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
// CORE-7: 動的窓集約。キュー取り出し時に後続クエリが到着している場合に限り
// 短時間窓で集約して GPU バッチへ載せる。静的窓（常時窓待ち）は実装しない。
// ---------------------------------------------------------------------

/// 集約するか否かの判定（副作用なしの純関数）。`pending_after_pop` はキューから
/// 1 件取り出した直後に後続が存在するかどうか（呼び出し側のキュー実装に依存
/// しない最小のインターフェース）。後続が無ければ即時 CPU-SIMD 経路へ回すべき
/// （待たない）ため `false`。ディスパッチ決定表本体（TASK-155）はこの判定を
/// 呼び出す側であり、本関数自体は経路選択ロジックを持たない。
pub fn should_aggregate_into_batch(pending_after_pop: bool) -> bool {
    pending_after_pop
}

/// 動的窓での集約バッファ。[`should_aggregate_into_batch`] が `true` を返した
/// キュー取り出し文脈でのみ使う想定（PoC-13 移植）。
#[derive(Debug, Default)]
pub struct DynamicWindowAggregator {
    queries: Vec<Vec<f32>>,
}

impl DynamicWindowAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// クエリ 1 件を窓へ追加する。上限は呼び出し側（キュー実装）が
    /// [`MAX_BATCH_QUERIES`] を尊重して制御する前提（本構造体自体は無制限に
    /// 追加を受け付けるため、呼び出し側の責務として明記する）。
    pub fn push(&mut self, query: Vec<f32>) {
        self.queries.push(query);
    }

    pub fn len(&self) -> usize {
        self.queries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    /// 窓を確定し、集約済みクエリ集合を取り出す（窓は 1 回使い切り）。
    pub fn drain(&mut self) -> Vec<Vec<f32>> {
        std::mem::take(&mut self.queries)
    }
}

// ---------------------------------------------------------------------
// CORE-6/7: バッチ検索本体。GPU スコア計算に相当する積和は f16 往復した行から
// f32 で行う（実際の WGSL 実行は追加しない。本モジュール冒頭コメント参照）。
// Top-k 抽出は `kernel.rs::TopKSelector` を共用する。
// ---------------------------------------------------------------------

/// クエリ 1 件分のバッチ入力。可視テナント集合はクエリ発行元の
/// `PolicyContext::tenant_id()` から呼び出し元が確定させ、ここでは単純な
/// 文字列一致でマスクする（`policy.rs::PolicyContext::is_visible` と同一の
/// 単一照合パスの考え方を踏襲。本モジュールは `PolicyContext` を直接構築
/// しないため文字列一致で表現する）。
pub struct BatchQuery<'a> {
    pub vector: &'a [f32],
    pub k: usize,
    pub tenant_id: &'a str,
}

/// `GpuBatchEngine::batch_search` の 1 クエリ分の結果。
#[derive(Debug)]
pub struct BatchHit {
    pub hits: Vec<SearchHit>,
}

/// バッチクエリ・一括インデクシング専用のエンジン（CORE-6）。単発クエリ経路
/// (`kernel.rs::SearchProvider`) は実装せず、`core.rs::EngineCore` から到達
/// できない（型レベルでの分離。CORE-7）。現段階はスコア計算を CPU で行う
/// リファレンス実装であり、GPU ディスパッチは行わない（モジュール冒頭コメント
/// 参照。[`GpuBatchEngine::new`] のドキュメンテーションコメントに導入条件を示す）。
pub struct GpuBatchEngine {
    matrix: ResidentMatrix,
}

impl GpuBatchEngine {
    /// 常駐行列から構築する。GPU デバイス初期化は行わない（依存制約により
    /// 実際の GPU 実行は追加していないため、本コンストラクタは常に成功する。
    /// 将来 `wgpu` 依存が承認された場合、ここで adapter/device 取得を行い
    /// 初期化失敗を `Result::Err` として fail-closed に伝播する設計とする）。
    pub fn new(matrix: ResidentMatrix) -> Self {
        Self { matrix }
    }

    /// バッチ検索を実行する（CORE-6・CORE-7）。クエリごとに次元・非有限値・`k` を
    /// 検証したうえで、常駐行列の中からクエリと同一テナントの行だけを候補として
    /// 選出する（Top-k 選出段でのクエリ別可視行マスク）。選出後、結果 id を
    /// 独立に再計算したテナント集合と突き合わせ、逸脱があれば結果を一切返さず
    /// [`GpuSearchError::TenantMaskViolation`] を返す（`core.rs::EngineCore` と
    /// 同じ二重防御。fail-closed）。
    ///
    /// 行列走査はクエリ外側ではなく行外側でループする（codex レビュー指摘対応:
    /// 旧実装はクエリループの内側で行を毎回 f16→f32 デコードしており、
    /// クエリ数×行数×dim のデコード・ヒープ確保が発生していた）。行 1 件を
    /// 1 回だけデコードし、そのバッチに含まれるテナントと一致する全クエリへ
    /// 使い回す。選出結果の各クエリ内順序（スコア降順・同点 id 昇順）は
    /// `TopKSelector` が保証するため、走査順序の変更による結果の変化はない。
    pub fn batch_search(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<BatchHit>, GpuSearchError> {
        if queries.len() > MAX_BATCH_QUERIES {
            return Err(GpuSearchError::TooManyQueries {
                count: queries.len(),
                max: MAX_BATCH_QUERIES,
            });
        }

        // 事前検証パス: 次元・非有限値・k をクエリごとに検証し、選出器を用意する。
        // 選出器コンテナ（`Vec<TopKSelector>`）は `Vec::with_capacity`
        // （失敗時に abort する内部確保）ではなく `try_reserve_exact` で
        // フォールブルに確保する（codex P1 指摘対応。`ResidentMatrix::build`
        // 用に定義済みの共通ヘルパーを再利用）。
        let mut selectors: Vec<TopKSelector> = Vec::new();
        try_reserve_exact(&mut selectors, queries.len(), "selectors")?;
        for (query_index, q) in queries.iter().enumerate() {
            if q.vector.len() != self.matrix.dim {
                return Err(GpuSearchError::DimMismatch {
                    expected: self.matrix.dim,
                    found: q.vector.len(),
                });
            }
            if q.vector.iter().any(|v| !v.is_finite()) {
                return Err(GpuSearchError::NonFiniteQuery { query_index });
            }
            if q.k == 0 || q.k > MAX_BATCH_K {
                return Err(GpuSearchError::InvalidK {
                    k: q.k,
                    max: MAX_BATCH_K,
                });
            }
            selectors.push(TopKSelector::new(q.k));
        }

        // このバッチに登場するテナント集合（`HashSet` にして行外側ループから
        // O(1) で参照できるようにする。バッチのクエリ件数は [`MAX_BATCH_QUERIES`]
        // で上限検証済みのため、集合サイズもそれに従う）。
        let batch_tenants: std::collections::HashSet<&str> =
            queries.iter().map(|q| q.tenant_id).collect();

        // id → tenant の逆引き表（選出後の独立再検証用）。`ResidentMatrix::build`
        // が id の重複を拒否しているため、id は tenant を一意に決める
        // （[`GpuSearchError::DuplicateRowId`] 参照）。選出段のマスク実装（行 index
        // からの文字列比較）とは別経路でこの表を組むことで、二重防御を維持する。
        // このバッチのテナントに属さない行は登録しない（マップを
        // `MAX_BATCH_ROWS` 全件分確保しないための最適化であると同時に、
        // バッチ外テナントの id が万一 hit に混入した場合を確実に
        // マップ不在 → `TenantMaskViolation` にする fail-closed 側の効果も持つ）。
        let mut id_to_tenant: std::collections::HashMap<u64, &str> =
            std::collections::HashMap::new();
        for (id, tenant) in self.matrix.ids.iter().zip(self.matrix.tenant_ids.iter()) {
            if batch_tenants.contains(tenant.as_str()) {
                id_to_tenant.insert(*id, tenant.as_str());
            }
        }

        // 行外側ループ: 行 1 件につき 1 回だけデコードし、一致する全クエリへ使う。
        let mut row_buf: Vec<f32> = Vec::with_capacity(self.matrix.dim);
        for row_idx in 0..self.matrix.row_count() {
            let Some(row_tenant) = self.matrix.tenant_ids.get(row_idx).map(String::as_str) else {
                continue;
            };
            // このバッチ内に同一テナントのクエリが 1 件も無ければデコードを省く。
            if !batch_tenants.contains(row_tenant) {
                continue;
            }
            let Some(id) = self.matrix.ids.get(row_idx).copied() else {
                continue;
            };
            if self.matrix.row_f32_into(row_idx, &mut row_buf).is_none() {
                continue;
            }

            for (q, selector) in queries.iter().zip(selectors.iter_mut()) {
                // (1) 選出前のマスク: 自テナントの行だけを候補にする。
                if q.tenant_id != row_tenant {
                    continue;
                }
                let score = crate::kernel::dot(&row_buf, q.vector);
                if !score.is_finite() {
                    continue;
                }
                selector.push(SearchHit { id, score });
            }
        }

        let mut out = Vec::with_capacity(queries.len());
        for (q, selector) in queries.iter().zip(selectors) {
            let hits = selector.into_sorted_vec();

            // (2) 選出後の独立再検証: 返す id が全て自テナント行由来であることを、
            // マスク実装（行 index からの文字列比較）から独立に id → tenant の
            // 逆引き表で確認する。
            for hit in &hits {
                match id_to_tenant.get(&hit.id) {
                    Some(&t) if t == q.tenant_id => {}
                    _ => return Err(GpuSearchError::TenantMaskViolation),
                }
            }

            out.push(BatchHit { hits });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CORE-16: f16 往復精度（GPU 不要・常時実行）。
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

    // CORE-16 codex レビュー指摘対応: サブノーマル f16 の復元値を厳密一致で検証する
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

    // CORE-7: 動的窓集約の判定（後続あり/なし・静的窓不在）。
    #[test]
    fn should_aggregate_only_when_pending_follows() {
        assert!(should_aggregate_into_batch(true));
        assert!(!should_aggregate_into_batch(false));
    }

    #[test]
    fn aggregator_drains_pushed_queries_once() {
        let mut agg = DynamicWindowAggregator::new();
        assert!(agg.is_empty());
        agg.push(vec![1.0, 0.0]);
        agg.push(vec![0.0, 1.0]);
        assert_eq!(agg.len(), 2);
        let drained = agg.drain();
        assert_eq!(drained.len(), 2);
        assert!(agg.is_empty());
    }

    // Cursor Bugbot 指摘対応: 上限検証済みの量を超えて `try_reserve_exact` が
    // 実際にメモリ不足になった場合、`Vec::with_capacity`/`push` のように abort
    // せず `Err(GpuSearchError::AllocationFailed)` を返すことを検証する
    // （`arena.rs` の同種テストと同方針）。`isize::MAX` 超のレイアウトは Rust の
    // アロケーション API 契約上、実メモリを確保しようとする前に即座に拒否
    // されるため、CI 環境で実際に大量のメモリを消費せず決定的に再現できる。
    #[test]
    fn try_reserve_exact_converts_oversized_request_to_allocation_failed_without_aborting() {
        let mut buf: Vec<u8> = Vec::new();
        let oversized = (isize::MAX as usize).saturating_add(1);
        let result = try_reserve_exact(&mut buf, oversized, "test buffer");
        assert!(matches!(result, Err(GpuSearchError::AllocationFailed(_))));
    }

    // ResidentMatrix の上限・整合性検証（untrusted 入力の防御的上限）。
    #[test]
    fn resident_matrix_rejects_zero_dim() {
        let err = ResidentMatrix::build(&[1], &["t".to_string()], 0, &[]).unwrap_err();
        assert_eq!(
            err,
            GpuSearchError::InvalidDim {
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
        let err = ResidentMatrix::build(&ids, &tenants, MAX_BATCH_DIM, &[]).unwrap_err();
        match err {
            GpuSearchError::CapacityExceeded { total_bytes, max } => {
                assert_eq!(max, MAX_BATCH_TOTAL_BYTES);
                assert!(total_bytes > max);
            }
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }
    }

    #[test]
    fn resident_matrix_rejects_length_mismatch() {
        let err = ResidentMatrix::build(&[1, 2], &["t".to_string()], 2, &[1.0, 0.0]).unwrap_err();
        assert_eq!(err, GpuSearchError::ArenaLengthMismatch);
    }

    // codex レビュー指摘対応: id 重複は fail-closed で拒否する（重複を許すと
    // id → tenant が一意に定まらず、`batch_search` の選出後独立再検証（id ベース）
    // が別テナントの行を取り違えうるため）。
    #[test]
    fn resident_matrix_rejects_duplicate_ids() {
        let ids = [1u64, 1];
        let tenants = ["tenant-a".to_string(), "tenant-b".to_string()];
        let vectors = [1.0f32, 0.0, 0.0, 1.0];
        let err = ResidentMatrix::build(&ids, &tenants, 2, &vectors).unwrap_err();
        assert_eq!(err, GpuSearchError::DuplicateRowId);
    }

    // Cursor Bugbot 指摘対応: 容量チェックは `MAX_TENANT_ID_LEN` で予算計上する
    // だけで、実際の各 `tenant_ids` 要素長を検証していなかった。上限超過の
    // tenant_id を含む入力を fail-closed で拒否することを確認する。
    #[test]
    fn resident_matrix_rejects_oversized_tenant_id() {
        let ids = [1u64];
        let oversized = "t".repeat(crate::storage::MAX_TENANT_ID_LEN as usize + 1);
        let tenants = [oversized.clone()];
        let vectors = [1.0f32, 0.0];
        let err = ResidentMatrix::build(&ids, &tenants, 2, &vectors).unwrap_err();
        assert_eq!(
            err,
            GpuSearchError::TenantIdTooLong {
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
        let vectors = [1.0f32, 0.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, 2, &vectors).expect("valid matrix");
        let engine = GpuBatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            tenant_id: &multibyte_tenant,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(
            results[0].hits.first().map(|h| h.id),
            Some(1),
            "tenant_id must round-trip byte-for-byte through the fallible clone"
        );
    }

    fn build_two_tenant_matrix() -> ResidentMatrix {
        // tenant-a: id=1,2 / tenant-b: id=3,4。dim=2。
        let ids = vec![1u64, 2, 3, 4];
        let tenants = vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ];
        let vectors = vec![1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0];
        ResidentMatrix::build(&ids, &tenants, 2, &vectors).expect("valid matrix")
    }

    // CORE-7 テナント境界（P0）: 混在テナントバッチで混入 0 件。
    // 検査は実装（`batch_search` 内のマスク）から独立に、返却 id → tenant を
    // 再計算して確認する（実装と検査器の経路分離）。
    #[test]
    fn batch_search_excludes_other_tenant_rows() {
        let matrix = build_two_tenant_matrix();
        let engine = GpuBatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            tenant_id: "tenant-a",
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

    // codex レビュー指摘対応（行外側ループへの構造変更）: 複数クエリが異なる
    // テナントを持つバッチで、選出器とクエリの対応がずれていないことを検証する
    // （行外側ループ・選出器の事前生成・共有 `row_buf` の組み合わせで、選出器の
    // 取り違えが起きうる構造のため）。
    #[test]
    fn batch_search_keeps_per_query_results_correct_across_different_tenants() {
        let matrix = build_two_tenant_matrix();
        let engine = GpuBatchEngine::new(matrix);
        let query_a = [1.0f32, 0.0];
        let query_b = [0.0f32, 1.0];
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 10,
                tenant_id: "tenant-a",
            },
            BatchQuery {
                vector: &query_b,
                k: 10,
                tenant_id: "tenant-b",
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
        let engine = GpuBatchEngine::new(matrix);
        let query_a = [1.0f32, 0.0];
        let query_b = [0.0f32, 1.0];
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 1,
                tenant_id: "tenant-a",
            },
            BatchQuery {
                vector: &query_b,
                k: 10,
                tenant_id: "tenant-a",
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
        let engine = GpuBatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0, 0.0];
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            tenant_id: "tenant-a",
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            GpuSearchError::DimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn batch_search_rejects_non_finite_query() {
        let matrix = build_two_tenant_matrix();
        let engine = GpuBatchEngine::new(matrix);
        let query_vec = [f32::NAN, 0.0];
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            tenant_id: "tenant-a",
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, GpuSearchError::NonFiniteQuery { query_index: 0 });
    }

    // codex レビュー指摘対応: 行外側ループへの構造変更後もクエリ間で選出器が
    // 独立に保たれることを踏まえ、`k` の防御的上限を検証する（`kernel.rs::
    // TopKSelector` 自体は `k` を検証しないため、本モジュールが未検証の巨大な
    // `k` をそのまま複数選出器へ使わないことを確認する）。
    #[test]
    fn batch_search_rejects_invalid_k() {
        let matrix = build_two_tenant_matrix();
        let engine = GpuBatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 0,
            tenant_id: "tenant-a",
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            GpuSearchError::InvalidK {
                k: 0,
                max: MAX_BATCH_K
            }
        );

        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: MAX_BATCH_K + 1,
            tenant_id: "tenant-a",
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            GpuSearchError::InvalidK {
                k: MAX_BATCH_K + 1,
                max: MAX_BATCH_K
            }
        );
    }

    #[test]
    fn batch_search_rejects_too_many_queries() {
        let matrix = build_two_tenant_matrix();
        let engine = GpuBatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES + 1)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                tenant_id: "tenant-a",
            })
            .collect();
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            GpuSearchError::TooManyQueries {
                count: MAX_BATCH_QUERIES + 1,
                max: MAX_BATCH_QUERIES,
            }
        );
    }

    // GpuBatchEngine が SearchProvider を実装しないこと（単発経路へ構造的に
    // 接続できないことのコンパイル時の裏付け）は型シグネチャそのものが保証する
    // ため実行時テストは不要（`kernel.rs::SearchProvider` を implement していない）。
}
