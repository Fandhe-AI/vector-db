//! 次元ごと min/max による対称スカラー量子化（SQ8）の共有変換層
//! （Issue #521・親 #520／Phase 4 親 #459。ポインタ: TASK-132・TASK-156・
//! CORE-16）。qdrant `encoded_vectors_u8` と同型の対称量子化を、`f16.rs`
//! （f16 常駐・Issue #514）と同じ「変換ロジックを 1 箇所へ集約し、
//! 常駐表現へのエンコード層としてのみ使う」位置づけで実装する
//! （`docs/design/simd-intrinsics-adoption.md` 決定 3・決定 5）。
//!
//! `hnsw.rs::NodeVectors::I8`（HNSW 索引ノードの i8 常駐表現）が本モジュール
//! の唯一の呼び出し元。索引凍結（`HnswIndex::freeze_from`）時に 1 回だけ
//! [`fit_dim_params`]・[`encode_rows`] を呼び、探索時のスコア計算は
//! [`dequantize`] による復号 dot（コード×スケールを f32 化してから
//! クエリ f32 と内積する）で行う。次元別スケールをノード側にだけ持たせ、
//! クエリ側は量子化せず f32 のまま比較するため、`f16::encode_rows` と同じ
//! 「格納側だけを低精度化する」非対称な設計になる。索引ヒットの最終スコアは
//! 常に `kernel::dot`（f32・アリーナ再計算）を経由するため
//! （`docs/design/simd-intrinsics-adoption.md` 決定 5）、この近似は候補生成段
//! にのみ影響する。整数 i8×i8 dot カーネル（VNNI／NEON dotprod）は本 Issue の
//! 対象外（#522・#524）。
//!
//! `half`／`simsimd` 等の外部クレートは依存最小方針
//! （[dependency-policy](../../../.claude/rules/dependency-policy.md)）
//! により不採用（自作の変換関数のみで完結する）。
//!
//! # 整数 i8×i8 dot（Issue #522・親 #520・前提 #521。ポインタ: TASK-132・
//! TASK-156・CORE-16）
//!
//! 上記の [`dot_i8_f32`]（クエリ非量子化の復号 dot）に加え、クエリ側も
//! 量子化してから `isa::I8Kernel::dot_i8`（VNNI／i16 widen の整数カーネル）
//! で計算する経路を [`prepare_query`] として提供する。ノード側コード
//! `code_d ∈ [-127, 127]`（本モジュール既存の対称量子化）はそのままに、
//! クエリ側は「次元ごとスケールで一旦 code 空間へ写像した値
//! （`q'_d = scale_d * q_d`）」に単一スケール `s_q = max_d|q'_d| / 127` で
//! 対称量子化する二重量子化になる（候補生成スコアのみに影響し、索引ヒットの
//! 最終スコアは常に `kernel::dot`〔f32・アリーナ再計算〕のまま——
//! `docs/design/simd-intrinsics-adoption.md` 決定 5）。
//!
//! `isa::I8Kernel::dot_i8` が使う `vpdpbusd`（u8×s8→i32）系命令に合わせ、
//! クエリ側コードを `u_d = (qq_d as i16 + 128) as u8 ∈ [1, 255]` へ符号なし
//! シフトし、ノード側の `hnsw.rs::NodeVectors::I8`（`hnsw.rs::freeze_from`
//! が凍結時に 1 回計算する [`row_sums`]）を使って
//! `Σ qq_d*code_d == Σ u_d*code_d − 128*row_sum` で復元する
//! （`docs/design/hnsw-sq8-resident.md`「Issue #522」節参照）。

/// 対称量子化のコード上限（`i8` の `-128` は使わない。qdrant
/// `encoded_vectors_u8` と同じ `[-127, 127]` の対称範囲）。
const CODE_MAX: f64 = 127.0;

/// 次元ごとの量子化スケール（Issue #521）。次元 `d` の格納コード `q_d` から
/// 元の値への復号は `dequantize(q_d, scale(d)) == q_d as f32 * scale(d)`。
///
/// 対称量子化のため次元ごとの平行移動（オフセット）は持たない
/// （`min_d`／`max_d` 自体は [`fit_dim_params`] がスケール算出のためだけに
/// 使う一時値で、`scale_d = max(|min_d|, |max_d|) / 127` にすべて集約される
/// ため保持しない）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sq8DimParams {
    scales: Vec<f32>,
}

impl Sq8DimParams {
    /// 次元数。
    pub(crate) fn dim(&self) -> usize {
        self.scales.len()
    }

    /// 次元ごとのスケール列（`hnsw.rs::NodeVectors::I8` の候補生成スコア
    /// 計算・`node_matches` の範囲検査が使う）。
    pub(crate) fn scales(&self) -> &[f32] {
        &self.scales
    }

    /// `hnsw.rs::NodeVectors::approx_bytes` が使う概算ヒープバイト量。
    pub(crate) fn approx_heap_bytes(&self) -> usize {
        self.scales.len().saturating_mul(std::mem::size_of::<f32>())
    }
}

/// [`fit_dim_params`]・[`encode_rows`] の失敗要因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sq8Error {
    /// `index` 番目の成分（`rows` を row-major で平坦化した添字）が非有限
    /// （NaN／Inf）だった。
    NonFinite { index: usize },
    /// `dim == 0`、または `rows.len()` が `dim` の整数倍でない。
    InvalidShape,
    /// 出力バッファの確保に失敗した（`try_reserve_exact` が `Err`）。
    AllocationFailed,
    /// `index` 次元目のスケール（f64 で算出した `max(|min|, |max|) / CODE_MAX`
    /// を f32 へ丸めた値）が境界を保持できなかった（PR #617 codex-review P1
    /// 指摘）。次の 2 状態のいずれかを指す——(1) 次元の値が非零（`max_abs >
    /// 0.0`）にもかかわらずスケールが `0.0` へアンダーフローし、
    /// `quantize_scalar_f64` の `scale == 0.0` 早期リターンにより全コードが
    /// 消失する、(2) f64→f32 丸めで拡大したスケールを `CODE_MAX`（127）倍
    /// して復号すると `f32::MAX` を超え `Infinity` になり得る（`dequantize`
    /// の呼び出し元が `NonFiniteScore` として扱う可能性がある）。いずれも
    /// D6（`f16.rs` の範囲外縮退）と同型の fail-closed 判定として扱い、
    /// 呼び出し元（`hnsw.rs::HnswIndex::freeze_from`）はこの次元 1 つでも
    /// 検出すれば索引全体を `F32` 常駐へ縮退する。
    ScaleOutOfRange { index: usize },
    /// [`prepare_query`]（Issue #522）専用: `dim` が [`I8_DOT_MAX_DIM`] を
    /// 超えるクエリを二重量子化しようとした。整数累積（u8×s8→i32）が
    /// `i32` の範囲へ収まる保証を失うため、呼び出し元（`hnsw.rs::
    /// PreparedI8Source`）はこのクエリに限り [`dot_i8_f32`]（復号 dot）へ
    /// fail-closed に縮退する。
    QueryDimTooLarge { dim: usize },
    /// [`prepare_query`] 専用: クエリ側の単一スケール `s_q` を復号したとき
    /// （`s_q * 127 * 127 * dim`）が `f32` で非有限になり得る、または算出
    /// 過程（`q'_d = scale_d * q_d`）自体が非有限になった。[`ScaleOutOfRange`]
    /// と同型の fail-closed 判定だが、ノード側スケールではなくクエリ単一
    /// スケールが原因である点を区別するため独立 variant にする。
    QueryScaleOutOfRange,
}

/// `value / scale` を round-half-away-from-zero で丸め、`[-127, 127]` へ
/// クランプして `i8` へ変換する（qdrant `encoded_vectors_u8` と同じ規律。
/// `packed_i8.rs::row_scale_f64` と同じく、丸めまでを f64 精度で行ってから
/// 最後に narrow する——#598 codex P1 指摘の再発防止）。`scale == 0.0`
/// （定数 0 次元）は除算せず `0` を返す。
pub(crate) fn quantize_scalar_f64(value: f64, scale: f64) -> i8 {
    if scale == 0.0 {
        return 0;
    }
    let q = (value / scale).round().clamp(-CODE_MAX, CODE_MAX);
    q as i8
}

/// `rows`（row-major・`dim` 列）から次元ごとの対称量子化スケールを求める
/// （凍結時に 1 回だけ呼ぶ）。`rows` が空（`n == 0`）の場合は全次元スケール 0
/// の [`Sq8DimParams`] を返す（成功。縮退ではない——凍結対象の索引が空である
/// ことをそのまま表す）。
pub(crate) fn fit_dim_params(dim: usize, rows: &[f32]) -> Result<Sq8DimParams, Sq8Error> {
    if dim == 0 {
        return Err(Sq8Error::InvalidShape);
    }
    if !rows.len().is_multiple_of(dim) {
        return Err(Sq8Error::InvalidShape);
    }
    for (index, &value) in rows.iter().enumerate() {
        if !value.is_finite() {
            return Err(Sq8Error::NonFinite { index });
        }
    }

    let mut mins = vec![0f64; dim];
    let mut maxs = vec![0f64; dim];
    let mut first_row = true;
    for row in rows.chunks_exact(dim) {
        if first_row {
            for ((v, min_d), max_d) in row.iter().zip(mins.iter_mut()).zip(maxs.iter_mut()) {
                let v = f64::from(*v);
                *min_d = v;
                *max_d = v;
            }
            first_row = false;
            continue;
        }
        for ((v, min_d), max_d) in row.iter().zip(mins.iter_mut()).zip(maxs.iter_mut()) {
            let v = f64::from(*v);
            if v < *min_d {
                *min_d = v;
            }
            if v > *max_d {
                *max_d = v;
            }
        }
    }

    let mut scales = Vec::with_capacity(dim);
    for (index, (min_d, max_d)) in mins.iter().zip(maxs.iter()).enumerate() {
        let max_abs = min_d.abs().max(max_d.abs());
        let scale_f64 = max_abs / CODE_MAX;
        let scale = scale_f64 as f32;
        // 非零成分（max_abs > 0.0）なのにスケールが 0.0 へアンダーフローする
        // と、quantize_scalar_f64 の scale == 0.0 早期リターンで当該次元の
        // 全コードが 0 に潰れ値が消失する（PR #617 codex-review P1 指摘）。
        if max_abs > 0.0 && scale == 0.0 {
            return Err(Sq8Error::ScaleOutOfRange { index });
        }
        // f64→f32 丸めでスケールが真値より拡大され得るため、CODE_MAX（127）倍
        // した復号後の最大値を f64 精度で検算する。f32::MAX を超える場合、
        // dequantize の f32 乗算（127.0f32 * scale）が Infinity へ丸まり得る
        // （PR #617 codex-review P1 指摘）。
        let decoded_max = f64::from(scale) * CODE_MAX;
        if !decoded_max.is_finite() || decoded_max > f64::from(f32::MAX) {
            return Err(Sq8Error::ScaleOutOfRange { index });
        }
        scales.push(scale);
    }
    Ok(Sq8DimParams { scales })
}

/// [`fit_dim_params`] が返した `params` で `rows`（row-major・`dim` 列）を
/// エンコードし `out` へ追記する（`f16::encode_rows` と同じ契約——呼び出し元は
/// 都度 `out.clear()` 済みの再利用バッファを渡す想定）。非有限成分・形状不一致
/// を検出した場合、または出力バッファの確保に失敗した場合は `Err` を返し
/// `out` は変更しない（fail-closed。呼び出し元は `F32` 常駐へ縮退する）。
pub(crate) fn encode_rows(
    dim: usize,
    rows: &[f32],
    params: &Sq8DimParams,
    out: &mut Vec<i8>,
) -> Result<(), Sq8Error> {
    if dim == 0 || dim != params.dim() {
        return Err(Sq8Error::InvalidShape);
    }
    if !rows.len().is_multiple_of(dim) {
        return Err(Sq8Error::InvalidShape);
    }
    for (index, &value) in rows.iter().enumerate() {
        if !value.is_finite() {
            return Err(Sq8Error::NonFinite { index });
        }
    }

    out.try_reserve_exact(rows.len())
        .map_err(|_| Sq8Error::AllocationFailed)?;
    for row in rows.chunks_exact(dim) {
        for (&v, &scale) in row.iter().zip(params.scales.iter()) {
            out.push(quantize_scalar_f64(f64::from(v), f64::from(scale)));
        }
    }
    Ok(())
}

/// 格納コード `code`（次元 `d` のスケール `scale`）を元の近似値へ復号する。
pub(crate) fn dequantize(code: i8, scale: f32) -> f32 {
    f32::from(code) * scale
}

/// `codes`（索引ノードの格納コード・`dim` 要素）を `scales`（同じ `dim`）で
/// 復号しながら `query`（f32・`dim` 要素）と内積する（`hnsw.rs::NodeVectors::I8`
/// の候補生成スコア。クエリ側は量子化せず f32 のまま扱うため、`f16.rs` の
/// 昇格 dot と同じ「格納側だけ低精度」の非対称設計になる）。3 スライスの
/// 長さが食い違う場合は短い方に合わせて打ち切る（呼び出し元が `dim` 一致を
/// 事前検証済みの内部専用パスであり、ここでの不一致は「本来到達しない」
/// 防御的縮退——`hnsw.rs::node_vector_i8` が範囲検証を担うため通常は
/// 到達しない）。
pub(crate) fn dot_i8_f32(codes: &[i8], scales: &[f32], query: &[f32]) -> f32 {
    let mut acc = 0f32;
    for ((&code, &scale), &q) in codes.iter().zip(scales.iter()).zip(query.iter()) {
        acc += dequantize(code, scale) * q;
    }
    acc
}

// ---------------------------------------------------------------------
// 整数 i8×i8 dot（Issue #522・親 #520・前提 #521）。
// ---------------------------------------------------------------------

/// [`prepare_query`] の二重量子化が扱える最大次元数。`vpdpbusd` 系命令
/// （u8×s8→i32 の非飽和積和）の累積が `i32` へ収まる保証として
/// `255（u8 最大）* 127（i8 最大絶対値）* dim ≤ 2^31 - 1` を満たす上限を
/// 逆算した値（`255 * 127 * 65_536 == 2_113_929_216 < i32::MAX`。
/// `65_537` からは超過する）。[`row_sums`] が返す行和の絶対値上限
/// （`127 * dim`）・`128 * row_sum` の絶対値上限もこの範囲内に収まる
/// （`128 * 127 * 65_536 == 1_065_353_216 < i32::MAX`）。
pub(crate) const I8_DOT_MAX_DIM: usize = 65_536;

/// [`crate::hnsw::NodeVectors::I8`] の各ノード行について `Σ_d code_d`
/// （凍結時に 1 回だけ計算する行和）を求める。[`prepare_query`] が生成する
/// クエリ側符号なしコード `u_d = qq_d + 128` を使った整数 dot
/// （`Σ u_d*code_d`）から、符号付きの真の内積（`Σ qq_d*code_d`）を
/// `Σ u_d*code_d − 128*row_sum` として復元するために必要（モジュール冒頭
/// 「整数 i8×i8 dot」節参照）。`codes`（row-major・`dim` 列）の形状が
/// `dim` の整数倍でない場合は [`Sq8Error::InvalidShape`] を返す。
pub(crate) fn row_sums(dim: usize, codes: &[i8]) -> Result<Vec<i32>, Sq8Error> {
    if dim == 0 || !codes.len().is_multiple_of(dim) {
        return Err(Sq8Error::InvalidShape);
    }
    let n = codes.len() / dim;
    let mut out = Vec::new();
    out.try_reserve_exact(n)
        .map_err(|_| Sq8Error::AllocationFailed)?;
    for row in codes.chunks_exact(dim) {
        // `|Σ code_d| <= 127 * dim <= 127 * I8_DOT_MAX_DIM` は `i32` の範囲に
        // 十分収まる（本関数のドキュメンテーションコメント参照）ため
        // wrapping は実質到達しないが、untrusted な `dim`（呼び出し元は
        // `hnsw.rs::freeze_from` の内部専用パスで通常は検証済みの値のみ渡す）
        // に対しても panic させない防御として `wrapping_add` を使う。
        let sum: i32 = row
            .iter()
            .fold(0i32, |acc, &c| acc.wrapping_add(i32::from(c)));
        out.push(sum);
    }
    Ok(out)
}

/// [`prepare_query`] が返す、クエリ側の二重量子化済みコード。
///
/// - `signed`：`qq_d ∈ [-127, 127]`（i16 widen カーネルが直接使う）。
/// - `shifted`：`u_d = (qq_d as i16 + 128) as u8 ∈ [1, 255]`（VNNI 系
///   カーネル（u8×s8→i32）が使う。`isa::I8QueryOperands` の契約
///   `shifted[i] == (signed[i] as i16 + 128) as u8` を満たす）。
/// - `scale_q`：`f64` の単一スケール（[`score_from_int_dot`] が使う）。
///   クエリの全成分が `0.0` の場合（もしくは次元ごとスケールとの積が
///   全て 0）は `0.0`（`signed`／`shifted` も全要素 0／128 の縮退値）。
#[derive(Debug, Clone)]
pub(crate) struct Sq8QueryCodes {
    pub(crate) signed: Vec<i8>,
    pub(crate) shifted: Vec<u8>,
    pub(crate) scale_q: f64,
}

/// `query`（f32・`params.dim()` 要素）を `params`（ノード側の次元ごと
/// スケール）と同じ code 空間へ写像したうえで、単一スケール `s_q` により
/// 対称量子化する（モジュール冒頭「整数 i8×i8 dot」節の導出そのもの）。
///
/// 失敗（`dim` 不一致・[`I8_DOT_MAX_DIM`] 超過・非有限・スケール溢れ）は
/// すべて `Err` を返し、呼び出し元（`hnsw.rs::PreparedI8Source`）はこの
/// クエリに限り [`dot_i8_f32`]（復号 dot）へ縮退する（性能崖のない
/// fail-closed。索引の再構築は発生しない）。
pub(crate) fn prepare_query(
    params: &Sq8DimParams,
    query: &[f32],
) -> Result<Sq8QueryCodes, Sq8Error> {
    let dim = params.dim();
    if dim == 0 || query.len() != dim {
        return Err(Sq8Error::InvalidShape);
    }
    if dim > I8_DOT_MAX_DIM {
        return Err(Sq8Error::QueryDimTooLarge { dim });
    }

    let mut q_prime = Vec::new();
    q_prime
        .try_reserve_exact(dim)
        .map_err(|_| Sq8Error::AllocationFailed)?;
    let mut amax: f64 = 0.0;
    for (index, (&q, &scale)) in query.iter().zip(params.scales().iter()).enumerate() {
        let qf = f64::from(q);
        if !qf.is_finite() {
            return Err(Sq8Error::NonFinite { index });
        }
        let v = qf * f64::from(scale);
        if !v.is_finite() {
            return Err(Sq8Error::NonFinite { index });
        }
        let a = v.abs();
        if a > amax {
            amax = a;
        }
        q_prime.push(v);
    }

    let mut signed = Vec::new();
    let mut shifted = Vec::new();
    signed
        .try_reserve_exact(dim)
        .map_err(|_| Sq8Error::AllocationFailed)?;
    shifted
        .try_reserve_exact(dim)
        .map_err(|_| Sq8Error::AllocationFailed)?;

    if amax == 0.0 {
        signed.resize(dim, 0i8);
        shifted.resize(dim, 128u8);
        return Ok(Sq8QueryCodes {
            signed,
            shifted,
            scale_q: 0.0,
        });
    }

    let scale_q = amax / CODE_MAX;
    // `score_from_int_dot` が計算する復号後の最悪絶対値
    // （`scale_q * 127 * 127 * dim`）が `f32` で非有限にならないことを
    // 事前検証する（`fit_dim_params` の `decoded_max` 検査と同型の
    // fail-closed 判定）。
    let worst = scale_q * CODE_MAX * CODE_MAX * (dim as f64);
    if !scale_q.is_finite() || !worst.is_finite() || worst > f64::from(f32::MAX) {
        return Err(Sq8Error::QueryScaleOutOfRange);
    }

    for &v in &q_prime {
        let code = quantize_scalar_f64(v, scale_q);
        signed.push(code);
        shifted.push((i16::from(code) + 128) as u8);
    }

    Ok(Sq8QueryCodes {
        signed,
        shifted,
        scale_q,
    })
}

/// `isa::I8Kernel::dot_i8` が返す整数内積 `int_dot`（`Σ qq_d*code_d`）を
/// `scale_q`（[`prepare_query`] が返した単一スケール）で復号し、`f64` の
/// 乗算結果を 1 回だけ `f32` へ丸める（ISA に依存しない決定的写像。
/// `hnsw.rs::score_of` 等、他の候補生成スコアと同じく最終スコアではなく
/// 候補生成にのみ使う近似値）。
pub(crate) fn score_from_int_dot(int_dot: i32, scale_q: f64) -> f32 {
    ((int_dot as f64) * scale_q) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 決定的な擬似乱数列（xorshift32）。外部クレート非依存でテストの再現性を
    /// 保つ（`hnsw.rs::tests` の既存フィクスチャと同じ方針）。
    struct XorShift32(u32);
    impl XorShift32 {
        fn next_f32(&mut self, scale: f32) -> f32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            // [-1, 1) の一様分布へ正規化してから scale を掛ける。
            let unit = (x as f64 / u32::MAX as f64) * 2.0 - 1.0;
            (unit as f32) * scale
        }
    }

    fn random_rows(seed: u32, n: usize, dim: usize, scale: f32) -> Vec<f32> {
        let mut rng = XorShift32(seed | 1);
        (0..n * dim).map(|_| rng.next_f32(scale)).collect()
    }

    #[test]
    fn fit_dim_params_constant_zero_dimension_has_zero_scale() {
        let dim = 3;
        // 次元 1 は常に 0（定数 0 次元）。
        let rows = vec![1.0, 0.0, -2.0, 5.0, 0.0, 3.5];
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        assert_eq!(params.scales().get(1).copied(), Some(0.0));
        assert!(params.scales()[0] > 0.0);
        assert!(params.scales()[2] > 0.0);

        let mut out = Vec::new();
        encode_rows(dim, &rows, &params, &mut out).expect("encode ok");
        // 定数 0 次元のコードは常に 0。
        assert_eq!(out[1], 0);
        assert_eq!(out[4], 0);
    }

    #[test]
    fn fit_and_encode_empty_rows_succeeds_with_zero_scales() {
        let dim = 4;
        let params = fit_dim_params(dim, &[]).expect("empty rows is success, not a fallback");
        assert_eq!(params.dim(), dim);
        for &scale in params.scales() {
            assert_eq!(scale, 0.0);
        }
        let mut out = Vec::new();
        encode_rows(dim, &[], &params, &mut out).expect("empty encode ok");
        assert!(out.is_empty());
    }

    #[test]
    fn dequantization_error_within_half_scale_bound() {
        for &dim in &[1usize, 3, 128, 129, 768] {
            let rows = random_rows(0x1234_5678 ^ dim as u32, 32, dim, 17.5);
            let params = fit_dim_params(dim, &rows).expect("valid shape");
            let mut out = Vec::new();
            encode_rows(dim, &rows, &params, &mut out).expect("encode ok");
            for (row_idx, row) in rows.chunks_exact(dim).enumerate() {
                let code_row = &out[row_idx * dim..(row_idx + 1) * dim];
                for (d, (&v, &code)) in row.iter().zip(code_row.iter()).enumerate() {
                    let scale = params.scales()[d];
                    let decoded = dequantize(code, scale);
                    let bound = f64::from(scale) / 2.0 + 1e-6;
                    assert!(
                        (f64::from(v) - f64::from(decoded)).abs() <= bound,
                        "dim={dim} row={row_idx} d={d}: |{v} - {decoded}| exceeds bound {bound}"
                    );
                }
            }
        }
    }

    #[test]
    fn dot_error_within_derived_upper_bound() {
        let dim = 64;
        let rows = random_rows(0x9E37_79B9, 16, dim, 12.0);
        let query = random_rows(0xC2B2_AE35, 1, dim, 3.0);
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        let mut out = Vec::new();
        encode_rows(dim, &rows, &params, &mut out).expect("encode ok");

        let mut saw_nonzero_error = false;
        for (row_idx, row) in rows.chunks_exact(dim).enumerate() {
            let code_row = &out[row_idx * dim..(row_idx + 1) * dim];
            let exact: f64 = row
                .iter()
                .zip(query.iter())
                .map(|(&v, &q)| f64::from(v) * f64::from(q))
                .sum();
            let approx = f64::from(dot_i8_f32(code_row, &params.scales, &query));

            // 誤差上界: Σ_d |q_d| * scale_d / 2（各次元の復号誤差 <= scale_d/2 に
            // クエリ側の重みを掛けた総和。クエリは量子化しないため寄与項は
            // このノード側のみ）。
            let bound: f64 = query
                .iter()
                .enumerate()
                .map(|(d, &q)| {
                    let scale = f64::from(params.scales()[d]);
                    f64::from(q).abs() * scale / 2.0
                })
                .sum::<f64>()
                + 1e-3;
            let err = (exact - approx).abs();
            assert!(
                err <= bound,
                "row={row_idx}: |{exact} - {approx}| = {err} exceeds bound {bound}"
            );
            if err > 1e-9 {
                saw_nonzero_error = true;
            }
        }
        assert!(
            saw_nonzero_error,
            "bound must be non-vacuous: at least one row should show measurable quantization error"
        );
    }

    #[test]
    fn codes_never_use_i8_min_and_stay_within_symmetric_range() {
        let dim = 8;
        // 極大値（スケールの決定要因そのもの）を含む行を混ぜ、クランプが
        // 起きない構成（fit 済み範囲内は必ず [-127, 127] に収まる）を確認する。
        let mut rows = random_rows(0xABCD_EF01, 20, dim, 9.0);
        // 先頭行を大きな値で上書きし min/max の駆動元にする。
        if let Some(first_row) = rows.get_mut(0..dim) {
            first_row.fill(100.0);
        }
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        let mut out = Vec::new();
        encode_rows(dim, &rows, &params, &mut out).expect("encode ok");
        for &code in &out {
            assert!(code != i8::MIN, "code must never be -128");
            assert!((-127..=127).contains(&code));
        }
    }

    #[test]
    fn extremely_small_scale_does_not_collapse_every_code_to_zero() {
        let dim = 4;
        // f32 でアンダーフローしない程度に小さいが 0 ではないスケール。
        let rows = random_rows(0x0BAD_F00D, 40, dim, 1e-3);
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        let mut out = Vec::new();
        encode_rows(dim, &rows, &params, &mut out).expect("encode ok");
        assert!(
            out.iter().any(|&c| c != 0),
            "at least one code must be nonzero for a non-degenerate small-scale corpus"
        );
    }

    /// 非零値のみを含む次元でも、真のスケール（f64）が f32 の最小正
    /// subnormal を下回るほど極小だと f64→f32 丸めで 0.0 へ潰れる（PR #617
    /// codex-review P1 指摘: `scale == 0.0` は `quantize_scalar_f64` の
    /// 早期リターンにより当該次元の全コードを 0 にし値を消失させる）。
    /// `fit_dim_params` はこれを `Sq8Error::ScaleOutOfRange` として検出し
    /// 拒否しなければならない（呼び出し元 `hnsw.rs::freeze_from` は `F32`
    /// 常駐へ縮退する）。
    #[test]
    fn fit_dim_params_rejects_scale_that_underflows_to_zero() {
        let dim = 2;
        // 次元 0: 全行 1e-44（非零・f32 subnormal だが scale = 1e-44/127 は
        // f32 の最小正 subnormal（約 1.4e-45）を下回り 0.0 へ丸まる）。
        // 次元 1: 通常のスケールで対照。
        let rows = vec![1e-44f32, 1.0, 1e-44f32, -1.0, 1e-44f32, 0.5];
        assert_eq!(
            fit_dim_params(dim, &rows),
            Err(Sq8Error::ScaleOutOfRange { index: 0 })
        );
    }

    /// f64 で算出したスケールを f32 へ丸める際に真値より拡大され得るため、
    /// `CODE_MAX`（127）倍した復号後の最大値が `f32::MAX` を超えると
    /// `dequantize` の f32 乗算が `Infinity` へ丸まり得る（PR #617
    /// codex-review P1 指摘）。`fit_dim_params` はこの次元を
    /// `Sq8Error::ScaleOutOfRange` として拒否しなければならない。
    #[test]
    fn fit_dim_params_rejects_scale_that_overflows_on_decode() {
        let dim = 1;
        // 次元の最大絶対値を f32::MAX に設定する。真のスケール
        // f32::MAX/127（f64 精度）を f32 へ丸めると、丸め方向次第では
        // 127 倍した復号値が f32::MAX を超え得る。
        let rows = vec![f32::MAX, -f32::MAX];
        let result = fit_dim_params(dim, &rows);
        // 拡大丸めが実際に起きた場合のみ ScaleOutOfRange。丸めが真値
        // 以下に留まった場合（縮小丸め）は安全なため成功してよい——本テストは
        // 「成功時に復号が必ず有限であること」を固定する（環境依存の丸め
        // 方向に左右されない不変条件の検証）。
        match result {
            Err(Sq8Error::ScaleOutOfRange { index: 0 }) => {}
            Ok(params) => {
                let decoded = dequantize(127, params.scales()[0]);
                assert!(
                    decoded.is_finite(),
                    "accepted scale must decode without overflowing to Infinity"
                );
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn fit_dim_params_rejects_non_finite_and_shape_mismatch() {
        assert_eq!(fit_dim_params(0, &[]), Err(Sq8Error::InvalidShape));
        assert_eq!(fit_dim_params(3, &[1.0, 2.0]), Err(Sq8Error::InvalidShape));
        assert_eq!(
            fit_dim_params(2, &[1.0, f32::NAN]),
            Err(Sq8Error::NonFinite { index: 1 })
        );
        assert_eq!(
            fit_dim_params(2, &[f32::INFINITY, 2.0]),
            Err(Sq8Error::NonFinite { index: 0 })
        );
    }

    #[test]
    fn encode_rows_rejects_non_finite_and_leaves_out_unchanged() {
        let dim = 2;
        let params = fit_dim_params(dim, &[1.0, 2.0, 3.0, 4.0]).expect("valid shape");
        let mut out = vec![9i8, 9i8];
        let err = encode_rows(dim, &[1.0, f32::NAN], &params, &mut out).unwrap_err();
        assert_eq!(err, Sq8Error::NonFinite { index: 1 });
        assert_eq!(out, vec![9i8, 9i8]);
    }

    #[test]
    fn encode_rows_rejects_dim_mismatch_with_params() {
        let params = fit_dim_params(2, &[1.0, 2.0]).expect("valid shape");
        let mut out = Vec::new();
        let err = encode_rows(3, &[1.0, 2.0, 3.0], &params, &mut out).unwrap_err();
        assert_eq!(err, Sq8Error::InvalidShape);
        assert!(out.is_empty());
    }

    #[test]
    fn encode_rows_is_deterministic() {
        let dim = 16;
        let rows = random_rows(0x1111_2222, 8, dim, 5.0);
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        let mut out1 = Vec::new();
        let mut out2 = Vec::new();
        encode_rows(dim, &rows, &params, &mut out1).expect("encode ok");
        encode_rows(dim, &rows, &params, &mut out2).expect("encode ok");
        assert_eq!(out1, out2);
    }

    #[test]
    fn quantize_scalar_rounds_half_away_from_zero_and_clamps() {
        assert_eq!(quantize_scalar_f64(0.0, 0.0), 0);
        assert_eq!(quantize_scalar_f64(5.0, 0.0), 0);
        assert_eq!(quantize_scalar_f64(1.5, 1.0), 2);
        assert_eq!(quantize_scalar_f64(-1.5, 1.0), -2);
        assert_eq!(quantize_scalar_f64(1000.0, 1.0), 127);
        assert_eq!(quantize_scalar_f64(-1000.0, 1.0), -127);
    }

    // ---------- 整数 i8×i8 dot（Issue #522） ----------

    #[test]
    fn row_sums_matches_naive_sum_and_rejects_bad_shape() {
        let dim = 4;
        let codes: Vec<i8> = vec![1, -2, 3, -4, 127, -127, 0, 5];
        let sums = row_sums(dim, &codes).expect("valid shape");
        assert_eq!(sums, vec![-2, 5]);

        assert_eq!(row_sums(0, &codes), Err(Sq8Error::InvalidShape));
        assert_eq!(row_sums(3, &codes), Err(Sq8Error::InvalidShape));
    }

    /// `prepare_query` の往復誤差が `s_q/2` 以内（[`quantize_scalar_f64`] の
    /// round-half-away-from-zero 規律どおり）であること、`shifted` が
    /// `signed + 128` の契約（`isa::I8QueryOperands` が要求する不変条件）を
    /// 全要素で満たすことを固定する。
    #[test]
    fn prepare_query_round_trip_and_shifted_contract() {
        let dim = 32;
        let rows = random_rows(0x0522_0001, 8, dim, 9.0);
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        let query = random_rows(0x0522_0002, 1, dim, 4.0);
        let prepared = prepare_query(&params, &query).expect("prepare ok");
        assert_eq!(prepared.signed.len(), dim);
        assert_eq!(prepared.shifted.len(), dim);

        for (d, ((&code, &shifted), &scale)) in prepared
            .signed
            .iter()
            .zip(prepared.shifted.iter())
            .zip(params.scales().iter())
            .enumerate()
        {
            assert_eq!(
                shifted,
                (i16::from(code) + 128) as u8,
                "d={d}: shifted must equal signed + 128"
            );
            assert!((-127..=127).contains(&code), "d={d}: code out of range");

            let q_prime = f64::from(query[d]) * f64::from(scale);
            let decoded = f64::from(code) * prepared.scale_q;
            let bound = prepared.scale_q / 2.0 + 1e-9;
            assert!(
                (q_prime - decoded).abs() <= bound,
                "d={d}: |{q_prime} - {decoded}| exceeds bound {bound}"
            );
        }
    }

    #[test]
    fn prepare_query_zero_query_yields_zero_scale_and_codes() {
        let dim = 8;
        let rows = random_rows(0x0522_0003, 4, dim, 2.0);
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        let query = vec![0.0f32; dim];
        let prepared = prepare_query(&params, &query).expect("prepare ok");
        assert_eq!(prepared.scale_q, 0.0);
        assert!(prepared.signed.iter().all(|&c| c == 0));
        assert!(prepared.shifted.iter().all(|&u| u == 128));
    }

    #[test]
    fn prepare_query_rejects_dim_mismatch_and_oversized_dim() {
        let dim = 4;
        let params = fit_dim_params(dim, &[1.0, 2.0, 3.0, 4.0]).expect("valid shape");
        assert_eq!(
            prepare_query(&params, &[1.0, 2.0, 3.0]).unwrap_err(),
            Sq8Error::InvalidShape
        );

        // I8_DOT_MAX_DIM 超過は次元数のみで判定できるため、1 次元の
        // params を再構成せず I8_DOT_MAX_DIM+1 要素の query を同じ 1 次元
        // params へ渡す形では検出できない（dim 不一致が先に InvalidShape
        // を返す）。ここでは dim 自体が I8_DOT_MAX_DIM を超える params を
        // 直接構成し、QueryDimTooLarge が実際に返ることを固定する。
        let big_dim = I8_DOT_MAX_DIM + 1;
        let big_params = Sq8DimParams {
            scales: vec![1.0f32; big_dim],
        };
        let big_query = vec![1.0f32; big_dim];
        assert_eq!(
            prepare_query(&big_params, &big_query).unwrap_err(),
            Sq8Error::QueryDimTooLarge { dim: big_dim }
        );
    }

    /// `score_from_int_dot` が整数値を素直に復号することを固定する（決定的
    /// 写像であることのスモークテスト）。
    #[test]
    fn score_from_int_dot_decodes_linearly() {
        assert_eq!(score_from_int_dot(0, 1.5), 0.0);
        assert_eq!(score_from_int_dot(10, 2.0), 20.0);
        assert_eq!(score_from_int_dot(-10, 2.0), -20.0);
    }

    /// `dot_i8_f32`（復号 dot）と `prepare_query`＋整数 dot（スカラー参照実装
    /// で手計算）が許容差内で一致することを固定する——`isa::I8Kernel` 側の
    /// ビット同一性は `isa.rs`／`tests/isa.rs` の担当だが、`sq8.rs` 単体では
    /// 「二重量子化した近似値が既存の単一量子化近似値と大きく乖離しない」
    /// ことを確認する。
    #[test]
    fn prepare_query_int_dot_agrees_with_dequant_dot_within_tolerance() {
        let dim = 64;
        let rows = random_rows(0x0522_0004, 16, dim, 12.0);
        let query = random_rows(0x0522_0005, 1, dim, 5.0);
        let params = fit_dim_params(dim, &rows).expect("valid shape");
        let mut codes = Vec::new();
        encode_rows(dim, &rows, &params, &mut codes).expect("encode ok");
        let sums = row_sums(dim, &codes).expect("row sums ok");
        let prepared = prepare_query(&params, &query).expect("prepare ok");

        for (row_idx, row_codes) in codes.chunks_exact(dim).enumerate() {
            let dequant = dot_i8_f32(row_codes, params.scales(), &query);

            let int_dot: i32 = row_codes
                .iter()
                .zip(prepared.signed.iter())
                .map(|(&c, &q)| i32::from(c) * i32::from(q))
                .sum();
            // 参照実装として shifted+row_sum の復元式も一致することを固定する。
            let acc_u8: i32 = row_codes
                .iter()
                .zip(prepared.shifted.iter())
                .map(|(&c, &u)| i32::from(c) * i32::from(u))
                .sum();
            let row_sum = sums[row_idx];
            assert_eq!(acc_u8 - 128 * row_sum, int_dot, "row={row_idx}");

            let approx = score_from_int_dot(int_dot, prepared.scale_q);
            let tolerance = 0.05 * dequant.abs().max(1.0) + 0.5;
            assert!(
                (approx - dequant).abs() <= tolerance,
                "row={row_idx}: approx={approx} dequant={dequant} exceeds tolerance {tolerance}"
            );
        }
    }
}
