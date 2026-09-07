# 零埋め固定長バッファによる分岐なし tail（AVX2／AVX-512／NEON）

- ステータス: **順序保存形を実装（既定経路は現行のまま不変）**
- 対応: Issue #528（`perf(engine): 零埋め固定長バッファによる分岐なし tail を
  AVX2／AVX-512／NEON に実装する`。親 #527・Phase 4 親 #459・ルート #455）
- 依存: Issue #508（`docs/design/simd-intrinsics-adoption.md`。ステータス
  Proposed・オーナー承認待ち）
- 後続: Issue #529（既定切替の採否・dim 100／129／768 の前後比較実測）

## 背景・要件

`crates/engine/src/isa.rs::dot_lanes<LANES>` は `as_chunks::<LANES>()` で
`LANES` 個ずつのレーンへ積算したあと、`LANES` 未満で割り切れない端数
（`rem`）をスカラーの逐次和で処理する。simsimd 等の実装は AVX-512 の
マスクロード（`_mm512_maskz_loadu_ps`）で端数を分岐なく取り込むが、本リポは
intrinsics を使わない自動ベクトル化方針（`docs/design/chip-kernel-guidelines.md`
§0.3）を採るため、マスクロード相当を「零埋めした固定長 `[f32; LANES]`
バッファへ詰めてから通常のレーン演算に合流させる」形で実現する
（ポインタ load を伴わない safe な形。ADR #508 決定 2 のとおりマスクロードは
不採用）。

受け入れ条件は次の 2 点:

1. 現行実装とのビット同一性（演算順を変えず、零埋めレーンが加算結果に影響しない
   性質を利用する）
2. 新規 `unsafe` を追加しない

## 設計

### 順序保存形のみを実装する（折り込み形は対象外）

ADR #508 決定 5「f32 カーネル適用条件」は 2 つの経路を許容する。

1. 現行 `dot_lanes<LANES>` と全入力でビット同一であることを機械検証したうえで
   適用する経路（**順序保存形**）
2. 演算順が変わる場合は Recall 3 ゲート・層 A 固定値・cold/hot 等価性の
   再実測を添えてオーナー判断を経る経路（**折り込み形**。端数を通常チャンクと
   同じレーン FMA へ畳み込む方式。simsimd 型の本来の高速化余地はこちら）

本 Issue は自動運転（ユーザーへの質問・承認待ち不可）のため経路 2 は実施できず、
経路 1（順序保存形）のみを実装する。折り込み形は #529 以降・オーナー判断が
可能なタイミングへ申し送る（「不採用形」節参照）。

### `-0.0` 恒等性による順序保存

`dot_lanes` の端数和は `a_rem.iter().zip(b_rem).map(|(x,y)| x*y).sum()`
（`Sum<f32>` の中立元 `-0.0` を起点に左から右へ逐次加算）である。この値を
変えずに固定長化するため、`a_rem`／`b_rem` を `LANES` 長の零埋めバッファへ
詰めてから積を取り、同じ `iter().sum()` で縮約する
（`crates/engine/src/isa.rs::padded_tail_sum`）。

パディングレーンの積が縮約結果へ影響しないためには、パディング位置の積が
`-0.0`（`Sum<f32>` の中立元と同じ値）である必要がある。`a` 側を `+0.0`・
`b` 側を `-0.0` で埋めると、パディング位置の積は `0.0 * (-0.0) == -0.0`
になり、`s + (-0.0) == s` が符号付きゼロ・±inf・NaN を含む任意の `f32` の `s`
で成り立つ（本開発環境で `s + (-0.0)` を空・実数・NaN・inf・`-0.0` 自身の
各ケースについて `to_bits()` 一致を確認済み。NaN は上流
（`kernel.rs::KernelError::NonFiniteQuery`）で拒否される契約のためスコープ外
だが、加算後もビットパターンが変わらないことは確認した）。両側を `+0.0`
（または両側を `-0.0`）にすると `+0.0` パディングが混ざり `-0.0` との等価性が
崩れるケースが生じるため、片側のみを負にする設計とした。

これにより `dot_lanes::<LANES, true>`（分岐なし tail）は
`dot_lanes::<LANES, false>`（現行のスカラー逐次和 tail）と全入力でビット同一
になる。

### const generic ディスパッチ・`unsafe` 個数の不変

tail 方式を `const PADDED_TAIL: bool` ジェネリックとして `dot_lanes`・
`dot_neon`・`dot_avx2_fma`・`dot_avx512` へ通す。`SimdKernel::dot`（既定経路。
シグネチャ不変）は `dot_impl::<DEFAULT_PADDED_TAIL>` へ委譲し、
`DEFAULT_PADDED_TAIL = false`（現状維持）に固定する。テスト・ベンチ向けに
`SimdKernel::dot_with_scalar_tail`／`dot_with_padded_tail`（`dot_impl::<false>`
／`dot_impl::<true>` の薄いラッパー）を公開し、両方式を production 経路と
同一実装で比較できるようにした（`hybrid::sparse_refetch_observed` と同じ、
生産経路と検証経路が同一実装を共有する hook という位置付け）。

SIMD カーネルを呼ぶ 3 箇所の `unsafe` ブロック・SAFETY 根拠（sealed トークン
所持）は `PADDED_TAIL` の値に依存せず不変であり、`unsafe` の個数は 3 個のまま
（`crates/engine/tests/isa.rs::unsafe_is_confined_to_isa_module_with_safety_comments`
が機械検証する）。

## 機械検証

- **ビット同一性**: `crates/engine/src/isa.rs` 内 unit test
  （`dot_lanes_padded_tail_matches_scalar_tail_bit_exact`）で LANES ∈
  {4, 8, 16} × dim 0..=129 の決定的乱数、および符号付きゼロ・微小値
  （`1e-30`／`1e-25`）・subnormal（`f32::MIN_POSITIVE`）を含むエッジ値集合を
  `to_bits()` 一致で検証。`dot_lanes` は `#[target_feature]` を持たない
  generic fn（`f32::mul_add` は IEEE 準拠の正確丸め FMA で ISA 非依存）のため、
  この unit test は実機 ISA に依存せず有効。
- **結合テスト**: `crates/engine/tests/isa.rs::
  branchless_tail_matches_scalar_tail_bit_exact_across_dims` が `isa::current()`
  （実行時検出 ISA）上で dim 0..=129 の決定的乱数を用い、
  `dot_with_scalar_tail`／`dot_with_padded_tail`／`dot`（既定経路が
  `dot_with_scalar_tail` と一致すること）の 3 者を検証する。
- **生成コード検査**（`scripts/check_simd_codegen.sh`。Issue #467）: 本開発
  環境（x86_64・AVX2+FMA・avx512f 非搭載）での実ビルド `make simd-codegen-check`
  は禁止命令（要素ごと挿入命令）0 件で pass。x86_64 側の必須シンボルは
  `dot_avx2_fma`／`dot_avx512` の各シンボルが `PADDED_TAIL=false`／`true` の
  2 monomorphization としてそれぞれ現れることを確認した。aarch64 側
  （`make simd-codegen-check-cross`。クロスコンパイル `--emit asm`）は
  `SimdKernel::dot` が LLVM の関数マージにより独立シンボルとして現れなくなり
  （`dot_impl` を共有本体化した副作用。`.s` 上は `.set` エイリアスとして出力
  される）、`dot_with_scalar_tail`／`dot_with_padded_tail` の 2 wrapper が実体
  シンボルとして現れることを確認したため、必須シンボルをこの 2 つへ更新した
  （`required_segments_for` の aarch64 分岐）。いずれの target でもレーン挿入
  命令（x86: `vinsertps`／`vpinsr*`／`vunpck{l,h}ps`、aarch64: `ins v`／
  `mov v.[bhsd][n]`／`ld1 {..}[n]`）は 0 件。

## 不採用形

- **折り込み形**（端数を通常チャンクと同じレーン FMA へ畳み込み、演算順が
  変わる方式）: ADR #508 決定 5 条件 2（Recall 3 ゲート・層 A 固定値・
  cold/hot 等価性の再実測＋オーナー判断）に該当し、自動運転では実施できない
  ため対象外。計画立案時のリポ外プロトタイプ確認では、LLVM が主ループを
  ymm→xmm へ劣化させ dim 768 でも約 25% 遅くなることを確認しており、自動
  ベクトル化のみでは成立しない（実現には主ループを含むカーネル全体の
  intrinsics 化が前提になる）。
- **`copy_from_slice`／`zip` によるコピー形**: 計画立案時のプロトタイプ確認
  （`-O --emit asm`）でいずれも `callq memcpy` を含むコードへコンパイルされた。
  採用した「`iter_mut().zip(src.iter())` で 1 要素ずつコピーする」形は
  `padded_tail_sum` の生成コード（`make simd-codegen-check` の `ok:` 行）で
  `callq` を含まないことを確認済み。
- **`get(i).unwrap_or(&0.0)` 形**: LLVM が現行と同型の分岐付きスカラー tail へ
  再スカラー化するため、実質的に現行実装と同一のコードになり分岐なし化の
  効果が得られない。

## 限界

- **AVX-512 実機**: 本開発環境は AVX2+FMA のみ（`avx512f` 非搭載）のため、
  `dot_avx512` 経路の実機ディスパッチ検証は行っていない。ビット同一性は
  ISA 非依存の unit test（LANES=16 を直接呼び出し）で担保し、実ビルドでの
  シンボル・命令検査（`make simd-codegen-check`）でコンパイル可能性・
  禁止命令の不在のみを確認した。
- **NEON 実機**: `make check-cross`／`make simd-codegen-check-cross`
  （aarch64-unknown-linux-gnu へのクロスコンパイル・`--emit asm` 検査）に
  依存しており、aarch64 実機での実行時ディスパッチ検証は別タスクの担当
  （`docs/design/chip-kernel-guidelines.md`「macOS 上の
  `is_aarch64_feature_detected!` 実効性」節と同様の位置付け）。

## 参考実測（採否根拠にしない）

本 Issue は自動運転のため、性能実測は情報提供のみに留め、採否判断には使わない
（`docs/design/benchmark-judgement-policy.md` の交互 N≥5・オーナー判断が必要な
判定は #529 が担う）。既存の `crates/engine/benches/dot_kernel_bench.rs` を
用いた前後比較・判定は #529 の担当とする。

## #529 への申し送り

- `DEFAULT_PADDED_TAIL`（`isa.rs`）は現状 `false`。採用判断がまとまった場合は
  この const を `true` へ反転するだけで production 経路が切り替わる
  （SIMD カーネル呼び出し 3 箇所の `unsafe` 構造・SAFETY 根拠は不変のため
  追加のレビュー観点は生じない）。
- 判定は `docs/design/benchmark-judgement-policy.md` §3〜§4（交互 N≥5・
  per-run 生データ・min-of-N と median 併記・ノイズ帯併記）に従う。
  `SimdKernel::dot_with_scalar_tail`／`dot_with_padded_tail` が既に production
  実装と同一の hook として公開されているため、単一バイナリ内 A/B
  （`dot_kernel_bench.rs` へのベンチ側 env 追加。production コードへ環境変数を
  持ち込まない）と、2 バイナリ方式（`DEFAULT_PADDED_TAIL` を一時反転した
  ビルドとの比較）のいずれの方式でも計測できる。
- 端数長（`dim % LANES`）が大きいほど（`LANES` に近いほど）分岐なし化の
  効果が出やすいと考えられるため、dim 100／129／768（親 Issue #528 の後続で
  指定された規模点）は端数長の異なる境界を含む選定になっている。
- 順序保存の分岐なし tail は「依存 add 鎖が `r`（端数長）本→`LANES` 本へ
  増えるだけで短縮の余地が構造的に無い」という見立てを doc に残しておく
  （`docs/design/dot-kernel-multi-accumulator.md`・
  `docs/design/knn-two-stage-topk.md` と同じ、既に検討済みである旨を記録して
  同種の再提案を防ぐ役割）。folding（折り込み）形でなければ大きな高速化は
  期待しにくく、折り込み形はオーナー判断（ADR #508 決定 5 条件 2）が前提。

## スコープ外

- 既定経路（`SimdKernel::dot`）の挙動変更（#529 が担当）
- 折り込み形の実装（オーナー判断待ち。上記「不採用形」参照）
- AVX-512／NEON 実機での実行時ディスパッチ検証（別タスク）
