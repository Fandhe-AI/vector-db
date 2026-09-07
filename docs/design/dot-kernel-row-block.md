# `search_range` の行ブロック（4 行）カーネル（AVX2+FMA／AVX-512F）

- ステータス: **実装済み（既定エンジンへ結線済み）**
- 対応: Issue #510（`perf(engine): search_range の行ブロック（4 行）カーネルを
  AVX2+FMA／AVX-512F で実装する`）
- 依存: ADR `docs/design/simd-intrinsics-adoption.md`（Issue #508。ステータス
  Proposed・オーナー承認待ち。本 Issue は決定 1 が明示的に許容する「新カーネル
  1 種につきトークンディスパッチ箇所 1 つの `unsafe`」を 2 箇所（AVX2+FMA・
  AVX-512）追加する）
- 関連ポインタ: TASK-156（CORE-14。`isa.rs` の実行時 ISA 検出）・Issue #365
  （行内 ILP 不採用の先行判断）・`docs/design/simd-codegen-guard.md`（生成コード
  検査ガード。「Issue #510 以降の追記」節に本変更の実測知見を記録）
- 後続: Issue #511（NEON 版行ブロックカーネル。本 Issue の `Neon` variant は
  4 × `dot` の縮退のまま）・Issue #512（前後比較・採否判断）

## 1. 背景・目的

`isa.rs::dot_lanes<LANES, PADDED_TAIL>` は 1 行 × 1 クエリの内積で、行ごとに
クエリのレーン配列をロードし直す。`parallel_search.rs::search_range` の行ループ
で 4 行を同時に処理すれば、クエリチャンクのロードを 4 行で 1 回に減らし、行側の
ロードは FMA のメモリオペランドへ畳み込める（load 帯域の削減。判断材料は
`docs/design/chip-kernel-guidelines.md` §3 表・`hotpath-implementation-survey.md`）。

Issue #365 で不採用になったのは **行内**（1 行の dot をアキュムレータ複数本へ
分割し演算順が変わる）ILP。本 Issue は **行間** の再利用であり、各行の
アキュムレータ構造（`LANES` 本のレーン FMA → レーン和 → 端数和）は 1 行版と
同一に保つため、スコアは 1 行版と**ビット同一**。既定エンジン
（`ParallelBruteForce`）へそのまま適用でき、Recall 3 ゲート・層 A 固定値・
cold/hot 等価性は構造的に不変。

## 2. 設計

### 2.1 カーネル契約（`isa.rs::SimdKernel::dot_block4`）

```rust
impl SimdKernel {
    /// 4 行 × 1 クエリの内積。契約:
    ///   dot_block4([r0,r1,r2,r3], q)[i].to_bits() == self.dot(r_i, q).to_bits()
    pub fn dot_block4(self, rows: [&[f32]; 4], query: &[f32]) -> [f32; 4]
}
```

- **高速経路の条件**: 4 行すべてと `query` の長さが等しい場合のみ intrinsics
  ブロックカーネルを使う。長さが 1 つでも異なれば `[self.dot(r0,q), ...]`
  （1 行版を 4 回呼ぶ）へ縮退する（`zip` が最短長で切り詰めてビット同一契約が
  崩れるのを防ぐ。production では `search_range` が `dim` 長を保証するため常に
  高速経路）。
- **`PADDED_TAIL`**: 既存の `dot_impl::<PADDED_TAIL>` と同じ const generic を
  貫通させる。`dot_block4`（既定経路）は `DEFAULT_PADDED_TAIL` を使うため、
  Issue #529 で既定が反転しても `dot` との同一性が自動的に維持される。
- **ディスパッチ**（`dot_block4_impl::<PADDED_TAIL>` の `match self`）:
  - `Scalar` → 4 × `dot_scalar`
  - `Neon(_)` → 4 × `self.dot_impl`（NEON 版カーネルは Issue #511）
  - `Avx2Fma(_)` → `unsafe { x86_block4::dot_block4_avx2_fma(...) }`
    （**新規 unsafe 1**。SAFETY: `Avx2FmaToken` 所持）
  - `Avx512(_)` → `unsafe { x86_block4::dot_block4_avx512(...) }`
    （**新規 unsafe 2**。SAFETY: `Avx512Token` 所持）
- **`unsafe` 個数**: `isa.rs` 全文で 3 → 5（Neon/Avx2Fma/Avx512 の `dot`
  ディスパッチ 3 箇所 + Avx2Fma/Avx512 の `dot_block4` ディスパッチ 2 箇所）。

### 2.2 intrinsics カーネル本体（`crates/engine/src/isa/x86_block4.rs`）

ADR 決定 1「カーネル本体は safe fn として `isa/*.rs` へ分離してよい。`unsafe` は
`isa.rs` 以外に持ち込めない」に従い、新規サブモジュールとして分離した
（`isa.rs` に `#[cfg(target_arch = "x86_64")] mod x86_block4;` を追加。`lib.rs`
の `pub mod` は変更しない）。

- `load8`／`load16`: `as_chunks::<8|16>()` で得た `&[f32; N]` をパターン分解し、
  `_mm256_set_ps`／`_mm512_set_ps` へレーン逆順（7→0／15→0）で渡す。ポインタ
  load（`_mm256_loadu_ps` 等）は使わない（raw pointer 経由の `unsafe fn` の
  ため、`isa.rs` 以外での `unsafe` 持ち込みになってしまう）。
- `dot_block4_avx2_fma`／`dot_block4_avx512`: クエリチャンクを 1 回だけロードし、
  4 本のアキュムレータ（`__m256`／`__m512`）へ `_mm256_fmadd_ps`／
  `_mm512_fmadd_ps` で積算する。行側のロードは FMA の第 1 引数として都度構築し、
  メモリオペランドへの畳み込みは LLVM に委ねる。
- レーン和・端数和の共有: `isa.rs::dot_lanes` の縮約段を `lane_sum`（`[f32;
  LANES]::iter().sum()`）／`tail_sum`（`PADDED_TAIL` 分岐込みの端数和）へ切り出し、
  `dot_lanes`（1 行版）と行ブロックカーネルの双方が `tail_sum` を共有する。
  レーン和自体は下記の理由でブロック側が独自のスカラー直接縮約
  （`lane_sum8`／`lane_sum16`）を持つ（`lane_sum` とビット同一の左畳み込み順
  `0.0 + l0 + l1 + ...` を維持）。

## 3. 生成コード検査で発覚した問題と対処（実装時の主要な設計変更点）

計画段階のプロトタイプ確認（rustc 1.96・`-C target-cpu=native`）では
`_mm256_set_ps`／`_mm256_extractf128_ps`＋`_mm_permute_ps` の組み合わせで
禁止命令が出ないことを確認していたが、実際に `make simd-codegen-check`
相当（`-C target-cpu=native` 無し・実際の release ビルド設定）で検査したところ
`vinsertps`／`vunpcklps`／`vunpckhps` が検出され fail した。

### 3.1 診断

生成された `.s` を直接確認したところ:

1. **主ループ**（`_mm256_fmadd_ps` 4 回・`load8` 呼び出し）は意図どおり
   `vmovups` ×1 + `vfmadd231ps` ×4 に畳み込まれており、禁止命令は 0 件だった。
2. 禁止命令はすべて**ループ後**、4 行分の水平和を `[f32; 4]` の戻り値として
   1 回で返す箇所に集中していた。LLVM の SLP ベクトライザが「4 本の独立した
   スカラー水平和 → `[f32; 4]` の array/戻り値」というパターンを見つけ、
   4 つの水平和を `vinsertps`／`vunpcklps`／`vunpckhps` で 1 個の SIMD
   レジスタへ**再構成**してから 1 回の `vmovups` で返す最適化を行っていた。

最初に試した「レーン抽出を `[f32; LANES]` へ詰めてから `iter().sum()`」を
「配列を経由しないスカラー直接縮約（`lane_sum8`／`lane_sum16`。抽出した
スカラーへ直接 `sum += l_i` を適用）」へ変更しても、この再パックは解消
しなかった。原因はレーン抽出方式ではなく、**4 つの計算結果を `[f32; 4]` として
1 回で返す戻り値の形そのもの**にあった。

### 3.2 対処

`dot_block4_avx2_fma`／`dot_block4_avx512` の戻り値を `[f32; 4]` の 1 回の
戻り値ではなく、**4 個の独立した `&mut f32` 出力引数**（`out0..out3`）へ変更した。
`[f32; 4]` の組み立ては呼び出し元（`isa.rs::SimdKernel::dot_block4_impl`。
`#[target_feature]` を持たないプレーンな関数）側で行う。この関数は AVX
レジスタでの計算をそもそも持たないため、SLP がベクトル化する対象を見つけられず、
再パックが起きなくなることを実測で確認した。

```rust
// isa/x86_block4.rs（intrinsics 本体）
pub(super) fn dot_block4_avx2_fma<const PADDED_TAIL: bool>(
    rows: [&[f32]; 4], query: &[f32],
    out0: &mut f32, out1: &mut f32, out2: &mut f32, out3: &mut f32,
) { /* ... *out0 = ...; *out1 = ...; ... */ }

// isa.rs（ディスパッチ側。target_feature なし）
let (mut s0, mut s1, mut s2, mut s3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
unsafe { x86_block4::dot_block4_avx2_fma::<PADDED_TAIL>(rows, query, &mut s0, &mut s1, &mut s2, &mut s3) }
[s0, s1, s2, s3]
```

対処後の実測命令サマリ（本開発環境・rustc 1.96.0・AVX2+FMA 実機。AVX-512F は
コンパイル確認のみ）は `docs/design/simd-codegen-guard.md`「Issue #510 以降の
追記」節に記録した。禁止命令 0 件・主ループの命令構成は不変。

この経緯は、`docs/design/simd-codegen-guard.md` §1 が述べる「LLVM の最適化は
言語仕様の保証ではなく、コードの書き方の些細な違いで挙動が変わる」ことを
実例で裏付けるものであり、`scripts/check_simd_codegen.sh` の機械検査なしには
この再パックの混入に気付けなかった。

## 4. ビット同一性の検証

`crates/engine/tests/isa.rs::dot_block4_matches_single_row_dot_bit_exact_across_dims`
が、決定的シード RNG による dim 0..=129・768・1000・1536（本開発環境の
実行時検出 ISA=AVX2+FMA 上で実行）と、符号付きゼロ・微小値を含むエッジ値集合の
両方で `dot_block4` の各要素が 1 行版 `dot` とビット同一であることを固定する。
`dot_block4_falls_back_to_single_row_dot_when_lengths_are_not_uniform` が
長さ不一致時の縮退経路の一致も固定する。AVX-512F は本開発環境（QEMU 上の
仮想 CPU・AVX2 のみ）では実行時検証できず、コンパイル・命令検査
（`make simd-codegen-check`）と AVX2+FMA 版とのロジック対称性のみで担保する
（実機検証は Issue #512／#530 の担当）。

`crates/engine/src/parallel_search.rs` 側の回帰は
`search_range_with_non_multiple_of_4_row_count_matches_scalar_reference`
（n が 4 の倍数でない場合）・`search_range_block_with_one_missing_row_only_skips_that_row`
（ブロック内 1 行だけ `vectors` 不足）・`search_range_with_dim_zero_and_k_zero_does_not_panic`
（dim=0・k=0）で固定する。`tests/parallel_search.rs::matches_scalar_reference_for_various_n_dim_k`
にもブロック境界（4 の倍数・非倍数）・AVX2/AVX-512 レーン境界（8・16）を横断する
ケースを追加した。

## 5. 限界・スコープ外

- NEON 版行ブロックカーネル → Issue #511（本 Issue の `Neon` arm は 4 × `dot`
  の縮退のまま）
- 前後比較・採否判断・`dot_kernel_bench` へのブロック計測段追加 → Issue #512
  （`SimdKernel::dot_block4` を `pub` にしておくことが計測 hook になる）
- `hnsw.rs`・`batch_search.rs`・`rls.rs` の 1 行 `dot` 呼び出しの行ブロック化
- `check_simd_codegen.sh` の「期待命令（`vfmadd*` ≥ 1）」検査への一般化
- `DEFAULT_PADDED_TAIL` の既定切替（Issue #529）
- ADR `simd-intrinsics-adoption.md` のステータス更新（オーナー作業）
