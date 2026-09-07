# i8 パック常駐と `dot4I8Packed` シェーダ（Issue #542）

- 親: #541（Phase 5 親 #460・ルート #455）
- 依存: #521（対称 SQ8 量子化。2026-09-07 時点で OPEN・実装未着手）
- 後続: #543（前後比較・Recall 影響の記録）

## 1. 背景・目的

既存の GPU バッチ経路（`gpu_batch.rs::GpuBatchBackend`）は f16 2 要素/u32
パック常駐（`batch_search.rs::ResidentMatrix::packed()`）を
`unpack2x16float` で f32 へ戻して積和する（2 byte/要素）。本 Issue は、
次元別対称 SQ8 量子化で行を i8 へ落とし 4 要素/u32 でパックし、WGSL 組み込み
`dot4I8Packed` で整数内積を計算する経路（1 byte/要素）を追加する。

親 Issue #541 の制約により、量子化・低精度常駐は **opt-in 限定・候補生成の
みで最終スコアは f32 再計算**。既定経路（`FallbackBatchEngine::
build_with_gpu` の primary）へは接続しない。

## 2. #521 の実装状況（着手前の確認）

2026-09-07 時点で #521（対称 SQ8 量子化・次元別 min/max）は OPEN・実装未着手
（`crates/engine/src/` に `Sq8`/`sq8`/量子化のトップレベル共有モジュールが
存在しないことを確認済み）。そのため本 Issue は `gpu_batch/packed_i8.rs`
モジュール内に対称 SQ8 エンコーダを自前で実装した（`encode_rows`・
`Sq8DimParams`）。#521 マージ後は、その型へ統合する（`f16.rs` の前例に倣い、
共有層の所有は #521 側に委ねる）。

## 3. 設計判断

| 論点 | 決定 |
| ---- | ---- |
| 適用境界 | `FallbackBatchEngine::build_with_gpu`（本番 primary）は無変更。i8 経路は `GpuI8BatchBackend::try_new` からのみ構築する opt-in 専用バックエンド（`GpuF32ContrastBackend` の前例と同型） |
| 配置 | 新規子モジュール `crates/engine/src/gpu_batch/packed_i8.rs`。`gpu_batch.rs` への編集は `pub mod packed_i8;` と `GpuContext::backend: wgpu::Backend` フィールド追加の 2 点に限定 |
| 行の量子化 | 次元 `i` ごとに `center_i = (min_i+max_i)/2`・`alpha_i = (max_i-min_i)/254`。`xq_i = round((x_i-center_i)/alpha_i)` を `[-127, 127]` へクランプ（`-128` は使わない）。`alpha_i == 0`（定数次元）は除算せず `xq_i = 0` |
| パック規約 | 4 レーン/u32。レーン `j`（`j = 0..4`）を bit `8j..8j+7` に格納（下位から詰める規約。`pack_f16x2` と同じ方向）。`row_stride = dim.div_ceil(4)`。末尾パディングレーンは 0 |
| クエリ側の量子化 | 次元別スケールを畳み込んでからグローバルスケールで量子化する（詳細は本節末尾の式参照）。行に依存しない定数項は候補順位付けに寄与しないため計算しない |
| オーバーフロー | `MAX_BATCH_DIM(8,192) × 127 × 127 = 132,129,792 < 2^31`。単体テスト `dot_i8_packed_ref_extreme_values_stay_within_i32_bound` で固定 |
| CPU 参照実装 | `packed_i8::dot_i8_packed_ref`（レーン展開＋i32 累積の純関数）。#522（VNNI）はこの参照実装とビット同一であることを契約とする |
| 候補生成＋再スコア | GPU は i32 整数内積で候補を選び、ホストが `k' = min(reachable_rows, k × oversample)` 件を i32 降順（同点は slot 昇順）で選出 → `ResidentMatrix::row_f32_into`（本 Issue で `pub(crate)` 化）で f32 復号し `kernel::dot` で再スコア → 上位 `k` 件へ切り詰め → 既存 `finalize_gpu_hits` で解決 |
| 量子化入力の出所 | `try_new(matrix, options)` は元の f32 ベクトル列を別引数で受け取らず、`matrix` 自身の全行を `row_f32_into` でデコードして量子化入力にする。別引数にすると呼び出し元が渡す行順とスロット順が食い違うリスクがあるため、`matrix` を単一の真値にした（D8 の f32 再スコアが参照する真値とも一致） |
| oversample | `GpuI8Options { oversample: usize }`。既定 `DEFAULT_I8_OVERSAMPLE = 4`・上限 `MAX_I8_OVERSAMPLE = 32`（範囲外は `InitFailed`）。環境変数では読まない（CORE-12） |

クエリ側の量子化の式（表の「クエリ側の量子化」行の詳細）:

1. 次元別スケールを先に畳み込む: 折り畳み値 = クエリ成分 × その次元の alpha
2. グローバルスケール = 折り畳み値の絶対値の最大 ÷ 127
3. 各成分を折り畳み値 ÷ グローバルスケールで丸め、`[-127, 127]` へクランプ
   （グローバルスケールが 0 なら全成分 0）

内積の分解: `dot(q, x)` は「クエリ成分 × その次元の中心値」の総和（行に依存
しない定数項）と、「折り畳み値 × 量子化済み行成分」の総和（整数内積で計算
する項）に分かれる。中心値（`center`）は全行共通の次元別パラメータのため、
定数項は候補順位付けに寄与しない。本実装はこの定数項を計算せず、整数内積
だけで候補を選ぶ。
| メモリ | GPU 側は i8 パック（1 byte/要素）のみ常駐。CPU 側は再スコア・スロット解決のため f16 `ResidentMatrix` を保持する |
| readback 方式 | 全量 readback のみ（i8 用 Top-k シェーダは本 Issue では作らない。#411 系の申し送りと同型） |
| クエリタイル化 | 本実装は 1 dispatch = 1 クエリに単純化した（既存 f16/f32 経路が持つ `GPU_QUERY_TILE_MAX` タイル化は本 Issue の対象外。#543 以降で必要なら追加） |
| meta | `GpuI8Meta { backend: wgpu::Backend, dot4_impl: Dot4I8Impl }`。`Dot4I8Impl` は `Native`／`Polyfill`／`Undetermined` の閉じた列挙で、下記 §4 の理由により本実装は常に `Undetermined` を返す |
| 統計 | `GpuI8BatchStatsSnapshot { dispatches, readback_bytes, rescored_candidates }` |

## 4. `Dot4I8Impl` が常に `Undetermined` である理由

`wgpu-types 30.0.1` の `Features` に `NATIVE_PACKED_INTEGER_DOT_PRODUCT`
相当の定数は存在しない。naga 30.0.1 の SPIR-V backend は
`DotProduct`／`DotProductInput4x8BitPacked` capability の有無で `OpSDot`
と `BitFieldSExtract` polyfill を切り替えるが、その capability は
wgpu-hal Vulkan が private capability（`VK_KHR_shader_integer_dot_product`
／API 1.3 昇格）から設定する。公開経路は `Adapter::as_hal`（`unsafe`）
のみで、本リポは `unsafe` 原則禁止（`tests/isa.rs` が個数・局在を固定）の
ため使えない。専用命令・polyfill のいずれでも整数結果は同一（`dot4I8Packed`
の仕様上の契約）なので、整数一致テストの正しさは判別結果に依存しない。

backend 別の専用命令の条件（naga／wgpu-hal 30.0.1 の実装コードから確認。
本実装では判別せず記録のみ）:

| backend | 専用命令の条件 |
| ---- | ---- |
| Vulkan | `VK_KHR_shader_integer_dot_product`（API 1.3 で昇格）の `shaderIntegerDotProduct` が true のとき `OpSDot`、それ以外は `BitFieldSExtract` polyfill |
| DX12 | Shader Model 6.4 以上で `dot4add_i8packed`、未満は polyfill |
| Metal | MSL 2.1 以上で `packed_char4` の内積、未満は polyfill |

## 5. 実測（本開発環境: RTX 3060・Vulkan backend）

`crates/engine/tests/gpu_batch_i8.rs`（`bench-internals` feature）を実
GPU 上で実行し、以下を確認した:

- 生の i32 スコア（`batch_search_raw_i32_for_tests`）が CPU 参照実装
  （`dot_i8_packed_ref`）と**完全一致**（奇数次元 dim=5 を含むフィクスチャ）
- 混在テナントバッチで他テナントの id が混入しない
- `stats()` が非 vacuous（`dispatches >= 1`・`rescored_candidates >= 1`）
- 同一入力の 2 回実行が id・スコア列とも一致（決定的）
- `meta().dot4_impl == Undetermined`

既存 GPU テスト（`tests/gpu_batch.rs`・`tests/batch_fallback.rs`・
`tests/gpu_scaling_accept.rs`）はいずれも本 Issue の変更後も green
（受け入れ条件「既存 GPU テストが green」を満たす）。

性能の前後比較・Recall 影響の記録は #543 の担当（本 Issue では実施しない）。

## 6. スコープ外・申し送り

- #521 マージ後のエンコーダ統合（本 Issue で置いた `packed_i8.rs` 内エンコー
  ダを #521 の共有層へ寄せる）
- i8 用 workgroup 内部分 Top-k シェーダ（i32 キーの bitonic 網）は未実装
- クエリタイル化（1 dispatch = 複数クエリ。既存 f16/f32 経路の
  `GPU_QUERY_TILE_MAX` 相当）は未実装
- oversample 既定値の調整（#523／#543 の実測後）
- `bench-gpu-scaling` への i8 経路追加と前後比較表（#543）
- wgpu 更新時に判別可能な feature が追加された場合の `Dot4I8Impl` 判定の実装
- `FallbackBatchEngine` への i8 primary 接続は行わない（既定経路不変・#541
  契約）
