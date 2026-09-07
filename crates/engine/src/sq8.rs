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

    let scales = mins
        .iter()
        .zip(maxs.iter())
        .map(|(min_d, max_d)| (min_d.abs().max(max_d.abs()) / CODE_MAX) as f32)
        .collect();
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
}
