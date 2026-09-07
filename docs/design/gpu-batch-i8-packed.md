# i8 パック常駐と `dot4I8Packed` シェーダ（Issue #542）

- 親: #541（Phase 5 親 #460・ルート #455）
- 依存: #521（対称 SQ8 量子化。2026-09-07 時点で OPEN・実装未着手）
- 後続: #543（前後比較・Recall 影響の記録）

## 1. 背景・目的

既存の GPU バッチ経路（`gpu_batch.rs::GpuBatchBackend`）は f16 2 要素/u32
パック常駐（`batch_search.rs::ResidentMatrix::packed()`）を
`unpack2x16float` で f32 へ戻して積和する（2 byte/要素）。本 Issue は、
行単位対称 SQ8 量子化（各行の絶対値最大からスケールを独立に導出する。D3
改訂）で行を i8 へ落とし 4 要素/u32 でパックし、WGSL 組み込み
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
| 行の量子化 | **（D3 改訂・§7-1 参照）** 行 `i` ごとに独立なスケール `s_i = max_j(abs(x_{i,j})) / 127`。`xq_{i,j} = round(x_{i,j}/s_i)` を `[-127, 127]` へクランプ（`-128` は使わない）。`s_i == 0`（零行）は除算せず `xq_{i,j} = 0`。他行（他テナントの不可視行を含む）の値には一切依存しない |
| パック規約 | 4 レーン/u32。レーン `j`（`j = 0..4`）を bit `8j..8j+7` に格納（下位から詰める規約。`pack_f16x2` と同じ方向）。`row_stride = dim.div_ceil(4)`。末尾パディングレーンは 0 |
| クエリ側の量子化 | **（D5 改訂・§7-1 参照）** クエリ自身の成分だけから対称スケール `s_q = max_i(abs(q_i))/127` を求めて量子化する（詳細は本節末尾の式参照）。行パラメータには一切依存しない |
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
実施結果は「前後比較実測（Issue #543）」節参照。

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

## 8. 前後比較実測（Issue #543）

### 前提

- before: `2d2c74e`（#542 適用直前）／after: 本 PR の作業ブランチ
  （`e14d53f`〔#542 マージコミット〕以降・`git diff e14d53f <作業ブランチ>
  -- crates/engine/src/` が空であることを確認済み）。`git diff 2d2c74e
  e14d53f -- crates/engine/src/gpu_batch.rs crates/engine/src/lib.rs` の
  差分は `pub mod packed_i8;`・`GpuContext::backend` フィールド追加のみで、
  既存 f16/f32 経路の dispatch・シェーダは不変。`Cargo.lock` は両コミット間で
  無変更（`git diff 2d2c74e e14d53f -- Cargo.lock` が空）。
- i8 経路（`packed_i8.rs`）は `e14d53f` より前には存在しないため、i8 の
  「before」は作れない（実測できない）。before バイナリでの `gpu_scaling_i8:`
  行は全 run で不在であることを確認済み（既存 `gpu_scaling:`／
  `gpu_scaling_stats:` の非退行〔層 1〕は本 Issue の主題ではないため個別の
  前後比較表は作らず、以下の層 2（同一 after バイナリ内の A/B/D 比較）のみ
  記録する）。
- 環境: 本開発環境（共有 QEMU VM・NVIDIA GeForce RTX 3060・Vulkan backend・
  12 vCPU・計測中の loadavg 約 3.6〜8.5）。`BENCH_DEDICATED_ENV` 未設定の
  共有環境であり、`benchmark-judgement-policy.md` §5 により**レイテンシ
  数値は参考値・採否根拠にしない**。Recall・mismatch・readback バイト数・
  再スコア候補数は実行のたびに一意に定まる確定的指標のため、本環境でも
  確定的に判定できる。
- 規模点: `20000:128:8`（GPU 転送・dispatch の固定コストが支配的な小規模
  点）・`100000:128:64`（`crossdb-bench.md` GPU 節と同一の中規模点）・
  `100000:256:64`（i8 は f16 比でバイト/要素が半減するため dim 拡大点を
  1 点含める）の 3 点。ペア数 N=5（交互 before→after。既定 oversample=4。
  `scripts/bench_gpu_scaling_ab.sh`）。`500000:128:64` 等の追加規模点・
  oversample のフル N=5 スイープ（{1,2,8}）は計測時間の都合で本実測の
  スコープ外とした（下記「oversample の推奨値」節参照。スコープ外事項として
  記録）。生データは
  `docs/design/bench-data/gpu-scaling-ab/20260907T084600Z-summary.tsv`
  （31 行＝3 点 × 5 ペア × 2 側 + ヘッダ）・
  `docs/design/bench-data/gpu-scaling-ab/20260907T084600Z-i8-stats.txt`
  （規模点ごとの `gpu_scaling_i8:`／`gpu_scaling_i8_stats:` 行の抜粋）に
  保持。

### 確定的指標: Recall・不一致件数・読み戻し量（環境ノイズの影響を受けない）

CPU-SIMD（A）厳密対照に対する i8 経路（D。既定 oversample=4）の平均
Recall@10・同点許容つき不一致件数は、3 規模点 × 5 ペア（after 側 15 run）
の**全 run で完全に同一の値**だった:

| rows | dim | batch | oversample | i8_recall_at_k（全 5 run） | i8_mismatch（全 5 run） |
| --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 8 | 4 | 1.0000 | 0 |
| 100,000 | 128 | 64 | 4 | 1.0000 | 0 |
| 100,000 | 256 | 64 | 4 | 1.0000 | 0 |

**Recall@10 の低下は 1 件も観測されなかった**（3 規模点 × 5 ペアの全 15
run で `i8_recall_at_k = 1.0000`・`i8_mismatch = 0`）。`crates/engine/
tests/gpu_batch_i8.rs` の受け入れ基準（brute-force 対照 Recall@10 ≥ 0.9・
oversample 増加で非減少）とも整合する。

読み戻し量・再スコア候補数（規模点ごと 1 run の代表値。同一規模点内で
`calls`・`readback_bytes_total`・`rescored_candidates_total` は決定的に
一致する——`oversample` が構築時固定でクエリ内容に依存しないため）:

| rows | dim | batch | readback_bytes/call | rescored_candidates/call | backend | dot4_impl | build_ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 8 | 640,000 | 320 | Vulkan | Undetermined | 20 |
| 100,000 | 128 | 64 | 25,600,000 | 2,560 | Vulkan | Undetermined | 91 |
| 100,000 | 256 | 64 | 25,600,000 | 2,560 | Vulkan | Undetermined | 175 |

readback バイト数は `dim` に依存せず（i8 dispatch が読み戻すのは i32
スコア配列のみでベクトル次元数を含まない）、`rows × batch × 4 × 10`
（候補生成専用のため既存 f16/f32 の部分 Top-k readback とは異なり
`k' = k × oversample` 件の生スコアをそのまま読み戻す構造）で決まる。
`dot4_impl` は wgpu 30.0.1 の制約により全 run で `Undetermined`
（§4「`Dot4I8Impl` が常に `Undetermined` である理由」節参照。native/
polyfill の A/B は本実測のスコープ外）。

### レイテンシ（参考値・共有 QEMU 環境のため採否根拠にしない）

`gpu_i8_p95`（after 側。min-of-5・median）と、同一 after バイナリ内の A
（CPU-SIMD）・B（GPU f16 常駐）の p95 を突き合わせた速度比
（`speedup_ratio` = 分子 / i8_p95。1.0 未満は i8 の方が遅いことを表す）。
参照区間帯は pooled `cpu_p50`（before + after 両側。`isa.rs`／
`batch_search.rs` は本 Issue で無変更のため純粋な run-to-run ノイズの目安）。

| rows:dim:batch | i8_p95（min/median） | A（cpu）p95（min） | B（f16）p95（min） | ratio i8/A（min-of-N） | ratio i8/B（min-of-N） | 参照区間帯 |
| --- | --- | --- | --- | --- | --- | --- |
| 20000:128:8 | 5,486 / 6,196 µs | 5,047 µs | 535 µs | 1.087x（**遅い**） | 10.254x（**遅い**） | 11.32% |
| 100000:128:64 | 306,634 / 310,024 µs | 82,869 µs | 15,810 µs | 3.700x（**遅い**） | 19.395x（**遅い**） | 11.62% |
| 100000:256:64 | 336,079 / 361,815 µs | 137,090 µs | 21,899 µs | 2.452x（**遅い**） | 15.347x（**遅い**） | 121.0%（loadavg 変動による外れ値混入。実測帯内・判定不能） |

3 規模点いずれも i8 経路は A（CPU-SIMD）・B（GPU f16 常駐）の**両方より
明確に遅い**（比 1.09〜3.70x 対 A、10.3〜19.4x 対 B）。参照区間帯を大きく
超える一貫した悪化方向であり（`100000:256:64` の参照区間帯 121% は 1 run
の外れ値〔loadavg スパイク〕によるもので、他の 2 点・生ログの他 4 ペアは
安定した傾向を示す）、共有環境のノイズでは説明できない一貫した傾向と判断
する。

### 原因分析

i8 経路（`packed_i8.rs::DOT_SHADER_I8_WGSL`）は**「1 dispatch = 1
クエリ」**の単純な構造のまま実装されている（同シェーダのドキュメンテー
ションコメント「D11 の申し送り」参照）。一方で既存 f16/f32 経路は
Issue #532（クエリタイル化。最大 `GPU_QUERY_TILE_MAX` 本を 1 dispatch へ
束ねる）・Issue #536（workgroup 内部分 Top-k による readback 削減）を
経て最適化済みであり、GPU dispatch・常駐行列読み込みの固定コストを複数
クエリで償却できる。i8 経路にはこの償却機構がなく、`batch >= 8` の全規模
点で `1 dispatch/query` の固定オーバーヘッドがバッチサイズに比例して
積み上がることが、上記の速度比が readback バイト数の削減比（f16 比
1/8〜1/4 相当のバイト数）から期待される改善とは逆方向に大きく外れている
主因と考えられる（クエリタイル化・部分 Top-k は #542 doc §6 で明示的に
「未実装」と申し送られている既知のスコープ外事項であり、本実測で初めて
定量的な裏付けが得られた）。

### oversample の推奨値

構築時固定オプションのため、本実測（既定 oversample=4）に加え、開発中の
スモークテストとして `20000:128:8`・`crates/engine/tests/
gpu_batch_i8.rs::i8_backend_recall_is_monotone_non_decreasing_in_
oversample_when_gpu_available`（2,000 行・dim 128 のクラスタ構造ありコー
パス）で oversample を 1・4・8 と振った際の Recall@10 が非減少である
ことを確認済み（同テストは `make ci` 対象として本 PR に含まれる）。加えて
2,000 行・dim 128・batch 8 の単発スモーク実測で oversample 1 → 2 → 4 → 8
の平均 Recall@10 が 0.9875 → 1.0000 → 1.0000 → 1.0000 と単調に改善する
ことを確認した（N=5 の正式なペア実測はこの oversample スイープでは実施
していない——時間予算の制約によりスコープ外。下記「スコープ外・申し送り」
参照）。

以上より、**既定値 4 は Recall の観点からは十分に安全**（本実測 3 規模点
すべてで Recall@10=1.0000）である一方、**レイテンシの観点では oversample
を下げても i8 経路自体が既存 2 経路（A・B）より大幅に遅いという結論は
変わらない**と見込まれる（原因はクエリタイル化の欠如であり oversample
とは独立の要因のため）。`DEFAULT_I8_OVERSAMPLE` の変更は本 Issue（テスト・
ベンチ・docs 専任）のスコープ外とし、production コード（`packed_i8.rs`）
は無変更のまま維持する。

### 判定

- **Recall 影響**: 3 規模点 × 5 ペアの全 run で `i8_recall_at_k=1.0000`・
  `i8_mismatch=0`。既定 oversample=4 で Recall 劣化は一切観測されなかった
  （確定的指標）。
- **速度**: 共有環境の参考値としては 3 規模点すべてで i8 経路が A（CPU-
  SIMD）・B（GPU f16 常駐）の両方より一貫して遅い（1.09〜19.4x）。原因は
  クエリタイル化未実装という既知のスコープ外事項（§6「申し送り」）に
  帰着すると分析した。
- 本実測は #542 の設計判断（候補生成専用・opt-in・primary 未接続）を
  変更する根拠にはならない——i8 経路は既定経路に一切接続されておらず、
  本実測結果は速度改善が必要になった場合の後続実装（クエリタイル化）の
  優先度判断材料として記録する。

### 再現手順（前後比較）

1. before バイナリを別 worktree・別 `CARGO_TARGET_DIR` で退避:
   `git worktree add <dir> 2d2c74e && cd <dir> && CARGO_TARGET_DIR=<target>
   cargo bench --bench gpu_scaling_bench -p engine --no-run
   --message-format=json` を実行し `executable` を抽出する。
2. after バイナリは作業ブランチで同様にビルドする。
3. 交互実行: `BEFORE_BIN=<path> AFTER_BIN=<path> OUT_DIR=<dir>
   I8_OVERSAMPLE=4 scripts/bench_gpu_scaling_ab.sh 5 20000:128:8
   100000:128:64 100000:256:64`
4. 集計: `summary.tsv` の `i8_*` 列を `rows:dim:batch` × `side` でグルー
   ピングし、min-of-N・median・`ratio = i8_p95_min / {cpu,f16}_p95_min`・
   pooled `cpu_p50` の `reference_band` を算出する。

## スコープ外・申し送り（Issue #543）

- `500000:128:64` 等の追加規模点（i8 は 1 run あたり最大約 2.2 秒〔p50〕・
  N=5 ペアで数分規模になり、本実測の時間予算では計測時間の都合で見送った）
- oversample {1, 2, 8} の正式な N=5 ペア実測（上記スモーク実測で単調性は
  確認済みだが、参照区間帯を伴う正式な前後比較表は未作成）
- `DEFAULT_I8_OVERSAMPLE` の変更（推奨値の記録のみ。採否は専有環境再実測
  後にオーナー判断）
- 専有環境（`BENCH_DEDICATED_ENV=1`）でのレイテンシ再実測（オーナー作業）
- i8 経路のクエリタイル化・workgroup 内部分 Top-k（#542 doc §6 の既存申し
  送りだが、本実測により定量的な優先度判断材料が追加された）
