# SHADER_F16 対応アダプタでの f16 算術版シェーダ選択（Issue #539）

親 Issue #538・対象ビヘイビア: CORE-6, 8, 16（ポインタ）。`gpu_batch.rs` の
既定 S0（内積）シェーダは `unpack2x16float`（WGSL コア機能）で f16 パック
常駐行列を f32 へ復元してから f32 で積和する（`docs/design/gpu-batch-wgpu-enablement.md`
§2.2）。本 Issue は `wgpu::Features::SHADER_F16` に対応するアダプタでのみ、
`enable f16;` によるネイティブ半精度演算の S0 シェーダを選択できる経路を
追加した。**前後比較の実測（性能への効果）は #540 の担当であり、本 doc・
本 Issue の変更には数値・計測表を含まない。**

## 1. feature 交渉（`init_gpu_context`）

- `adapter.features().contains(Features::SHADER_F16)` を満たす場合のみ
  `request_device` に `Features::SHADER_F16` を要求する
- プロセス共有 `wgpu::Device` は `OnceLock` で 1 回しか作られないため、
  feature 付き要求が失敗した場合は `Features::empty()` で **1 回だけ
  再試行**し、GPU 経路全体（既存の unpack 版）を道連れにしない
- f16 算術版のパイプライン（内積本体・workgroup 内部分 Top-k の 2 本）は
  `create_topk_pipeline`（既存の汎用パイプライン生成ヘルパ。名称は
  Top-k 由来だが shader_src/label を引数化した汎用実装）を使い、独立
  error scope で生成する。生成失敗は `init_gpu_context` 自体を失敗させず
  `GpuContext::f16_arith_pipelines = None` へ吸収する（`topk_pipeline` と
  同じ段階的 fail-closed 縮退。ADR 決定 2 と同型）
- feature が有効化されなかった場合（未対応アダプタ・feature 要求失敗後の
  再試行）は f16 算術版シェーダのコンパイル自体を試みない（naga 30.0.1 は
  `enable f16;` を capability 不足として validation エラーにするため。
  `enable subgroups;` と同じ扱い）

## 2. シェーダ

2 本を新設する。行データ（f16 2 要素/u32 パック常駐）は既存 unpack 版と
完全に同一のバイト列で、`vec2<f16>` として再解釈するだけ。

- `DOT_SHADER_F16_ARITH_WGSL`（全量 readback 用）
- `DOT_SHADER_TOPK_F16_ARITH_WGSL`（`topk_dot_shader!` マクロ経由。既存の
  unpack 版・f32 対照版と S1〔Top-k 選出〕を共有し、S0〔内積〕だけが異なる）

`topk_dot_shader!` は本 Issue で `$prelude`（`enable f16;` の要否）・
`$query_binding`（クエリバッファの要素型）の 2 引数を追加した。既存 2 呼び
出し（unpack・f32 対照）は `$prelude=""`・`$query_binding="array<f32>"` を
渡すだけで挙動不変。

f16 レジスタでの積算は `GPU_F16_ACC_BLOCK`（実装既定値 8）回ごとに
`f32(acc2.x) + f32(acc2.y)` で f32 アキュムレータへフラッシュし、f16
レジスタを 0 に戻す。QUERY_TILE_MAX 分のクエリすべてに渡って積算し続けると
容易に f16 の有限最大値（65504）を超えるため、ブロック単位でのフラッシュに
より「ブロック内部分和のみ f16 の値域に収まればよい」設計にしている。

## 3. オーバーフローガードと選択規則

f16 の有限最大値（65504）を超える中間値が生じると、f16 算術版だけが該当
行を非有限スコアとして除外し、unpack 版との等価性（受け入れ条件の核心）が
壊れる。これを防ぐため、ホスト側の純関数 `select_dot_shader` で dispatch
前に判定する:

```text
select_dot_shader(f16_available, row_max_abs, query_max_abs):
  1. f16_available が false なら Unpack
  2. row_max_abs／query_max_abs のいずれかが非有限なら Unpack
  3. query_max_abs > F16_MAX_FINITE(65504) なら Unpack
     （row_max_abs の値に関わらず崩れる独立した条件。クエリ成分が
     f16 パック時点で ±Inf へ飽和すると、unpack 版では有限のスコアに
     なる行が f16 算術版だけ除外されてしまうケースを閉じる）
  4. row_max_abs * query_max_abs * GPU_F16_ACC_BLOCK が
     F16_ARITH_PARTIAL_SUM_LIMIT(32768) を超えるなら Unpack
     （ブロック内部分和が f16 の有限最大値へ達しうる保守的な上界判定。
     65504 の約半分を選び、丸め・fma 誤差蓄積の余裕を持たせる）
  5. それ以外は F16Arith
```

`row_max_abs`（常駐行列の有限成分のみの絶対値最大）は `GpuBatchBackend::
try_new` で 1 回だけ走査して確定させ、`query_max_abs`（クエリバッチの有限
成分のみの絶対値最大。f16 丸め前の f32 値）は `batch_search` 呼び出しごとに
算出する。非有限成分（f16 パック時の飽和で ±Inf 化した値）は unpack 版でも
必ず非有限スコアとして除外される値のため、最大値計算から除外してよい
（`select_dot_shader` doc コメント参照）。

選択は **`batch_search` 呼び出し単位**で 1 回だけ行う（タイル単位にしない。
コード量と検証の単純さを優先した実装判断）。

## 4. ホスト側の dispatch

- `DotDispatchTarget` に `query_encoding: QueryEncoding {F32, F16Packed}` を
  追加し、`dispatch_dot_products`/`dispatch_partial_topk` は
  `encode_query_bytes` で `queries_concat`（常に f32 論理値・既存の
  `query_stride` 契約は不変）をエンコードしてからアップロードする。
  `F16Packed` は `batch_search::pack_f16x2` と同一表現で 2 要素ずつ u32
  パックし、バイト数は f32 表現の半分になる
- `GpuF32ContrastBackend`（CORE-16 対照経路）は Issue #539 の対象外。常に
  `QueryEncoding::F32` 固定で無変更

## 5. 統計・テスト用オーバーライド

- `GpuBatchStatsSnapshot` へ `f16_arith_dispatches`（`batch_search` 呼び出しが f16 算術版へ
  dispatch した回数）・`f16_arith_guard_fallbacks`（f16 算術版パイプライン
  は利用可能だが自動選択のオーバーフローガードにより unpack 版へ縮退した
  回数）を追加。`gpu_scaling_bench.rs` の `gpu_scaling_stats:` 出力行へ
  両カウンタを追記し、#540 の非 vacuous 判定材料にする
- `GpuBatchBackend::f16_arith_available() -> bool`（テナント情報を含まない
  情報提供専用の問い合わせ）を追加
- `GpuSearchTestOptions`（`bench-internals` feature 限定）に
  `dot_shader: Option<GpuDotShaderKind>` を追加。`Some(Unpack)` は常に
  unpack 版を強制し、`Some(F16Arith)` は f16 版が利用不能またはガード不成立
  なら黙って縮退せず `Err(KernelLaunchFailed)` を返す（fail-closed 分岐の
  検証用。CORE-12「外部からの経路上書き機構を設けない」と整合し、既定
  ビルド・`wire-server` からは到達不能）
- 環境変数・`from_env` による切替は追加しない

## 6. 実機検証（RTX 3060・Vulkan・`SHADER_F16 = true`）

`crates/engine/tests/gpu_batch.rs::f16_arith_dot_shader` モジュールで:

- 既定経路（対応アダプタでは自動的に f16 算術版）と強制 unpack 版が
  境界同点許容つき Recall（`benches/harness/gpu_scaling.rs::
  count_boundary_tolerant_mismatches`）で一致し、`f16_arith_dispatches > 0`
  （非 vacuous）であることを確認
- f16 算術版の部分 Top-k 経路と強制全量 readback 経路が実 GPU dispatch
  経由でビット同一であることを確認（S0 演算順一致契約）
- 大振幅フィクスチャ（ブロック内部分和が上限を超える）では自動選択が
  unpack 版へ縮退し `f16_arith_guard_fallbacks` が増加すること、かつ
  強制 `F16Arith` はガード不成立のため `Err` を返すこと（黙示縮退しない）
  を確認

## 7. スコープ外・申し送り

- 前後比較の実測・CORE-16 ゲートへの影響記録は #540 の担当
- `GpuF32ContrastBackend` 側の f16 算術化は対象外（CORE-16 の公平性は
  f32 常駐のまま維持する契約）
- `unsafe`・依存追加なし（wgpu `=30.0.1` 既存依存の範囲内）
