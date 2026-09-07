# SHADER_F16 対応アダプタでの f16 算術版シェーダ選択（Issue #539）

親 Issue #538・対象ビヘイビア: CORE-6, 8, 16（ポインタ）。`gpu_batch.rs` の
既定 S0（内積）シェーダは `unpack2x16float`（WGSL コア機能）で f16 パック
常駐行列を f32 へ復元してから f32 で積和する（`docs/design/gpu-batch-wgpu-enablement.md`
§2.2）。本 Issue は `wgpu::Features::SHADER_F16` に対応するアダプタでのみ、
`enable f16;` を使う S0 シェーダを選択できる経路を追加した。最終実装は
`vec2<f16>`（行・クエリとも）を読み出し直後に `vec2<f32>` へ拡張してから
積和する（積和自体は f32・§2 参照）ため、既定版との違いは「常駐・転送
表現が f16 パックか f32 か」のみであり、算術がネイティブ半精度演算に
なるわけではない。クエリが f16 へ厳密往復できない場合（§3 の
`query_has_precision_loss`）は unpack 版（既定・f32 のまま送る）へ
fail-closed に縮退する。**前後比較の実測（性能への効果）は #540 の担当
であり、本 doc・本 Issue の変更には数値・計測表を含まない。**

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

**最終実装**（2 巡のレビュー指摘対応を経た現在の構成。`crates/engine/src/
gpu_batch.rs:782` 付近の `GpuDotShaderKind::F16Arith` doc も同じ内容を
指す）は、読み出した `vec2<f16>`（行・クエリとも）を読み出し直後に
`vec2<f32>(...)` へ拡張し、積和を最初から最後まで f32 で行う
（`unpack2x16float` 版と同じ演算順。詳細は本節末尾の「対処は 2 つを
組み合わせる」参照）。f16 レジスタで部分和を保持する `acc2`・
`GPU_F16_ACC_BLOCK` 回ごとのフラッシュという構成は、以下の 2 つの小節が
記録する開発過程のバグ（中間実装で発見・修正した経緯）であり、**この
最終実装の WGSL シェーダ本体には存在しない**（Rust 側の
`GPU_F16_ACC_BLOCK` 定数は撤去せず `1` のまま残るが、`select_dot_shader`
のオーバーフロー margin 計算式の乗数としてのみ使う後方互換の名残であり、
シェーダの実装とは無関係）。以下 2 小節はその経緯記録として残す。

### f16 積算の桁落ち（PR #591 レビュー P1 指摘対応。中間実装の記録）

初版実装は `GPU_F16_ACC_BLOCK = 8` を採用し、f16 レジスタでの積算を
`GPU_F16_ACC_BLOCK` 回ごとに `f32(acc2.x) + f32(acc2.y)` で f32
アキュムレータへフラッシュする構成だった。「ブロック内部分和が f16 の
有限最大値（65504）へ達しないこと」だけをオーバーフローガードで判定して
いたが、オーバーフローしない範囲でも複数項を f16 のまま加算すると桁落ちで
正解行が入れ替わりうることが判明した。反例:
`query=[2048,0,1,0,-2048]`・`row=[1,0,1,0,1]`（真の内積は `1`）は
`row_max_abs=1`・`query_max_abs=2048` で `select_dot_shader` のオーバー
フローガード（4 番目の条件）を通過するが、f16 レジスタ内で `2048+1` を
計算した時点で最近接偶数丸めにより `2048` へ丸められ、続く `-2048` の
加算で最終スコアが `0` になる（真値 `1` と異なり、k=1 の正解が別行と
入れ替わりうる）。

対処として `GPU_F16_ACC_BLOCK` を `1` 固定にし、`acc2` を毎回
「1 組の f16 積を計算した直後」にフラッシュする構成へ変更した。これにより
複数項の f16 加算が構造的に発生しなくなり（残るのは行データ自体が既に
f16 常駐である既存の量子化誤差のみ）、上記反例は真値 `1` と一致する
（`crates/engine/src/gpu_batch.rs`
`tests::f16_arith_precision_bug_pr591_p1_is_fixed_by_acc_block_1` で
CPU 上のシミュレーションとして固定）。性能への影響（フラッシュ頻度の
増加）は前後比較実測の別 Issue（#540）へ申し送り、本変更は正しさ優先の
修正であり実測を伴わない。

### 単一積の丸め（PR #591 レビュー P1 指摘対応・2 巡目。中間実装の記録）

上記「ブロック幅 1」修正は複数項の f16 加算を無くしたが、**単一の積その
ものを `vec2<f16>` として保持する丸め**は解消していなかった。反例:
`query=[128.125,128]`・`row_a=[128.125,-128.25]`・`row_b=[0,2^-14]`
（真の内積は `row_a` が `0.015625`・`row_b` が `0.0078125` で `row_a` が
k=1 の正解）はいずれの成分も f16 で厳密表現でき、既存のオーバーフロー・
アンダーフローガードもすべて通過するが、`128.125 * 128.125 = 16416.015625`
がその区間の f16 分解能（`16`）で `16416` へ丸められ、`row_a` の
`fma(row_pair, qv, 0)`（レーンごと独立）の x レーンが `16416`・y レーンが
`128 * -128.25 = -16416`（厳密表現可能）となって合算が `0` になり、
`row_b`（`0.0078125`）に順位が入れ替わる
（`crates/engine/src/gpu_batch.rs`
`tests::f16_arith_single_product_rounding_bug_pr591_p1_round2_reproduction_and_fix`
で CPU 上のシミュレーションとして固定。この丸めはブロック幅を `1` にした
効果とは独立で、`f16` 積を計算した直後にフラッシュしても積そのものの
丸め誤差は残る）。

さらに独立した問題として、クエリ成分自体の f16 パック時の丸めも振幅・
アンダーフローの既存ガードでは検知できない。反例:
`query=[2048.5,2048]`・`row_a=[1,-1]`・`row_b=[0,2^-13]`（真の内積は
`row_a` が `0.5`・`row_b` が `0.25` で `row_a` が正解）はいずれのガードも
通過するが、`2048.5` は f16 の分解能（この振幅で `2`）で `2048` へ丸め
られ、`row_a` のスコアが `0` になり `row_b` に順位が入れ替わる。この
丸めはクエリを f16 へエンコードする時点で発生するため、シェーダ側の
算術精度をどう変えても救えない。

**最終実装（現在の構成）** は次の 2 つを組み合わせる:

1. **積和を f32 で行う**（`DOT_SHADER_F16_ARITH_WGSL`/
   `DOT_SHADER_TOPK_F16_ARITH_WGSL`）。読み出した `vec2<f16>`（行・クエリ
   とも）を `vec2<f32>(...)` で読み出し直後に拡張してから乗算・加算する
   （[`DOT_SHADER_WGSL`] の `unpack2x16float` 版と同じ演算順）。これにより
   `GPU_F16_ACC_BLOCK`・`acc2`・フラッシュ機構は不要になり撤去した
   （WGSL 側の `F16_ACC_BLOCK` 定数宣言も撤去。Rust 側 `GPU_F16_ACC_BLOCK`
   は既存のオーバーフロー margin 計算式〔`select_dot_shader` の
   `F16_ARITH_PARTIAL_SUM_LIMIT` 判定〕を変えないための乗数 `1` としてのみ
   残る）。行・クエリを `vec2<f16>` のまま常駐・転送する設計自体（帯域幅
   削減）は変更しない
2. **クエリ成分の f16 往復可能性を検知する独立ガード** `f16_round_trip_exact`
   （`crate::batch_search::pack_f16x2`/`unpack_f16x2` で実際に往復させ、
   元の値と完全一致するかを見る）を追加し、`QueryAmplitudeStats::
   has_precision_loss` として `select_dot_shader` の新しい拒否条件にする
   （§3 参照）

f32 へ拡張後の演算は、クエリが f16 へ厳密往復可能な場合は
`DOT_SHADER_WGSL`（unpack 版）とビット同一になる（`row` は両シェーダ共通
で既に f16 量子化済みのため、`unpack2x16float` と `vec2<f32>(vec2<f16>)`
はどちらも同じ厳密な f16→f32 拡張）。

## 3. オーバーフローガードと選択規則

f16 の有限最大値（65504）を超える中間値が生じると、f16 算術版だけが該当
行を非有限スコアとして除外し、unpack 版との等価性（受け入れ条件の核心）が
壊れる。これを防ぐため、ホスト側の純関数 `select_dot_shader` で dispatch
前に判定する:

```text
select_dot_shader(f16_available, row_max_abs, query_max_abs,
                   query_has_subnormal_underflow, query_has_precision_loss):
  1. f16_available が false なら Unpack
  2. row_max_abs／query_max_abs のいずれかが非有限なら Unpack
  3. query_max_abs > F16_MAX_FINITE(65504) なら Unpack
     （row_max_abs の値に関わらず崩れる独立した条件。クエリ成分が
     f16 パック時点で ±Inf へ飽和すると、unpack 版では有限のスコアに
     なる行が f16 算術版だけ除外されてしまうケースを閉じる）
  4. query_has_subnormal_underflow が真なら Unpack
     （PR #591 レビュー P1 指摘対応で追加。有効な非ゼロの小さいクエリ
     成分が f16 パックで厳密にゼロへ丸められ正解行が脱落しうるケースを
     閉じる。クエリは F16Arith 選択時のみ f16 パックされるため、この
     アンダーフローはクエリ側にのみ新たに生じるリスク）
  5. query_has_precision_loss が真なら Unpack
     （PR #591 レビュー P1 指摘対応・2 巡目で追加。振幅上限・アンダー
     フローの範囲内でも f16 の仮数部 10 bit で表現できない値は丸め
     られる。`f16_round_trip_exact` で実際に f16 へ往復させ元の値と
     一致するかを見る。§2「単一積の丸め」節参照）
  6. row_max_abs * query_max_abs * GPU_F16_ACC_BLOCK が
     F16_ARITH_PARTIAL_SUM_LIMIT(32768) を超えるなら Unpack
     （保守的なオーバーフロー上界判定。積和は f32 で行う設計へ変更した
     ため必須ではないが、既存の安全マージンとして維持する。§2「単一積の
     丸め」節参照）
  7. それ以外は F16Arith
```

`row_max_abs`（行ごとの有限成分のみの絶対値最大）は `GpuBatchBackend::
try_new` で行ごとに 1 回だけ走査して確定させる（PR #591 レビュー P2 指摘
対応。各行の値は行自身の内容のみに依存する定数のためキャッシュしてよい）。
`batch_search` 呼び出し時は、クエリを `PolicyContext` ごとにグループ化し
（`group_queries_by_ctx`）、**グループ単位**で当該コンテキストが可視な行
（`gather_reachable_rows`）に対応する事前計算値だけを集約する
（`max_abs_finite_from_precomputed_rows`）。`query_max_abs`／
`query_has_subnormal_underflow`（クエリバッチの有限成分のみの絶対値最大・
アンダーフロー有無。f16 丸め前の f32 値）も同じグループのクエリのみを母数
にする（`max_abs_finite_from_queries_subset`）。非有限成分（f16 パック時の
飽和で ±Inf 化した値）は unpack 版でも必ず非有限スコアとして除外される値の
ため、最大値計算から除外してよい（`select_dot_shader` doc コメント参照）。

選択は **`PolicyContext` グループ単位**で行う（PR #591 レビュー P0 指摘
対応で「`batch_search` 呼び出し単位で 1 回」から変更。複数 `PolicyContext`
が混在するバッチで、あるグループの可視行・クエリの振幅が他グループの
シェーダ選択・返却スコアの数値精度へ波及しない——他テナントから不可視な
行の振幅変化が自テナントの検索結果から観測できてしまうテナント境界の
弱体化を防ぐ）。タイル単位にはしない（コード量と検証の単純さを優先した
実装判断は不変）。

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

- `GpuBatchStatsSnapshot` へ `f16_arith_dispatches`（`PolicyContext` グループが
  f16 算術版へ dispatch した回数。PR #591 レビュー P0 指摘対応でシェーダ
  選択がグループ単位になったため、1 `batch_search` 呼び出しで複数
  `PolicyContext` を含むバッチでは複数回加算されうる）・
  `f16_arith_guard_fallbacks`（f16 算術版パイプラインは利用可能だが自動
  選択のオーバーフローガードにより unpack 版へ縮退したグループ数）を
  追加。`gpu_scaling_bench.rs` の `gpu_scaling_stats:` 出力行へ両カウンタを
  追記し、#540 の非 vacuous 判定材料にする
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

## 6.1. 行振幅の事前計算（PR #591 レビュー P2 指摘対応）

初版実装は `batch_search` 呼び出しのたびに、対象クエリバッチが可視な行を
`gather_reachable_rows` で求めたうえで全行を f16 unpack して絶対値最大を
再計算していた（プロセス共有の GPU dispatch ロック保持中に発生する
O(可視行数 × 次元数) の CPU 処理）。各行の絶対値最大は行自身の内容にのみ
依存する定数のため、`GpuBatchBackend::try_new` で全行を 1 回だけ走査して
`row_max_abs: Vec<f32>` へキャッシュし、`batch_search` 呼び出し時は
`PolicyContext` グループの可視行 index に対する単純な配列参照 + 最大値
集約（`max_abs_finite_from_precomputed_rows`）だけで済ませる。

## 7. スコープ外・申し送り

- 前後比較の実測・CORE-16 ゲートへの影響記録は #540 の担当
- `GpuF32ContrastBackend` 側の f16 算術化は対象外（CORE-16 の公平性は
  f32 常駐のまま維持する契約）
- `unsafe`・依存追加なし（wgpu `=30.0.1` 既存依存の範囲内）
