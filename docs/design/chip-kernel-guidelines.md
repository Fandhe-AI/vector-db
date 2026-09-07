# チップ別カーネル設計指針と Rust stable での実現可能性

- ステータス: 調査記録（Informational）。production コード無変更。toolchain
  更新・intrinsics 導入方針の決定は [#508 ADR](simd-intrinsics-adoption.md)
  （オーナー承認）が担う
- 対応: Issue #470（Phase 1 親 #456・ルート #455）
- 前提: `crates/engine/src/isa.rs`・`crates/engine/src/gpu_batch.rs`・
  [`docs/design/knn-stage-profile.md`](knn-stage-profile.md)・
  [`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md)・
  [`docs/design/core16-f16-resident-gate.md`](core16-f16-resident-gate.md)・
  [`docs/design/gpu-batch-wgpu-enablement.md`](gpu-batch-wgpu-enablement.md)

## 背景・目的

[`docs/design/hotpath-implementation-survey.md`](hotpath-implementation-survey.md)
の距離カーネル節が挙げる候補（f16 昇格・i8 VNNI・binary popcount・prefetch 等）
は、いずれも `#[target_feature]` 付き fn 内での `std::arch` intrinsics 使用が
前提になる。本 doc はチップ（Intel／AMD／Apple Silicon／ARM サーバー／GPU）別の
設計指針と、Rust stable でどこまで実現できるかを 2026-09-05 時点で調査した
記録として残す。「採用推奨」等の語はいずれも候補判定であり、承認・決定を
意味しない。

## 0. 機械検証した事実（2026-09-05・rustc 1.96.0）

### 0.1 現行 isa.rs は intrinsics 不使用（前提の訂正）

現行 `crates/engine/src/isa.rs` は `std::arch` intrinsics を一切使っていない。
実体は `#[target_feature(enable=...)]` を付けた safe fn の中で
`dot_lanes<const LANES>`（NEON=4／AVX2+FMA=8／AVX-512=16 のレーン幅ぶんの
アキュムレータ配列 `[f32; LANES]`）を `f32::mul_add` で積むのみで、実 SIMD 化は
LLVM の自動ベクトル化任せである。`unsafe` は ISA 検出の sealed トークン
（`NeonToken`/`Avx2FmaToken`/`Avx512Token`）経由での `#[target_feature]` fn 呼び
出し 3 箇所に限定される。したがって intrinsics 導入は「既存方針の延長」ではなく
**初導入**であり、`unsafe` 面積とユーザー承認コストの評価はこれを前提にする。

### 0.2 `#[target_feature]` fn 内での intrinsic 種別ごとの `unsafe` 要否

rustc 1.96.0（`#[target_feature(enable="avx2,fma")]` 等の safe fn 内）で
実コンパイル確認した結果:

| intrinsic の種別 | `unsafe` 要否 | 例 |
| ---------------- | ------------- | -- |
| 算術・set・シャッフル・水平和 | 不要（safe） | `_mm256_fmadd_ps` / `_mm256_set_ps` / `_mm256_add_ps` / `_mm_hadd_ps` |
| f16 昇格 | 不要（safe） | `_mm256_cvtph_ps` / `_mm_set_epi16` |
| i8 VNNI 内積 | 不要（safe） | `_mm256_dpbusd_avx_epi32` |
| binary popcount | 不要（safe） | `_mm512_popcnt_epi64` |
| マスク演算・reduce | 不要（safe） | `_mm512_maskz_mov_ps` / `_mm512_reduce_add_ps` |
| prefetch | 不要（safe） | `_mm_prefetch::<_MM_HINT_T0>(slice.as_ptr() as *const i8)`（`#[target_feature]` fn の内側限定。通常の fn から直接呼ぶと E0133。呼び出し元が新規 `unsafe` を追加できない場合は `core::hint::black_box` による早期 load で代替する——`core::hint::prefetch_read`〔`hint_prefetch` feature〕は stable 未安定化。Issue #490 で機械検証・`hnsw/prefetch.rs` に適用） |
| ポインタ load/store | 必要（unsafe） | `_mm256_loadu_ps` / `_mm256_storeu_ps`（E0133） |

結論: ポインタを取る load/store 以外は、`#[target_feature]` fn の内側であれば
すべて safe。新規 `unsafe` を要するのは「メモリからベクトルレジスタへ直接
load/store する」経路のみ。

### 0.3 `as_chunks` + `set` 構築による単一ロード命令への畳み込み

`as_chunks::<N>()` で得た固定長配列の要素から `_mm256_set_ps(...)` /
`_mm_set_epi16(...)` を構築すると、safe な経路のままロード相当の生成コードが
得られる（-O, `--emit asm` で確認した自前コンパイル出力）:

- f32: `set_ps` 8 個が単一の `vmovups (%rdi,%r8), %ymm4` へ畳み込まれる
  （`vinsertps`/`vunpcklps` の残留は自前コンパイル出力上 0 個）
- f16: `set_epi16` 8 個が `vcvtph2ps -16(%rdi,%r8), %ymm1`（メモリオペランド
  付き 1 命令）へ畳み込まれる（`vpinsrw` の残留は自前コンパイル出力上 0 個）

これにより f16／i8／binary／prefetch を含む特殊カーネルを、新規 `unsafe`
ゼロ・依存追加ゼロで実装できる可能性がある。**ただしこの畳み込みは LLVM の
最適化挙動であって言語仕様の保証ではない**。採用する場合は
`scripts/check_sort_determinism.sh` と同型の生成コード検査ガード（対象命令の
不在をアセンブリで検査）を CI に置くことを推奨する（既起票 #467 → 実装済み。
`scripts/check_simd_codegen.sh`・`docs/design/simd-codegen-guard.md` 参照）。

### 0.4 `std::simd`（portable SIMD）は stable では使えない

rustc 1.96.0 で `E0658: use of unstable library feature 'portable_simd'`
（gate: `portable_simd`／rust-lang/rust #86656）。nightly 必須のため不採用。

### 0.5 現行 `dot_lanes::<8>` は単一依存チェーン

`LANES=8` は 1 ベクタレジスタ幅（＝アキュムレータ 1 本）に相当し、8 レーンは
SIMD レーンであってアキュムレータの本数ではない。この構造的事実は
[`docs/design/knn-stage-profile.md`](knn-stage-profile.md) の段別プロファイルと
整合する。複数アキュムレータ化（ACC=2/4）の regime 別の効果は
[`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md)
の実測表（cache 常駐と arena 規模で挙動が異なる）を参照。

### 0.6 本開発環境は性能判定に使えない

本開発環境（QEMU Virtual CPU 報告・L2 48MiB/L3 16MiB という非現実的な階層）で
dot カーネルの A/B を交互実測したところ速度比が 1.01〜1.72x を無秩序に往復し、
規模・次元と単調な関係を示さなかった。これは Issue #365・#366 が記録した
「共有計測環境ではノイズと分離できない」という既往の結論と整合する。本 doc の
チップ別指針・優先順は、専有環境での再実測（既起票 #462 の計測規約）を経て
初めて採否判断の根拠になる。

## 1. チップ別設計指針

### 1-1. Intel

| 世代 | 実効 FMA 構成 | 推奨レーン幅 | f16／bf16／i8 経路 | 注意点 |
| ---- | ------------- | ------------ | ------------------- | ------ |
| Ice Lake-SP | 1×512 FMA（port 5 の 512b FMA は SKU 依存） | 512 bit | VNNI（`vpdpbusd`）有。BF16 無し | サーバー向けは降周波あり（下記注記参照。クライアントの実測を一般化しない） |
| Sapphire Rapids／Emerald Rapids | 2×512 FMA | 512 bit | VNNI・AVX-512 BF16（`vdpbf16ps`）・AMX（BF16/INT8 TMUL） | AMX は OS 有効化必須 |
| Granite Rapids | 2×512 FMA | 512 bit | 上記＋AMX-FP16 | 同上 |
| Alder Lake〜Arrow Lake（クライアント） | AVX-512 無し（fuse off） | 256 bit（AVX2+FMA） | AVX-VNNI（256bit）有。F16C 全世代有 | P/E コア混在。E コアへ移送されうる |
| Diamond Rapids | AVX10.2-512 | 512 bit | AVX10.2 の新 AI データ型 | 未発売・未確認 |

- 周波数低下: 上記の「1 コア稼働時に約 100 MHz のみの低下」（Travis Downs
  実測、二次資料）は **クライアント SKU**（Rocket Lake 等）での計測であり、
  Ice Lake-SP を含む第3世代 Xeon Scalable（サーバー SKU）へそのまま
  一般化できない。Intel の一次資料（Intel 64 and IA-32 Architectures
  Optimization Reference Manual §4.2〜4.2.1 の AVX-512 Power License 記載）
  によれば、サーバー SKU は依然として命令カテゴリ（Light／Heavy）・SKU・
  同時稼働コア数に応じた Power Level（License 0〜2）遷移とそれに伴う
  周波数低下を持つ。Ice Lake-SP の FMA カーネル設計では、クライアントの
  「降周波はほぼ無視できる」という結論を流用せず、対象 SKU の稼働コア数・
  命令の Light／Heavy 分類ごとに一次資料または自環境の実測で周波数レベルを
  確認すること
- AMX の OS 有効化（Linux）: `arch_prctl(ARCH_REQ_XCOMP_PERM, XFEATURE_XTILEDATA)`
  必須。XFD により既定無効
- キャッシュ律速の目安（導出値・実測ではない）: dim=128 の f32 行は 512 B。
  L2 1〜2 MB → 約 2,000〜4,000 行、L3 32〜120 MB → 約 65,000〜240,000 行で
  L3 を溢れる。f16 なら 2 倍、i8 なら 4 倍。本リポの実測点（25k／100k 行）は
  L2 を超え L3 境界を跨ぐ領域

### 1-2. AMD

| 世代 | AVX-512 データパス | 推奨レーン幅 | f16／bf16／i8 | 注意点 |
| ---- | ------------------- | ------------ | -------------- | ------ |
| Zen 3 | 無し | 256 bit | — | AVX2+FMA のみ |
| Zen 4（Genoa／Ryzen 7000） | double-pump（256 bit HW へ 512 bit 命令を 2 サイクル投入）。load 1×512b/cycle・store 0.5×512b/cycle | 512 bit 命令可（降周波なし） | `avx512_bf16`・VNNI 有 | 「512 bit にすれば 2 倍」にはならない |
| Zen 5 デスクトップ／Turin | フル 512 bit。4×512b EU、load 2×512b/cycle・store 1×512b/cycle | 512 bit | 同上 | Zen 4 比で load 帯域 2 倍 |
| Zen 5 モバイル（Strix Point） | 256 bit のまま | 512 bit 命令可・実行幅 256 | 同上 | 「Zen 5 = フル 512」は半数の製品で誤り |

AMD はライセンス降周波を持たずサーマルベースのみ。3D V-Cache・CCD 跨ぎ帯域が
brute-force に与える影響は一次実測未確認。

### 1-3. Apple Silicon

| 項目 | 内容 |
| ---- | ---- |
| SIMD 実行 | Firestorm（M1 P コア）は 4 本の FP/SIMD ユニット（128 bit NEON）。8-wide decode（二次資料） |
| キャッシュ | L1 192 KB(I)+128 KB(D)/コア、L2 12 MB 共有 |
| f16 算術 | NEON FP16（`fmla` f16）有 |
| i8 dot | `sdot`/`udot`（FEAT_DotProd）有 |
| bf16 | `bfdot`（FEAT_BF16）。M シリーズでの有無は §8.3（Issue #468 実機検出結果表）参照 |
| SME／SME2 | M4 で対応。通常の SVE は非対応（Streaming SVE のみ） |
| Apple AMX | 非公開命令。Accelerate 経由のみ |
| UMA／Metal | `wgpu` Metal backend は `SHADER_F16` 対応 |

M4 の SME 有効ベクタ長・`fmopa` スループットは未確認。

### 1-4. ARM サーバー

| プラットフォーム | コア | ベクタ長 | 備考 |
| ---------------- | ---- | -------- | ---- |
| AWS Graviton3 | Neoverse V1 | SVE 256 bit | Graviton4 比 33% 多くロード可 |
| AWS Graviton4 | Neoverse V2 | SVE2 128 bit | L2/コア 2 倍 |
| NVIDIA Grace | Neoverse V2 | SVE2 128 bit | 詳細未確認 |
| Ampere Altra | Neoverse N1 | SVE 非対応（NEON のみ） | — |
| AmpereOne | 独自コア | SVE 対応状況未確認 | — |

Graviton4／Grace は SVE2 でも 128 bit のため NEON と理論ピークが同じ。SVE 化の
利得は述語処理と可搬性にある。

### 1-5. GPU（wgpu 30.0.1）

| 項目 | 状況 |
| ---- | ---- |
| f16 算術 | `Features::SHADER_F16`（Vulkan／Metal／DX12／WebGPU）。WGSL `enable f16;` |
| i8 dot | WGSL `dot4I8Packed`／`dot4U8Packed`。naga が全 backend 実装（SPIR-V／HLSL／Metal は専用命令、他は polyfill）。専用命令化は DX12 SM≥6.4／Vulkan `VK_KHR_shader_integer_dot_product` |
| `NATIVE_PACKED_INTEGER_DOT_PRODUCT` | wgpu-types 30.0.1 に該当 feature 定数が存在しないことを確認済み（#542）。専用命令／polyfill の判別は `Adapter::as_hal`（`unsafe`）経由でのみ可能で、本リポの `unsafe` 原則禁止のため未判別のまま（詳細: [`gpu-batch-i8-packed.md`](gpu-batch-i8-packed.md) §4） |
| Subgroup | `Features::SUBGROUP`（Vulkan／DX12／Metal）。GPU 側 Top-k 縮約に有効。実機確認・設計は [`gpu-batch-topk.md`](gpu-batch-topk.md)（#535） |
| bf16 | 未確認 |

## 2. Rust stable での実現可能性

### 2-A. stable 1.96 以上で使える

| 機能 | 代表 intrinsic | stable since | 検証 |
| ---- | --------------- | ------------ | ---- |
| safe fn への `#[target_feature]` | — | 1.86.0（`target_feature_11`） | §0.2 で実コンパイル確認済み |
| F16C | `_mm256_cvtph_ps` | 1.68.0 | §0.2・§0.3 で実コンパイル確認済み |
| AVX-512（F/BW/DQ/VL 等） | — | 1.89.0 | docs 参照 |
| AVX-512 VNNI | `_mm512_dpbusd_epi32` | 1.89.0 | docs 参照 |
| AVX-512 BF16 | `_mm512_dpbf16_ps` | 1.89.0 | docs 参照 |
| AVX-VNNI（256 bit） | `_mm256_dpbusd_avx_epi32` | 1.89.0 | §0.2 で実コンパイル確認済み |
| AVX-512 FP16 | `_mm512_fmadd_ph` | 1.94.0（`f16` プリミティブ依存分は除く） | docs 参照 |
| NEON FP16 | `vfmaq_f16` | 1.94.0 | docs 参照 |
| NEON FMLAL | `vfmlalq_low_f16` | 1.94.0 | docs 参照 |
| 実行時検出（x86） | `is_x86_feature_detected!` | `avx512vnni`／`avx512bf16`／`avx512fp16`／`avxvnni`／`f16c`／`amx-*`／`avx10.*` を受理 | docs 参照 |
| 実行時検出（aarch64） | `is_aarch64_feature_detected!` | `fp16`／`fhm`／`dotprod`／`bf16`／`i8mm`／`sve`／`sve2`／`sme`／`sme2` を受理 | docs 参照 |

### 2-B. stable 1.98 で追加

`rust-toolchain.toml` は `channel = "stable"`（浮動）で、機械検証は
ローカル rustc 1.96.0 で行った。1.98 系の項目はリリースノート参照のみで
ローカル未検証。toolchain 更新は本 Issue のスコープ外（#508 の対象）。

| 機能 | 代表 intrinsic | stable since |
| ---- | --------------- | ------------ |
| NEON dot product（i8） | `vdotq_s32`／`vdotq_u32` | 1.98.0 |

### 2-C. nightly のみ（不採用）

| 機能 | gate |
| ---- | ---- |
| Intel AMX | `x86_amx_intrinsics`（rust-lang/rust #126622） |
| AArch64 SVE／SVE2 | `stdarch_aarch64_sve`（#145052。2026 プロジェクトゴールでも nightly 継続の見込み） |
| `f16` プリミティブ | #116909 |
| `std::simd` | `portable_simd`（#86656。1.96 で E0658 を実機確認） |

### 2-D. 未確認

- NEON bf16（`vbfdotq_f32`）: stable docs に該当ページ無し。少なくとも 1.96 で
  は利用不可
- SME／SME2 intrinsics: Rust に API 無し。実行時検出も std_detect 内部には
  実装があるが、`is_aarch64_feature_detected!` マクロ経由の利用は
  `stdarch_aarch64_feature_detection` 機能ゲート未安定のため stable では
  コンパイル不可（`crates/engine/examples/detect_features.rs` で
  `"n/a (unstable macro)"` 固定として実確認済み。§8.5 参照）
- `is_aarch64_feature_detected!` の macOS 上の実効性: §8（Issue #468）で静的解析＋
  GitHub ホステッド Apple Silicon 実機（仮想化。§8.3 参照）で確認済み
  （オーナー実機は §8.5 へ申し送り）。既存 NEON 経路は Apple ターゲットで常に有効

### 2-E. 候補クレート（情報のみ）

依存の追加・更新は `.claude/rules/dependency-policy.md` によりユーザー承認制。
本 Issue では依存を追加しない。

| クレート | 版 | ライセンス | 備考 |
| -------- | -- | ---------- | ---- |
| simsimd | 6.5.16 | Apache-2.0 | C ビルド必須。推移的依存未確認 |
| half | 2.7.1 | MIT OR Apache-2.0 | `f16` プリミティブ未安定のため CPU f16 経路の現実解 |
| pulp | 0.22.3 | MIT | safe generic simd |
| wide | 1.7.0 | Zlib OR Apache-2.0 OR MIT | — |
| rayon | 1.12.0 | MIT OR Apache-2.0 | 自作 `parallel_search.rs` があるため不要 |

## 3. 追加カーネルの優先順

Issue #365 で行内マルチアキュムレータ化は不採用済み（cache 常駐 dim100/dim128
の小次元で悪化。dim384 は cache 常駐で改善・arena 規模で非劣化）。残る有効な
レバーは (a) 行間マイクロカーネル、(b) 格納精度の削減（メモリ律速側）、
(c) blocking／prefetch。

| 優先 | 施策 | 対象 | 根拠 | Rust | 既起票 |
| ---- | ---- | ---- | ---- | ---- | ------ |
| 1 | CPU f16 常駐＋F16C／NEON FP16 デコード | 全 | GPU 側の f16x2 常駐表現を CPU 側でも読めば arena 半減。L3 溢れ点が拡大 | stable 可 | #513（#514 実装済み。`docs/design/hnsw-f16-resident.md` 参照） |
| 2 | 行間マイクロカーネル（4〜8 行 × 1 クエリ） | 全 | #365 が潰したのは行内 ILP。行間の load 削減は別軸。Zen 5 の load 2×512b で特に効く | stable 可 | #509（#510・#511 実装済み） |
| 3 | クライアント Intel の 256 bit 経路最適化 | Alder〜Arrow Lake | AVX-512 fuse off がクライアント主流。#520 の AVX-VNNI（256 bit）側に含まれる | stable 可 | #520 |
| 4 | AVX-512 BF16／VNNI 量子化スキャン | SPR／GNR／Zen 4／5 | ANN 候補生成限定で f32 再計算（HNSW の rescoring 契約と同型） | stable 1.89 | #520 |
| 5 | AVX-VNNI i8 | クライアント Intel | 4 と同一 Issue（#520）の 256 bit 版 | stable 1.89 | #520 |
| 6 | NEON `sdot`/`udot` i8 | Apple／Graviton／Grace | 4 の Arm 版 | stable 1.98 | #524 |
| 7 | AVX-512 FP16 ネイティブ | SPR／GNR | 変換コストも省く。対応チップ限定。#513（F16C デコード）とは別方式で未起票 | stable 1.94 | — |
| 8（低） | AMX／SME／SVE | SPR+／M4／Graviton | 細長い形状に不向き・OS 有効化・Rust API 不在 | nightly／不可 | — |

## 4. GPU 経路（`gpu_batch.rs`）の改善候補

| 優先 | 候補 | 内容 | 既起票 |
| ---- | ---- | ---- | ------ |
| 1 | マルチクエリ dispatch＋クエリの workgroup 常駐 | 現状はクエリごとに行列全体を再読み込み（行列トラフィック Q 倍） | #531 |
| 2 | GPU 側 Top-k | 行数分の f32 全量 readback（最大 32 MiB）を k×workgroup 数へ。`SUBGROUP` で縮約。実機確認・設計は [`gpu-batch-topk.md`](gpu-batch-topk.md)（#535） | #534 |
| 3 | `SHADER_F16` ネイティブ f16 FMA | 現状は unpack して f32 演算。[`docs/design/core16-f16-resident-gate.md`](core16-f16-resident-gate.md) の環境依存があるため A/B 必須 | #538 |
| 4 | i8 量子化＋`dot4I8Packed` | dim=128 が 32 words。`NATIVE_PACKED_INTEGER_DOT_PRODUCT` の有無は実機確認が要る | #541 |
| 5 | Apple UMA ゼロコピー | [`docs/design/redb-insert-reserve-zero-copy.md`](redb-insert-reserve-zero-copy.md)（Issue #400）の先例に倣い静的確認済み（wgpu 30.0.1 は staging 経由で真のゼロコピーは不成立。詳細は [`gpu-batch-phase5-before-after.md`](gpu-batch-phase5-before-after.md) §6） | #544（静的確認済み・実装見送り） |

## 5. 既 Rejected との関係

Issue #365（行内複数アキュムレータ）は cache 常駐 dim100/dim128 の小次元で
悪化したことを理由に不採用としたが（dim384 は cache 常駐で改善・arena 規模で
非劣化）、arena 規模かつ dim>=768 限定では改善が確認されている。
本 doc §3 の優先 1〜7 はいずれも dim>=768 限定ディスパッチ（#517。dot カーネルの
実装は #518 で完了・[`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md)
「Issue #518 追記」節参照。#519 で本環境（共有 QEMU）の参考値を実測済み。
閾値確定はオーナー実機〔#530〕）や量子化
opt-in 経路（#520 等）に閉じており、#365 が不採用とした「全 dim 一律の複数
アキュムレータ化」を再提案するものではない。詳細な対応表は
[`docs/design/hotpath-implementation-survey.md`](hotpath-implementation-survey.md)
§10 を参照。

## 6. 出典

- Rust: https://doc.rust-lang.org/std/arch/macro.is_x86_feature_detected.html ／
  macro.is_aarch64_feature_detected.html ／
  core/arch/x86_64/fn._mm512_dpbf16_ps.html ／ fn._mm512_dpbusd_epi32.html ／
  fn._mm256_dpbusd_avx_epi32.html ／ fn._mm512_fmadd_ph.html ／
  fn._mm256_cvtph_ps.html ／ core/arch/aarch64/fn.vdotq_s32.html ／
  fn.vfmaq_f16.html ／ fn.vfmlalq_low_f16.html ／
  https://releases.rs/docs/1.98.0/ ／ rust-lang/rust
  #136058・#134090・#111137・#127213・#136306・#117224・#126622・#145052・
  #116909・#86656 ／
  https://rust-lang.github.io/rust-project-goals/2026/scalable-vectors.html
- Intel: AVX10 技術資料（cdrdv2-public.intel.com/849709）／ Alder Lake AVX-512
  fuse off（support article 000089918）／ AMX solution brief ／
  https://docs.kernel.org/arch/x86/xstate.html ／
  https://travisdowns.github.io/blog/2020/01/17/avxfreq1.html ／
  https://travisdowns.github.io/blog/2020/08/19/icl-avx512-freq.html ／
  https://chipsandcheese.com/p/a-peek-at-sapphire-rapids
- AMD: https://www.amd.com/en/blogs/2026/understanding-avx-512---validating-usage-on-amd-epyc-.html ／
  https://www.numberworld.org/blogs/2024_8_7_zen5_avx512_teardown/ ／
  https://www.hwcooling.net/en/mobile-zen-5-is-here-ryzen-ai-300-strix-point-soc-detailed/ ／
  https://chipsandcheese.com/p/zen-5s-avx-512-frequency-behavior
- Arm／Apple: https://aws.github.io/graviton/ ／
  https://www.lkuffo.com/graviton3-better-than-graviton4-vector-search/ ／
  https://old.chipsandcheese.com/2024/07/22/arms-neoverse-v2-in-awss-graviton-4/ ／
  https://dougallj.github.io/applecpu/firestorm.html ／
  https://developer.apple.com/forums/thread/757704
- GPU／wgpu: https://docs.rs/wgpu/30.0.1/wgpu/struct.FeaturesWebGPU.html ／
  struct.FeaturesWGPU.html ／ gfx-rs/wgpu #7494・#7574・#7595

## 7. 結果記録テンプレート（Issue #469）

本開発環境（QEMU 仮想 CPU・AVX-512 なし・NEON なし。§0.6）では Phase 4 の
採否判定に必要な実測ができないため、オーナー実機（Apple M／AMD Zen 4・5／
Intel）での手動計測が必要になる。手順は README「チップ別カーネルの実測手順
（Issue #469）」・`make bench-chip`（`crates/engine/benches/chip_bench.rs`）・
`docs/design/benchmark-judgement-policy.md` §3〜§5 を参照。本節はその結果を
チップ横断で比較可能な形式に揃えるための記録テンプレートである。

### 7.1 環境記録表（1 計測 = 1 行）

| チップ名／世代 | OS | 検出 ISA（`engine::isa`） | runtime_features 要点 | L1d／L2／L3 | nproc | rustc | commit（before/after） | dedicated_env_attested | rounds／meets_policy_min_rounds | loadavg 範囲 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| （未計測） | | | | | | | | | | |

### 7.2 結果記録表（区間 ↔ policy §7.2 の列と 1:1）

| 区間（`summary.json` メトリクスキー） | before min | before median | after min | after median | ratio (min-of-N) | 判定クラス（`classify_change(ratio, 0.05)`） | 参照区間帯（`reference_band_pct`） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| （未計測） | | | | | | | | |

### 7.3 参照区間の指定（施策別）

dot カーネル変更（#517・実装は #518。前後比較・閾値候補実測は #519。本環境参考値は
`docs/design/dot-kernel-multi-accumulator.md`「Issue #519 追記」節・確定はオーナー
実機〔#530〕）の参照区間は、dot を通らない `feature_bench` フェーズ
（例: `agg_count`・`explain`・`where_compound`）と `knn_profile` の
`S1_redb_scan`／`S2_header_decode`（`chip_bench` の `knn_profile` ワークロードが
同時に計測する）を用いる。f16 常駐・行間マイクロカーネル等、施策ごとの
「対象区間 → 参照区間」対応は #509・#513・#517・#520・#524・#527 側で個別に
定義し、本 doc へはポインタのみを残す（行ブロックカーネル〔#510・#511〕の
対象区間 `S5_search_parallel` ↔ 参照区間 `S1_redb_scan` 対応と実測は
Issue #512・`docs/design/dot-kernel-multi-accumulator.md`「行間再利用
（Issue #512）」節参照）。

### 7.4 `summary.json` キー一覧

`chip_bench` の出力スキーマ（`schema_version: 1`）。キー追加は minor 互換、
既存キーの意味変更は `schema_version` を繰り上げる。

- `rounds`／`policy_min_rounds`／`meets_policy_min_rounds`（`rounds` が
  `docs/design/benchmark-judgement-policy.md` の最小ペア数〔5〕未満なら
  `false`。参考値の自己ラベル）
- `dedicated_env_attested`（`BENCH_DEDICATED_ENV=1` の自己申告。自動検出はしない）
- `workloads`（実行したワークロードの固定順配列）
- `build.{commit,dirty,rustc,host}`
- `env.{os,arch,logical_cpus,detected_isa}`・`env.cpu.{model_name,flags,caches,sysctl}`・
  `env.runtime_features`（`is_x86_feature_detected!`／
  `is_aarch64_feature_detected!` の結果。#468 の材料であり本 doc は結論を書かない）
- `runs[]`（`round`・`workload`・`loadavg_before`・`exit_code`・`stdout_log`）
- `results.<workload>.metrics.<key>.{values,min,median,max,reference_band_pct}`
  （`dot_kernel`: `<working_set>/dim=<n>/{ns_per_dot,median_ms}`・
  `diagnostic_ab/simd_vs_scalar_ratio`。`knn_profile`:
  `<stage>/{median_ms,ns_per_row}`。`feature_128`／`feature_768`:
  `<phase>/{min_us,p50_us,p95_us}`）
- `raw_logs_dir`（`<BENCH_CHIP_OUT_DIR>` からの相対パスのみ。絶対パス・
  ホスト名・ユーザー名は含まない）

### 7.5 チップ別空テンプレート

以下はいずれも未計測（本 Issue の実装時点では起票のみ）。各小節の実測は
オーナー実機での `make bench-chip` 実行後にオーナー／管理者が追記する。

- **Apple M1／M2／M3／M4**（`sysctl machdep.cpu.brand_string`・
  `hw.perflevel{0,1}.physicalcpu`・`hw.optional.arm.FEAT_*` を記録列に含める）
- **AMD Zen 4**（Ryzen 7000／EPYC Genoa）
- **AMD Zen 5**（デスクトップ／EPYC Turin／Ryzen AI 300 Strix Point）
- **Intel Ice Lake-SP**
- **Intel Sapphire Rapids／Emerald Rapids**
- **Intel Alder Lake〜Arrow Lake**

各小節共通の注記: 本開発環境（QEMU）の値は参考値であり production 変更の
採否根拠にしない（`docs/design/benchmark-judgement-policy.md` §5）。macOS 上の
`is_aarch64_feature_detected!` の実効性（true/false の実機確認）は Issue #468
（§8 参照）の担当。Phase 4 通しのチップ別前後比較・最速判定は Issue #530 の担当。

### 7.6 本環境での完走記録（参考値）

`make bench-chip`（既定 5 ラウンド・全 4 ワークロード）を本開発環境（QEMU
仮想 CPU）で 1 回実行し、以下の環境ブロックを得た（実測値は公開可能な範囲
——[spec-confidentiality](../../.claude/rules/spec-confidentiality.md) 参照
——だが、共有 QEMU 環境の絶対値のため §7.5 の各チップ小節へは転記しない）。

```json
{
  "os": "linux", "arch": "x86_64", "logical_cpus": 12, "detected_isa": "Avx2Fma",
  "cpu": {
    "model_name": "QEMU Virtual CPU version 2.5+",
    "flags": ["fma", "sse4_2", "avx", "f16c", "avx2"],
    "caches": [
      {"level": 1, "type": "Data", "bytes": 32768},
      {"level": 1, "type": "Instruction", "bytes": 32768},
      {"level": 2, "type": "Unified", "bytes": 4194304},
      {"level": 3, "type": "Unified", "bytes": 16777216}
    ]
  },
  "runtime_features": {
    "avx2": true, "fma": true, "f16c": true,
    "avx512f": false, "avx512bw": false, "avx512vl": false,
    "avx512vnni": false, "avx512bf16": false, "avx512fp16": false, "avxvnni": false
  }
}
```

`rounds: 5`・`meets_policy_min_rounds: true`。所要時間は本環境で数分程度
（環境依存のため参考値）。全 4 ワークロード × 5 ラウンドが exit_code 0 で
完走し、`summary.json`・per-run ログが `target/bench-chip/<unix-ts>/` に
出力されることを確認した。

### 7.7 Apple M 実機での i8／f16／f32 経路の前後比較（Issue #526）

#### 7.7.1 目的・#530 との境界

Phase 4（#459）の Apple 向け i8 経路（#520〜#525）・f16 経路（#513〜#516）は
いずれも実装・単体テスト・クロスコンパイル確認（`make check-cross`・
`make simd-codegen-check-cross`）・GitHub ホステッド `macos-latest` での
`detect-apple` ジョブ（`isa::current_f16()`／`current_i8()` が期待どおり
`NeonFp16`／`NeonDotprod` を返すことの確認）までが実施済みで、**Apple 実機
での性能前後比較（レイテンシ・常駐メモリ）は本 Issue（#526）・#530 の担当
として未実施**のまま申し送られていた（`docs/design/hnsw-f16-resident.md`
「既知の限界」節・`docs/design/hnsw-sq8-resident.md`「Issue #525」節「検証の
限界」参照）。

本節は、Apple M 実機で 3 精度（f32〔`hnsw`〕・f16〔`hnsw_f16`〕・i8
〔`hnsw_i8`〕を同一計測セッション・同一ノイズ帯で計測できる手順・記録
テンプレートを整備する。**「前後」の意味は commit 前後ではなく同一
バイナリでの arm 比較**であり、f16／i8 常駐は構築時 opt-in
（`ValidatedHnswParams::with_resident_precision`）なので before を
`hnsw`（f32 常駐）、after を `hnsw_f16`／`hnsw_i8` として扱う（#516・#523 と
同じ方式）。補助系列として `brute_force` を各 arm の直前に計測し、
参照区間（run-to-run ノイズ帯の基準）は同一プロセスが同時に測る
`COUNT(*)`（`S0prime_count_star`。`hnsw_params:` を通らないため精度・
エンジン非依存）を用いる。

本 Issue が担うのは (a) Apple 実機で 3 精度比較を 1 コマンドで再現可能に
する計測ドライバ・非 vacuous 証跡の整備、(b) 本節の記録テンプレート・
手順、(c) 本開発環境（QEMU x86_64）での同一ドライバの完走確認（x86 参考値）
までであり、**Apple 実機での実測値そのものはオーナー申し送り**（下記
7.7.7 参照）。Phase 4 通しの 3 チップ比較・`vector_knn` crossdb 再計測は
Issue #530 の担当。

#### 7.7.2 手順（オーナー向け）

1. `make detect-features`（`.github/workflows/detect-features.yml`
   `detect-apple` ジョブと同型。§8.3 の実機検出結果表へ転記）
2. `rustup update stable`（stale な toolchain での計測を避ける）
3. `BENCH_DEDICATED_ENV=1 make bench-knn-precision-resident`
   （既定 5 規模点〔25k／100k／500k × dim128、25k／100k × dim768〕・
   `AB_PAIRS` 既定 5。`hnsw`・`hnsw_f16`・`hnsw_i8` の 3 arm を同一セッション
   で計測する。所要時間は環境依存だが本環境の縮小規模〔7.7.6〕実績から
   フル規模点では相応の時間を要すると見込まれる）
4. `scripts/bench_knn_f16_resident_ab.sh --summarize <target/bench-knn-precision-resident/<UTC ts>>`
   で per-run 生データを TSV へ集約
5. TSV・env.txt を `docs/design/bench-data/hnsw-precision-resident-ab/<ts>-{env.txt,summary.tsv}`
   へ保存（相対パス・生データのみ。絶対パス・ホスト名・ユーザー名を含めない）
6. 手計算（min-of-N・median・ratio・`classify_change`）で 7.7.4 の表へ転記
7. 任意で `make bench-chip` を実行し環境ブロック・`dot_kernel` 参照区間を
   補完（§7.1〜§7.2 の表と同じ形式）

`AB_MEMORY_POINTS` 既定（`AB_POINTS` ＋ `20:768`）の 500k×768 点は
索引単体メモリ計測で VmHWM 約 4.7 GiB に達する（`docs/design/
hnsw-f16-resident.md`「Issue #516 追記」節参照）。Apple 実機のメモリ量に
応じて `AB_MEMORY_POINTS` を縮小することを推奨する。

#### 7.7.3 環境記録表

§7.1 の列に加え、macOS 固有の記録列（`scripts/bench_knn_f16_resident_ab.sh`
の `env.txt`。`crates/engine/benches/chip_bench.rs::collect_cpu_info` と
同じ `sysctl` キー集合を使う best-effort 収集）を含める。

| チップ名／世代 | OS（`sw_vers`） | `uname -m` | `brand_string` | perflevel0／1 physicalcpu | `FEAT_FP16`／`FEAT_DotProd`／`FEAT_BF16`／`FEAT_I8MM`／`FEAT_SME` | nproc | rustc | commit | `kernel_isa`（dot／f16／i8） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| （未計測） | | | | | | | | | |

#### 7.7.4 結果記録表

行 = 規模点（`scale:dim`）× arm（`hnsw`／`hnsw_f16`／`hnsw_i8`）、列は
policy §7.2 と同じ形式（`docs/design/benchmark-judgement-policy.md`）。

| 規模点 | arm | before（`hnsw`）min | before median | after min | after median | ratio (min-of-N) | ratio (median) | 判定クラス（`classify_change(ratio, 0.05)`） | 参照区間帯（`S0prime_count_star`） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| （未計測） | | | | | | | | | |

索引単体メモリ表（`approx_heap_bytes`。macOS では `proc_stats.rs` が
`/proc/self/status` に依存するため VmRSS／VmHWM は取得できず
`unavailable` と出力される——`approx_heap_bytes` は決定的な集計値のため
この欠落の影響を受けない）:

| 規模点 | 要求精度 | 実効精度 | `approx_heap_bytes` | `vm_rss_kb_before` | `vm_rss_kb_after` | `vm_hwm_kb` |
| --- | --- | --- | --- | --- | --- | --- |
| （未計測） | | | | unavailable（macOS） | unavailable | unavailable |

#### 7.7.5 非 vacuous チェックリスト

各 arm の per-run ログで以下を確認してから表へ転記する
（`knn_profile_bench.rs::run_hot_only` が計測値出力の前に fail-closed 検証
として出力する行。いずれか欠落・不一致であれば計測をやり直す）:

- `resident_precision requested=<f32|f16|i8> search_engine_kind=hnsw(...,resident=<同値>...)`
- `hnsw_stats builds>0 hits>0 build_failures=0`
- `f16_residency_fallbacks=0`（`hnsw_f16` arm のみ。非 0 は D6 自動縮退が
  発生し精度計測として無意味であることを示す）
- `i8_residency_fallbacks=0`（`hnsw_i8` arm のみ。同上）
- `kernel_isa dot=Neon f16=NeonFp16 i8=NeonDotprod`（Issue #526。Apple 実機で
  実際に NEON 系カーネルへディスパッチされたことの証跡。本環境〔x86_64
  QEMU〕では `dot=Avx2Fma f16=F16c i8=Avx2Widen` が期待値——下記 7.7.6 参照）

#### 7.7.6 本環境（QEMU x86_64）スモーク結果（参考値・完走確認）

`AB_POINTS="1:128" AB_MEMORY_POINTS="1:128" AB_PAIRS=5
AB_CANDIDATE_ENGINES="hnsw_f16 hnsw_i8" make bench-knn-precision-resident`
（縮小規模点 1 点のみ。500k×768 の索引単体メモリ点は本開発環境〔共有 QEMU
VM〕での所要時間の都合で `AB_MEMORY_POINTS` を明示的に絞り本スモークの
対象外とした——hot-only 30 run ＋ memory 6 run〔1:128 点 × 3 arm × 2 rep〕）
を本開発環境（QEMU 仮想 CPU）で実際に実行し、全 36 run が exit_code 0 で
完走することを確認した。per-run 生データは下記のとおり
`docs/design/bench-data/hnsw-precision-resident-ab/` へ保存済み。

**この結果は x86_64・`F16c`／`Avx2Widen` 経路の参考値であり Apple の数値
ではない。production 変更・Apple 側の判断の採否根拠にしない**
（`docs/design/benchmark-judgement-policy.md` §5）。

- 環境: `cpu_model=QEMU Virtual CPU version 2.5+`・`nproc=12`・
  `rustc_version=1.98.1`
- `kernel_isa dot=Avx2Fma f16=F16c i8=Avx2Widen`（全 36 run で一貫。Apple
  実機では `dot=Neon f16=NeonFp16 i8=NeonDotprod` となることが期待される）
- `resident_precision`・`hnsw_stats builds>0 hits>0`・
  `f16_residency_fallbacks=0`・`i8_residency_fallbacks=0` をいずれの arm でも
  確認（7.7.5 のチェックリストが本環境で全て満たされることを確認）
- per-run 生データ（TSV・env.txt）は
  `docs/design/bench-data/hnsw-precision-resident-ab/20260907T190235Z-{env.txt,summary.tsv}`
  へ保存済み（相対パス・生データのみ。絶対パス・ホスト名・ユーザー名は
  含まない）

参考値として、実測した 1:128（25,000 行・dim 128）点の hot-only レイテンシ・
索引単体メモリを下記に記録する（7.7.4 の Apple 行の記入例も兼ねる。
判定規約は `docs/design/benchmark-judgement-policy.md` §3〜§4）:

| arm | hot min (ms) | hot median (ms) | ratio (min-of-N, vs hnsw) | ratio (median) | 固定 ±5% 帯判定 | 参照区間帯（`COUNT(*)`） |
| --- | --- | --- | --- | --- | --- | --- |
| hnsw（f32） | 0.427 | 0.431 | 1.0000 | 1.0000 | — | 0.82%（n=10・生データの中央値 0.053751〜0.054191ms から算出） |
| hnsw_f16 | 0.422 | 0.426 | 0.9883 | 0.9884 | Neutral | 2.03%（n=10・生データの中央値 0.053735〜0.054828ms から算出） |
| hnsw_i8 | 0.419 | 0.426 | 0.9813 | 0.9884 | Neutral | 0.21%（n=10・生データの中央値 0.053751〜0.053864ms から算出） |

| arm | `approx_heap_bytes`（rep1/rep2 ビット同一） | ratio (vs hnsw) | 削減率 |
| --- | --- | --- | --- |
| hnsw（f32） | 17,226,056 | 1.0000 | — |
| hnsw_f16 | 10,826,056 | 0.6285 | 37.15% |
| hnsw_i8 | 7,726,568 | 0.4485 | 55.15% |

`hnsw`／`hnsw_f16` の値は `docs/design/hnsw-f16-resident.md`「Issue #516
追記」節の 1:128 行（`hnsw`=17,226,056・`hnsw_f16`=10,826,056）と完全一致
しており、本スモークが同一の測定経路を再現していることの裏付けになる。
本節の数値はいずれも x86_64 QEMU の参考値であり、Apple M 実機の数値では
ない（上記のとおり判断根拠にしない）。

#### 7.7.7 Apple 行（未計測・オーナー申し送り）

7.7.3・7.7.4 の Apple M 行は本 Issue の実装時点では未計測。7.7.2 の手順で
オーナーが実測し追記する。既定常駐精度（f16／i8 いずれかへの反転）の
採否判断は #515／#516／#523 と同じくオーナー判断のまま（本 Issue は判断
材料を追加しない）。

## 8. macOS 上の `is_aarch64_feature_detected!` 実効性（Issue #468）

### 8.1 目的と結論

`is_aarch64_feature_detected!` マクロの doc には「linux 系以外の OS では多くの
feature の実行時検出が常に `false` を返す」という注記があり、これが真であれば
Apple Silicon 上で `crates/engine/src/isa.rs::NeonToken::try_new` が `None` を
返し既存 NEON カーネルが fail-closed 側で無効化される懸念があった。

**結論（静的解析＋実機。GitHub ホステッド Apple Silicon で確認済み。オーナー
実機は §8.5 へ申し送り）**: `aarch64-apple-darwin` ターゲットは `neon`／`fp16`／
`fhm`／`dotprod` をコンパイル時 target_feature として含むため、これら 4
feature については `is_aarch64_feature_detected!` マクロが `cfg!(target_feature
= ...) || __is_feature_detected::…()` へ展開されコンパイル時に定数 `true` へ
畳み込まれる（マクロ doc の注記は Darwin についてはこの意味で陳腐化した記述で
あり、Darwin 向けの実行時検出実装〔`sysctlbyname` 経由〕自体は std_detect に
存在する）。よって **既存 NEON 経路は Apple Silicon 上で常に有効**。`bf16`／
`sme` はコンパイル時 target_feature に含まれないため std の `sysctlbyname`
経由の実行時検出に落ちる（M シリーズ世代依存。§8.3 参照）。有効化の根拠は
実行時 sysctl ではなくコンパイル時 target_feature（`neon`／`fp16`／`fhm`／
`dotprod`）である点は §8.3 の実機観測（`hw.optional.AdvSIMD` unknown oid）でも
裏付けられた。

### 8.2 根拠（機械確認事実）

| 確認項目 | 結果 |
| -------- | ---- |
| マクロ展開 | `is_aarch64_feature_detected!(X)` は `cfg!(target_feature = X) \|\| __is_feature_detected::X()` に展開される（コンパイル時に有効な feature は定数 `true`） |
| `aarch64-apple-darwin` のコンパイル時 target_feature | `neon`・`fp16`・`fhm`・`dotprod` を含む（`bf16`・`sme`・`i8mm` は含まない。`rustc --print cfg --target aarch64-apple-darwin` で確認） |
| `aarch64-unknown-linux-gnu` の同項目 | `neon` のみ |
| Darwin の実行時検出実装 | std_detect の Darwin 向け実装が `sysctlbyname`（`hw.optional.AdvSIMD`／`hw.optional.arm.FEAT_FP16`／`FEAT_FHM`／`FEAT_DotProd`／`FEAT_BF16`／`FEAT_SME`／`FEAT_SME2`／`FEAT_I8MM` 等）で検出する |
| 実機（§8.3）での `hw.optional.AdvSIMD` | GitHub ホステッド runner（macOS 26.5.2・仮想化）では `sysctl: unknown oid` で参照不可。std_detect の Darwin 実装が `asimd` 判定に使う OID と同一であり、この環境では実行時 `asimd` 検出は成立しない。それでも `is_aarch64_feature_detected!("neon")` が `true` を返すのはコンパイル時 target_feature の短絡によるもので、実行時 sysctl 経由ではない |

コードは転記しない（出典は std_detect の該当ソース。rustc 1.96.0 同梱・
nightly-2026-07-15 rust-src で同一内容を確認）。

### 8.3 実機検出結果表

`crates/engine/examples/detect_features.rs`（`make detect-features`）の出力を
転記する。観測点は以下の 1 点のみであり、仮想化された Apple Silicon runner・
macOS 26.5.2 に限定される（bare-metal・旧 macOS へは一般化しない）。

**GitHub ホステッド `macos-latest`（Apple Silicon。仮想化。confirmed）**:

- run: [`detect-apple`（run 34038864529・commit `33e05b1f`）](https://github.com/Fandhe-AI/vector-db/actions/runs/34038864529)
- 環境: `machdep.cpu.brand_string` = `Apple M1 (Virtual)`／`uname -m` = `arm64`／
  `sw_vers` `ProductName: macOS`・`ProductVersion: 26.5.2`・`BuildVersion: 25F84`／
  toolchain `stable-aarch64-apple-darwin`（`rustup show`。rustc 版数行は run ログに
  含まれないため run 実行日〔2026-09-06〕時点の stable とのみ記録し版数は推測しない）
- `engine::isa::current()`: `Neon`
- 検出表（`cfg!`／マクロ／`sysctl` の順）:

  | feature | cfg | macro | sysctl |
  | ------- | --- | ----- | ------ |
  | neon | true | true | n/a |
  | fp16 | true | true | true |
  | fhm | true | true | true |
  | dotprod | true | true | true |
  | bf16 | false | false | false |
  | i8mm | false | false | false |
  | sme | false | n/a (unstable macro) | false |
  | sme2 | false | n/a (unstable macro) | false |

- raw sysctl 出力: `sysctl hw.optional.AdvSIMD` → `sysctl: unknown oid
  'hw.optional.AdvSIMD'`（§8.2 参照）。`hw.optional.arm.FEAT_FP16`／
  `FEAT_FHM`／`FEAT_DotProd` = `1`、`FEAT_BF16`／`FEAT_I8MM`／`FEAT_SME`／
  `FEAT_SME2` = `0`
- `cargo test -p engine --test isa`: `test result: ok. 8 passed; 0 failed;
  0 ignored; 0 measured; 0 filtered out`

**オーナー実機（M4 等）**:

- 未実測・申し送り（§8.5 参照）。`make detect-features` の出力を貼り付ける形で
  追記できる

### 8.4 手順

1. `make detect-features`（`cargo run -p engine --release --example
   detect_features`）を実行する
2. macOS では追加で `sysctl -n hw.optional.arm.FEAT_SME` 等（example の
   sysctl 名一覧を参照）で相互検証できる
3. 出力を本節（§8.3）の該当行へ転記する
4. `.github/workflows/detect-features.yml` は `workflow_dispatch` からも
   手動起動できる（GitHub Actions の「Run workflow」）

### 8.5 #508 への申し送り

- Darwin 向けの代替検出機構（環境変数上書き等）は不要と判断する。
  `neon`／`fp16`／`fhm`／`dotprod` はコンパイル時 target_feature 化により
  Apple ターゲットでは実行時 `false` になり得ない（§8.3 の実機観測で確認済み）
- `bf16` は std の `sysctlbyname` 経由の実行時検出（`is_aarch64_feature_detected!`
  マクロ経由）で足りる（M1 runner では世代依存で `false`）。`sme`／`sme2` は
  std_detect 内部に実装があるのみで、マクロ経由の利用は stable でコンパイル
  不可（`stdarch_aarch64_feature_detection` 未安定・§2-D 参照）。isa.rs から
  利用するには機能ゲート安定化を待つか、マクロを介さず `sysctl`（macOS）等を
  直接呼ぶ代替実装が必要——後続作業として申し送る
- `hw.optional.AdvSIMD` が §8.3 の runner で unknown oid だった観測を、実行時
  sysctl 検出に依存する経路が将来生じた場合の注意として記録する。現行の対象
  ターゲット（`aarch64-apple-darwin`）はいずれも `neon` をコンパイル時
  target_feature に含むため、本 Issue の範囲では影響しない
- オーナー所有実機（特に M4 の SME 判定）での `make detect-features` 実行結果を
  §8.3 へ追記することが残作業

## 参照

- spec ポインタ（本文非転記）: CORE-9／CORE-10／CORE-16／TASK-132
  （`docs/spec/04-behavior/core-engine.md`）
- [`docs/design/hotpath-implementation-survey.md`](hotpath-implementation-survey.md)
  （手法×実装×採否候補×ライセンスの表。§9・§10 の既起票／Rejected 対応表）
