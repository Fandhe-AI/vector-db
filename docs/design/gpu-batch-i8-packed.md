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
`Sq8RowScales`。当初は次元別 `min`/`max` パラメータ `Sq8DimParams` だったが
§7 の PR レビュー対応で行単位スケールへ改めた）。#521 マージ後は、その型へ
統合する（`f16.rs` の前例に倣い、共有層の所有は #521 側に委ねる。**#521 が
次元別 `min`/`max` 方式を踏襲する場合、§7-1 と同種のテナント境界問題を
引き継ぐ点はオーナーへの申し送り事項**）。

## 3. 設計判断

| 論点 | 決定 |
| ---- | ---- |
| 適用境界 | `FallbackBatchEngine::build_with_gpu`（本番 primary）は無変更。i8 経路は `GpuI8BatchBackend::try_new` からのみ構築する opt-in 専用バックエンド（`GpuF32ContrastBackend` の前例と同型） |
| 配置 | 新規子モジュール `crates/engine/src/gpu_batch/packed_i8.rs`。`gpu_batch.rs` への編集は `pub mod packed_i8;` と `GpuContext::backend: wgpu::Backend` フィールド追加の 2 点に限定 |
| 行の量子化 | **（D3 改訂・§7-1 参照）** 行 `i` ごとに独立なスケール `s_i = max_j(|x_{i,j}|) / 127`。`xq_{i,j} = round(x_{i,j}/s_i)` を `[-127, 127]` へクランプ（`-128` は使わない）。`s_i == 0`（零行）は除算せず `xq_{i,j} = 0`。他行（他テナントの不可視行を含む）の値には一切依存しない |
| パック規約 | 4 レーン/u32。レーン `j`（`j = 0..4`）を bit `8j..8j+7` に格納（下位から詰める規約。`pack_f16x2` と同じ方向）。`row_stride = dim.div_ceil(4)`。末尾パディングレーンは 0 |
| クエリ側の量子化 | **（D5 改訂・§7-1 参照）** クエリ自身の成分だけから対称スケール `s_q = max_i(|q_i|)/127` を求めて量子化する（詳細は本節末尾の式参照）。行パラメータには一切依存しない |
| オーバーフロー | `MAX_BATCH_DIM(8,192) × 127 × 127 = 132,129,792 < 2^31`。単体テスト `dot_i8_packed_ref_extreme_values_stay_within_i32_bound` で固定 |
| CPU 参照実装 | `packed_i8::dot_i8_packed_ref`（レーン展開＋i32 累積の純関数）。#522（VNNI）はこの参照実装とビット同一であることを契約とする |
| 候補生成＋再スコア | **（§7-2 参照）** GPU は i32 整数内積を返す。ホストは行スケール `s_i` を掛けた近似内積（`i32_score × s_i`）降順（同点は slot 昇順）でチャンクごとに逐次縮約し、`k' = min(reachable_rows, k × oversample)` 件だけを保持 → `ResidentMatrix::row_f32_into`（本 Issue で `pub(crate)` 化）で f32 復号し `kernel::dot` で再スコア → 上位 `k` 件へ切り詰め → 既存 `finalize_gpu_hits` で解決 |
| 量子化入力の出所 | `try_new(matrix, options)` は元の f32 ベクトル列を別引数で受け取らず、`matrix` 自身の全行を `row_f32_into` でデコードして量子化入力にする。別引数にすると呼び出し元が渡す行順とスロット順が食い違うリスクがあるため、`matrix` を単一の真値にした（D8 の f32 再スコアが参照する真値とも一致） |
| oversample | `GpuI8Options { oversample: usize }`。既定 `DEFAULT_I8_OVERSAMPLE = 4`・上限 `MAX_I8_OVERSAMPLE = 32`（範囲外は `InitFailed`）。環境変数では読まない（CORE-12） |

クエリ側の量子化の式（表の「クエリ側の量子化」行の詳細。D5 改訂後）:

1. スケール = クエリ成分の絶対値の最大 ÷ 127
2. 各成分をスケールで割って丸め、`[-127, 127]` へクランプ
   （スケールが 0 なら全成分 0）

近似内積: GPU が返す整数内積 `Σ qq_i·xq_i` に行スケール `s_i` とクエリ
スケール `s_q` を掛けると `s_i · s_q · Σ qq_i·xq_i` が近似内積になるが、
同一クエリ内の候補順位付けでは `s_q` は全候補に共通の正の定数なので、
順位付けには `s_i` だけを掛ければ足りる（`packed_i8.rs::ranking_key`）。
D3 改訂前は次元別 `center`（行に依存しない次元別パラメータ）との内積を
定数項として省略する設計だったが、行単位スケール方式にはこの「次元別
center」という概念自体が存在しないため、この定数項の議論は D3 改訂で
消滅した。
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

## 7. PR #598 レビュー対応

codex-review の指摘 3 件（P0 1 件・P1 2 件）はいずれも修正済み
（`crates/engine/src/gpu_batch/packed_i8.rs`）。

### 7-1. P0: 量子化パラメータのテナント境界侵害

当初実装は次元別 `min`/`max`（`ResidentMatrix` の全行・全テナント横断）
から `center`/`alpha` を導出していたため、他テナントの不可視行の値・存在
が量子化スケール経由で候補選出（ひいては検索結果）に影響しうるテナント
境界侵害だった。行 `i` 単独から決まる対称スケール
`s_i = max_j(|x_{i,j}|)/127` へ変更し（`Sq8DimParams` → `Sq8RowScales`）、
他行（他テナントの不可視行を含む）の値に一切依存しない構成にした
（§3 の「行の量子化」「クエリ側の量子化」行を参照）。

検証は CPU オンリーの回帰テスト
`packed_i8::tests::encode_rows_row_scale_is_independent_of_other_rows_cross_tenant_leak_regression`
で固定した: 同じ行列に同居する他テナントの行を極端な外れ値へ差し替えても、
対象行のスケール・パック済みバイト列がビット同一であることを確認する
（GPU 不要）。

**#521（対称 SQ8 量子化）が次元別 `min`/`max` 方式を踏襲する場合は同種の
境界問題を引き継ぐため、#521 実装時にオーナーへ申し送りが必要**
（§2 にも記載）。

### 7-2. P1: `raw_i32_scores` の無制限メモリ保持

当初実装は `raw_i32_scores` がクエリごとに到達行**全件**の `(slot, score)`
を一括保持していた。`dim` が小さい入力（例: dim=1・到達行 100 万・4,096
クエリ・各 k=1）では既存の件数・`sum(k)`・`MAX_BATCH_WORK` 検証を通過し
つつ約 32.768GB のメモリを要求しうる構造で、`Vec::push`/`clone`（未予約）
も確保失敗時に abort するため fail-closed 契約に反していた。

チャンク受信のたびに [`reduce_top_k_prime`] で逐次縮約し、保持量を
`k' = min(reachable_rows, k × oversample)` 件（＋直近チャンクの行数）以内
に抑える方式へ変更した。ストリーミング top-k の標準的な性質
`TopK(A∪B, k) == TopK(TopK(A,k)∪B, k)` により、逐次縮約後の最終集合は
「全件を一括保持してから 1 回だけ選出した場合」と一致する（正当性の証明は
コード内コメント参照）。バッチ全体での保持量の上限は
`Σk' <= MAX_BATCH_TOTAL_K(1,000,000) × MAX_I8_OVERSAMPLE(32) = 32,000,000`
要素（既存の `sum(k)` 上限・oversample 範囲検証から導かれる）。すべての
新規確保は `try_reserve_exact` によるフォールブル確保とし、`batch_search`
側の未予約 `clone`（旧・791 行相当）は撤去した（縮約済みの結果をそのまま
再利用する）。

行単位スケール化（§7-1）に伴い、チャンク内の縮約・最終選出のキーも
「生の i32 スコア降順」から「行スケールを掛けた近似内積降順」
（`ranking_key`／`sort_by_ranking_key`）へ変更した（行をまたぐ生の i32
スコアは、行ごとにスケールが異なるためそのままでは比較できない）。

### 7-3. P1: 量子化の中間演算オーバーフロー

`encode_rows`/`quantize_query` の量子化（除算・丸め）を f32 のみで行うと、
極端な入力（非常に大きい／小さい有限値）で中間演算が非有限値
（NaN/Inf）になり、`NaN as i8 == 0` へ暗黙に丸まって本来有効なスコアが
無言で無効化されうる欠陥があった。除算・丸めを f64（`row_scale`・
`quantize_scalar`）で行い、桁数に余裕を持たせたうえで、それでも中間結果が
非有限になった場合は `I8EncodeError::NonFinite` を明示的に返すフェイル
クローズへ変更した。

### 検証

- `cargo test -p engine --features bench-internals,contrast-bench --lib gpu_batch::packed_i8`
  （CPU オンリーの単体テスト。§7-1 の回帰テストを含む）
- `cargo test -p engine --features bench-internals,contrast-bench --test gpu_batch_i8`
  （実 GPU がある本開発環境〔RTX 3060・Vulkan backend〕で実走。既存の
  CPU 参照実装一致・混在テナント非漏えいテストは無変更のまま green）
- `cargo clippy -p engine --all-targets --features bench-internals,contrast-bench -- -D warnings`
