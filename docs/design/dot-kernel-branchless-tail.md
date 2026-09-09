# 零埋め固定長バッファによる分岐なし tail（AVX2／AVX-512／NEON）

- ステータス: **順序保存形を実装・既定切替は Rejected（現状維持確定）**
- 対応: Issue #528（`perf(engine): 零埋め固定長バッファによる分岐なし tail を
  AVX2／AVX-512／NEON に実装する`。親 #527・Phase 4 親 #459・ルート #455）
- 依存: Issue #508（`docs/design/simd-intrinsics-adoption.md`。ステータス
  Proposed・オーナー承認待ち）
- 対応: Issue #529（既定切替の採否・dim 100／129／768 の前後比較実測。
  「Issue #529: dim 100／129／768 での前後比較と採否記録」節参照）

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

## Issue #529: dim 100／129／768 での前後比較と採否記録

### 計測方式

`crates/engine/benches/dot_kernel_bench.rs` へ opt-in セクション
（`BENCH_DOT_KERNEL_TAIL_AB=1`）を追加した。既定（未設定）では従来の
`label=current` 10 ステージ・診断 A/B のみが実行され出力・所要時間とも不変。
有効時は dim ∈ `harness::dot_kernel::TAIL_AB_DIMS`（`[100, 129, 768]`）ごとに

1. `SimdKernel::dot_with_scalar_tail`（A・現行）と `dot_with_padded_tail`
   （B・Issue #528）の全行 `to_bits()` 完全一致を検証（fail-closed。1 件でも
   不一致なら実測値を出さず非ゼロ終了）
2. 参照区間（`SimdKernel::dot`＝production 経路。`tail_ab_ref` 行）を単独計測
3. `harness::ab::run_ab`（interleaved・warmup/計測とも先行経路を反復ごとに
   入れ替え）で A/B を計測（`tail_ab` 行。各 20 warmup・50 計測）

を行う。`docs/design/benchmark-judgement-policy.md` §3 に従い、同一バイナリを
**5 回**プロセス起動し（1 プロセス実行 = 1 run）、各 run の `min_us`／
`median_us` を per-run 生データとして保持したうえで、run 間の min-of-N・
median・参照区間の実測帯（§4 の `(max-min)/min`）を算出した。

### 環境

- CPU: `QEMU Virtual CPU version 2.5+`（本開発環境・共有 QEMU 環境）
- 命令セットフラグ: `avx2` `fma` `f16c`。`avx512*` なし
- `nproc`: 12。各 run 直前の `loadavg`: 4.26〜5.41（他プロセスと共有・非専有。
  複数 worktree が並列稼働中のため前回計測時〔2.59〜2.65〕より高負荷）
- `BENCH_DEDICATED_ENV`: 未設定（専有環境ではない）
- 計測対象コミット: before＝after＝同一バイナリ・同一コミット
  `51eab9ea8044`（`SimdKernel::dot_with_scalar_tail`／`dot_with_padded_tail`
  の hook 切替による単一バイナリ内 A/B。2 バイナリ方式は未実施。`isa.rs` は
  Issue #528（`2ca1536`）以降無変更のため、本コミットでの計測は #528 実装を
  そのまま対象にする）

### 実測記録（N=5 run・per-run 生データ・min-of-N と median の両方）

`tail_ab_ref`（参照区間・production `dot`）の run 間 min_us 値列と実測帯:

| dim | run1 | run2 | run3 | run4 | run5 | 参照区間帯（相対） |
| --- | --- | --- | --- | --- | --- | --- |
| 100 | 171.815 | 172.303 | 173.308 | 165.145 | 166.864 | 4.94% |
| 129 | 222.924 | 222.048 | 219.629 | 204.025 | 207.833 | 9.26% |
| 768 | 3112.172 | 3147.144 | 3114.036 | 2395.737 | 3067.786 | 31.37% |

`scalar_tail`（A・現行）の run 間 `min_us`／`median_us` 生データ:

| dim | 統計量 | run1 | run2 | run3 | run4 | run5 |
| --- | --- | --- | --- | --- | --- | --- |
| 100 | min_us | 171.983 | 172.193 | 173.139 | 164.204 | 165.141 |
| 100 | median_us | 175.144 | 173.703 | 176.287 | 166.774 | 168.675 |
| 129 | min_us | 221.081 | 226.317 | 219.739 | 206.220 | 208.723 |
| 129 | median_us | 227.976 | 233.194 | 225.776 | 211.836 | 216.402 |
| 768 | min_us | 3151.723 | 3142.240 | 3122.358 | 2369.882 | 3047.813 |
| 768 | median_us | 3232.705 | 3215.186 | 3440.696 | 2437.651 | 3070.705 |

`padded_tail`（B・Issue #528）の run 間 `min_us`／`median_us` 生データ:

| dim | 統計量 | run1 | run2 | run3 | run4 | run5 |
| --- | --- | --- | --- | --- | --- | --- |
| 100 | min_us | 224.878 | 218.602 | 221.697 | 205.098 | 210.964 |
| 100 | median_us | 229.089 | 225.395 | 229.243 | 214.323 | 217.665 |
| 129 | min_us | 345.771 | 345.552 | 344.544 | 336.987 | 341.132 |
| 129 | median_us | 354.783 | 354.060 | 353.485 | 344.539 | 351.878 |
| 768 | min_us | 3197.160 | 3194.851 | 3166.309 | 2404.363 | 3078.007 |
| 768 | median_us | 3273.185 | 3258.601 | 3477.560 | 2484.032 | 3115.560 |

上記 2 系列から各 run 単位で算出した `ratio_min`
（`padded_tail.min_us / scalar_tail.min_us`）・`ratio_median` 値列と
判定クラス（固定 ±5% 帯）:

| dim | ratio_min（5 run） | ratio_median（5 run） | 固定帯判定 | 参照区間帯超過 |
| --- | --- | --- | --- | --- |
| 100 | 1.3076 / 1.2695 / 1.2805 / 1.2490 / 1.2775 | 1.3080 / 1.2976 / 1.3004 / 1.2851 / 1.2904 | 全 run `Regressed` | 5 run 全てで超過（4.94% 帯に対し差分 25〜31%） |
| 129 | 1.5640 / 1.5268 / 1.5680 / 1.6341 / 1.6344 | 1.5562 / 1.5183 / 1.5656 / 1.6264 / 1.6260 | 全 run `Regressed` | 5 run 全てで超過（9.26% 帯に対し差分 53〜63%） |
| 768 | 1.0144 / 1.0167 / 1.0141 / 1.0145 / 1.0099 | 1.0125 / 1.0135 / 1.0107 / 1.0190 / 1.0146 | 全 run `Neutral` | 5 run とも非超過（31.37% 帯内） |

run 間集計（min-of-N＝5 run の `min_us` 列の最小値／median-of-N＝5 run の
`median_us` 列の中央値）から算出した集計 `ratio_min`／`ratio_median`:

| dim | scalar_tail min-of-N | padded_tail min-of-N | 集計 ratio_min | scalar_tail median-of-N | padded_tail median-of-N | 集計 ratio_median |
| --- | --- | --- | --- | --- | --- | --- |
| 100 | 164.204 | 205.098 | 1.2491 | 173.703 | 225.395 | 1.2976 |
| 129 | 206.220 | 336.987 | 1.6341 | 225.776 | 353.485 | 1.5658 |
| 768 | 2369.882 | 2404.363 | 1.0145 | 3215.186 | 3258.601 | 1.0135 |

dim=100・129 は 5 run 全て・集計 ratio のいずれで見ても固定 ±5% 帯・参照区間
実測帯の**両方**を明確に超える一貫した悪化（`docs/design/benchmark-judgement-policy.md`
§4 の判定基準を満たす）。dim=768（端数ゼロ）は 5 run 全て・集計 ratio のいずれも
固定 ±5% 帯・参照区間実測帯（31.37%）の**両方の内側**にとどまり非退行
（padded_tail は rem==0 のとき零埋めバッファ処理そのものを実行しない分岐
構造のため、追加コストがほぼ生じない）。

### 実アセンブリ確認

コミット `51eab9ea8044`（`isa.rs` は Issue #528・`2ca1536` 以降無変更）で
`cargo bench --bench dot_kernel_bench -p fandhe-vector-db-engine --no-run` のバイナリに対し
`nm` で `dot_avx2_fma` の 2 monomorphization（`PADDED_TAIL=false`／`true`）
シンボルを取得し `objdump -d -M intel` で比較した。

- 要素ごと挿入命令（`vinsertps`／`vpinsr*`／`vunpck{l,h}ps`）: 両シンボルとも
  0 件（`make simd-codegen-check` の既存確認と整合）
- 分岐命令（`j*` mnemonic）数: `PADDED_TAIL=false`（scalar tail）14 件・
  `PADDED_TAIL=true`（padded tail）9 件。tail 長 0〜7 の 7 分岐チェーンが
  「tail が空か否か」の 1 分岐へ縮約されている点は設計どおり
- 一方 `PADDED_TAIL=true` 側の tail 処理は、零埋めバッファへの詰め込みを
  `memcpy@GLIBC_2.14` の呼び出し 2 回（a 側・b 側）として実装しており、
  `docs/design/dot-kernel-branchless-tail.md`「不採用形」節で確認済みの
  `make simd-codegen-check`（専用の `--emit asm` 検査ビルド）が `callq` を
  含まないと確認した結果とは異なるコード生成になっている。両者はコンパイル
  時のプロファイル（`cargo bench` の bench プロファイル vs 検査スクリプトの
  ビルド設定）が異なり、インライン化・ループアンローリングの閾値判断が
  LLVM 側で変わったことが原因と考えられる（本 Issue の範囲では踏み込んだ
  原因特定は行わない）。この `memcpy` 呼び出し（関数呼び出しオーバーヘッド・
  ポインタエイリアシング解析の断念）が、特に tail 長が小さい dim=129
  （`129 % 8 = 1`）で悪化幅が最大（集計 ratio ≈ 1.57〜1.63）になる主因と
  分析する——固定オーバーヘッドが小さいコピー量に対して相対的に支配的に
  なるため。dim=100（`100 % 8 = 4`）は現行スカラー tail 側の逐次乗算・加算
  そのものが相対的に重くなる分、悪化幅は小さい（集計 ratio ≈ 1.25〜1.30）が、
  それでも一貫して悪化する。

### 分析: 端数長と改善の不在（設計時の見立てとの相違）

「不採用形」節・旧「#529 への申し送り」節では「端数長が `LANES` に近いほど
分岐なし化の効果が出やすい」という見立てを記録していた。実測（dim=129・
rem=1 の集計 ratio_min≈1.63 に対し dim=100・rem=4 は≈1.25）を見ると、
rem が大きいほど悪化幅が小さいという**相対的な傾向自体は元の見立ての方向と
整合する**（固定オーバーヘッドがコピー量に対して相対的に軽くなるため）。

一方で当初の見立てが指していた「効果」とは分岐なし化による**改善**であり、
実測はどの rem（1・4）でも改善ではなく一貫した悪化にとどまった。見立てが
誤っていたのは「rem が大きいほど有利」という相対方向ではなく、「分岐なし化
（零埋め固定長バッファ方式）が rem=4〜7 のいずれかで改善に転じる」という
絶対方向の期待である。零埋め固定長バッファ方式の実装（`padded_tail_sum`）が
コピー方式（本ビルドでは `memcpy` 呼び出し）に依存する限り、rem の大小に
かかわらず固定オーバーヘッドを打ち消すだけの利得は生じないと考えられる。
この「改善に至らなかったこと」と「rem に対する相対的な悪化幅の傾向」を
混同しないよう、今後の再提案を防ぐため記録として残す。

### 判定

`docs/design/benchmark-judgement-policy.md` §5「環境別の証拠力」の
「production 変更の棄却（Rejected・現状維持）」行に従い、共有 QEMU 環境でも
「両ノイズ帯を超える一貫した悪化＋静的解析／実アセンブリの裏付け」がある
場合は棄却判断が可能である。dim=100・129 で 5 run 全てが両ノイズ帯を超える
一貫した悪化を示し、上記の実アセンブリ確認（`memcpy` 呼び出しによる固定
オーバーヘッド）がその原因を裏付けたため、**Rejected（現状維持確定）**と
判断する。

- `DEFAULT_PADDED_TAIL`（`isa.rs`）は `false` のまま変更しない（本 Issue は
  `crates/engine/src/` を無変更のまま完結する）。
- `SimdKernel::dot_with_scalar_tail`／`dot_with_padded_tail` の hook・
  `unsafe` 個数・SAFETY 根拠はいずれも不変（Issue #528 の実装をそのまま
  維持）。
- 折り込み形（演算順を変える方式。ADR #508 決定 5 条件 2）は本 Issue の
  対象外のまま。順序保存形自体が悪化方向という実測が出たため、性能改善を
  狙うなら折り込み形以外の方式は見込み薄いという所見を追加で記録する。
- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測は、共有 QEMU 環境の実測でも
  一貫した明確な悪化が確認できたため、追加のオーナー実測を必須とはしない
  （ただし実施を妨げるものではない）。

### 再現手順

```sh
for i in 1 2 3 4 5; do
  cat /proc/loadavg
  BENCH_DOT_KERNEL_TAIL_AB=1 make bench-dot-kernel
done
```

各 run の `dot_kernel: tail_ab_ref ...`／`dot_kernel: tail_ab label=... ...`
行を保存し、dim ごとに `tail_ab_ref` の `min_us` 列から実測帯
（`(max-min)/min`）を、`tail_ab` の `ratio_min`／`ratio_median` 列から
固定 ±5% 帯の判定クラスを算出する。

### 限界

- AVX-512（`dot_avx512::<PADDED_TAIL>`）・NEON（`dot_neon::<PADDED_TAIL>`）
  実機での tail A/B 実測は未実施（本開発環境は AVX2+FMA のみ）。対応 ISA を
  持つ環境（Issue #530 のチップ別前後比較）へ引き継ぐ。
- N=5 run はいずれも同一開発セッション内の連続実行であり、日をまたいだ
  再現性・別ホストでの再現性は未検証。

## スコープ外

- `DEFAULT_PADDED_TAIL` の `true` への切替（本 Issue の実測で Rejected と
  判断したため、専有環境実測でも覆らない限り再提案しない）
- 折り込み形の実装（オーナー判断待ち。上記「不採用形」参照）
- AVX-512／NEON 実機での実行時ディスパッチ検証（別タスク・Issue #530）
- `dot_kernel_bench.rs` の `memcpy` コード生成差異（`make simd-codegen-check`
  の検査ビルドとの乖離）の原因特定（本 Issue の範囲外。悪化の裏付けとしては
  十分なため踏み込まない）
