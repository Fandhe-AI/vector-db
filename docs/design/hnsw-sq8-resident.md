# HNSW 索引ノードの対称 SQ8（i8）量子化常駐表現

- ステータス: **Implemented（既定 F32・I8 は opt-in）**
- 対応: Issue #521（親 #520・Phase 4 親 #459・ルート #455）
- 依存: Issue #514（HNSW 索引ノードの f16 常駐。同じ `NodeVectors`／
  `ResidentPrecision` 拡張構造を踏襲）
- 準拠 ADR: `docs/design/simd-intrinsics-adoption.md` 決定 5（低精度常駐は
  `SearchEngineKind::Hnsw` opt-in 限定・候補生成のみ・最終スコアは
  `kernel::dot` の f32 再計算）
- ポインタ: TASK-132・TASK-156・CORE-16（`docs/spec` 側のビヘイビア定義。
  本文は転記しない）
- 関連コード: `crates/engine/src/sq8.rs`（新設）・`crates/engine/src/hnsw.rs`
  （`ResidentPrecision::I8`／`NodeVectors::I8`／`freeze_from`）・
  `crates/engine/src/hnsw/prefetch.rs`（`touch_node_vector_i8`）・
  `crates/engine/src/sql/hnsw_cache.rs`（`i8_residency_fallbacks`）
- 関連 doc: `docs/design/hnsw-f16-resident.md`（同型の設計判断・実装パターン）・
  `docs/design/gpu-batch-i8-packed.md`（GPU 側の行単位 SQ8。統合は未実施の
  申し送りのまま）・`docs/design/ann-index-adoption.md`（B 案 ADR）

## 背景・目的

Issue #514 で HNSW 索引ノードは f16 常駐（2 byte/要素）まで下がったが、
VNNI／NEON dotprod 系の整数内積カーネル（#522・#524）を載せるための i8 量子化
型そのものがリポに存在しなかった。本 Issue は qdrant `encoded_vectors_u8`
と同型の対称スカラー量子化（次元ごと min/max 由来のスケール）を索引凍結時
（`HnswIndex::freeze_from`）に 1 回行い、量子化スケールを索引エントリ内に
保持する i8 常駐表現を追加する。

## 量子化の定義

- 統計の母集団: 凍結に渡された `vectors`（row-major・`n × dim`）のみ
  ——ADR 決定 5「事後フィルタ不採用」に沿い、`sql::hnsw_cache::IndexedBase::
  build` が受け取る ctx 可視アリーナに閉じる（下記「テナント境界」節参照）。
- 次元 `d` ごとに `min_d`／`max_d` を収集し、対称スケール
  `s_d = max(|min_d|, |max_d|) / 127`（f64 で計算してから f32 へ丸める。
  `gpu_batch/packed_i8.rs::row_scale_f64` と同じ「丸めまで f64 精度で行う」
  規律を踏襲——#598 codex P1 指摘の再発防止）。
- 格納コード: `q_d = clamp(round_half_away_from_zero(v_d / s_d), -127, 127)`
  を `i8` で保持（`-128` は使わない）。`s_d == 0`（定数 0 次元）は除算せず
  `0`。
- **クエリ側は量子化しない**: `f16.rs` の昇格 dot（f16 行を f32 化してから
  f32 クエリと内積する）と同じ非対称設計を採用し、候補生成スコアは
  `Σ_d dequantize(code_d, scale_d) * query_d`（`dequantize(c, s) = c as f32 * s`）
  を f32 で計算する（`sq8.rs::dot_i8_f32`）。qdrant 型の「クエリ側も量子化して
  i32 累積 dot を取る」二重量子化方式（VNNI `dpbusd` 向け）は本 Issue では
  採用しない——整数 i8×i8 dot カーネルは #522／#524 の担当であり、本 Issue の
  スコープは「量子化・逆量子化」と「i8 常駐表現」に限定される（Issue 本文の
  受け入れ条件どおり）。この単純化により `sq8.rs` はクエリ側の二重量子化・
  i32 オーバーフローガード・行和保持を持たない。

## テナント境界

`IndexedBase::build`（`sql/hnsw_cache.rs`）は ctx 可視アリーナのみを受け取り、
索引エントリは `(table, PolicyContext)` × テーブル世代でキー付けされる
（ADR 決定 5「事後フィルタ不採用」）。したがって `scale_d` は ctx が既に
読める行にしか依存せず、不可視行の値・存在は量子化スケールへ一切流入しない
——構造的な保証であり、Issue #514（f16 常駐）と同一の性質を i8 常駐でも
そのまま持つ。専用の統計縮約オラクル回帰テストは追加していない（f16 版
（Issue #515）でも同種テストは追加されておらず、本表現も `IndexedBase::build`
の既存構造に完全に追従するため、構造的な保証が既存の RLS 統合テスト
（`sql/hnsw_cache.rs::run_r4_tenant_isolation_never_leaks_across_ctx` 等）で
間接的に検証される）。

## 設計判断

| 論点 | 決定 | 理由 |
| ---- | ---- | ---- |
| 常駐精度の既定 | `ResidentPrecision::F32` を既定、`I8` は `ValidatedHnswParams::with_resident_precision(I8)` の opt-in | f16 版 D1 と同型。既存 HNSW テスト・Recall ゲートをビット同一に保つ |
| クエリ側の扱い | 量子化せず f32 のまま（`f16.rs` の昇格 dot と同型の非対称設計） | 二重量子化（VNNI 向け i32 累積 dot）は整数カーネル担当の #522 のスコープであり、本 Issue の範囲外（量子化・i8 常駐表現に限定） |
| 量子化パラメータの保持形 | `Sq8DimParams { scales: Vec<f32> }`（次元ごとのスケールのみ） | 対称量子化のため次元ごとの平行移動を持たず、`min_d`／`max_d` はスケール算出のためだけの一時値。復号・範囲検査（`node_matches`）・候補スコアいずれも `scale_d` のみで完結する |
| `node_matches` の範囲検査 | 量子化前に `\|candidate_d\| > 127 * scale_d` を先に検査し、超えていれば直ちに不一致と判定する | 127 段の粗い分解能では、fit 済み範囲の極値（コード ±127）にあった行がさらに大きい値へ更新されてもクランプにより「たまたま同じコード」になり誤って「未変更」と判定されうる（f16 版には無い i8 固有の穴）。最終スコアは常に f32 アリーナから再計算されるため正しさ自体は不変——影響はグラフ近傍構造の再利用判定のみ |
| 範囲外・非有限成分の扱い | `sq8::fit_dim_params`／`sq8::encode_rows` が失敗した場合、凍結時に `F32` 常駐へ自動縮退（f16 版 D6 と同型） | 性能崖を避ける fail-closed な選択。`HnswIndexCacheStats::i8_residency_fallbacks` へ計上 |
| グラフ構築 | 構築（`GraphBuilder`／`parallel_build`）は常に f32 で行い、`freeze_from` で i8 化 | f16 版 D7 と同型。精度によらずグラフを同一に保つ（`i8_precision_produces_identical_graph_shape_to_f32` で機械検証） |
| `NodeSource::score` のシグネチャ | 変更なし（`score(dim, node, query: &[f32])`） | f16 版と同じく「格納側だけ低精度化しクエリは f32 のまま」の設計のため、クエリ量子化状態を呼び出し間で共有する `QueryView` 等の追加抽象化は不要 |

## データ構造

- `sq8.rs`: `Sq8DimParams`（`scales: Vec<f32>`。`dim()`／`scales()`／
  `approx_heap_bytes()`）・`fit_dim_params(dim, rows) -> Result<Sq8DimParams,
  Sq8Error>`（凍結時に 1 回。`rows` が空でも成功——全次元スケール 0）・
  `encode_rows(dim, rows, &params, out: &mut Vec<i8>)`（`try_reserve_exact`
  によるフォールブル確保。失敗時 `out` 不変）・`dequantize(code, scale) ->
  f32`・`dot_i8_f32(codes, scales, query) -> f32`（候補生成スコア）・
  `quantize_scalar_f64`（`packed_i8.rs` と同じ半整数丸め規則）・`Sq8Error
  { NonFinite, InvalidShape, AllocationFailed }`。
- `hnsw.rs`: `ResidentPrecision::I8`（`Display` は `"i8"`）・
  `NodeVectors::I8 { codes: Arc<[i8]>, params: Arc<Sq8DimParams> }`・
  `node_vector_i8`（`node_vector_u16` の i8 版）・`vector_i8(node) ->
  Option<&[i8]>`（D5 と同型。`vector()` は I8 常駐時 `None`）。
- `hnsw/prefetch.rs`: `touch_node_vector_i8`（`touch_node_vector_u16` と同型。
  先頭 1 要素を触れるだけで復号は行わない）。
- `sql/hnsw_cache.rs`: `HnswIndexCacheStats::i8_residency_fallbacks`
  （`f16_residency_fallbacks` と同型。`IndexedBase::build` の戻り値を
  `(Self, bool /* f16_fallback */, bool /* i8_fallback */)` へ拡張）。

## 検証

- `sq8.rs::tests`: 逆量子化誤差上界（`\|v - dequantize(q, s)\| <= s/2 +
  eps`）・内積誤差上界（クエリ非量子化のため寄与項はノード側のみ）・境界値
  （定数 0 次元・極小非零行・極大有限値）・fail-closed（非有限・形状不一致・
  `out` 不変）・決定性・`i8::MIN` 不使用の固定。
- `hnsw.rs::tests`: T-I1〜T-I5 相当（グラフ形状の f32/i8 完全一致・
  `approx_heap_bytes` 削減・`node_matches`／`vector`／`vector_i8` の契約
  ＋範囲検査回帰・非有限成分での自動縮退・brute-force 対照 Recall@10。
  クラスタ構造ありフィクスチャでの層 A 縮小規模実測は informational——
  Recall ゲート同一閾値検証は後続 #523 の担当）。
- `sql/hnsw_cache.rs::tests::
  i8_resident_precision_hits_the_ann_path_and_matches_default_engine_scores_exactly`:
  I8 opt-in が実際に索引探索へ到達すること（非 vacuous・`hits >= 1`・
  `i8_residency_fallbacks == 0`）と、返るヒットのスコアが既定エンジン
  （`CpuScalarProvider`。f32 brute-force）と `to_bits()` 完全一致すること
  を固定する（f16 版と同型）。
- `tests/hnsw_cache.rs::assert_resident_precision_reached`: `resident=i8`
  の表示・`i8_residency_fallbacks == 0` の網羅 match へ I8 分岐を追加。

## 既知の限界・スコープ外（後続 Issue へ申し送り）

- 整数 i8×i8 dot カーネル（VNNI `dpbusd`／NEON dotprod）は本 Issue の対象外
  （#522・#524）。VNNI 向けにノードコードの行和（`Σ r_d`）を保持する拡張が
  必要になった場合は #522 が `NodeVectors::I8` へフィールドを追加する。
- `RecallEngine` fixture（`crates/engine/tests/fixtures/recall_engine.rs`）
  への `hnsw_i8` 追加・`recall.yml` matrix 拡張・Recall 3 ゲート同一閾値
  検証・`bench-knn-profile` 等の `hnsw_i8` トークン追加は #523 の担当。
- `gpu_batch/packed_i8.rs`（GPU・行単位対称スケール）の本モジュールへの
  統合は未実施（#598 側の申し送りのまま。行単位／次元別で方式が異なるため
  統合形は #522 実装後に判断）。
- `EXPLAIN` の `resident=` 表記は `f32|f16|i8` の閉じた語彙へドキュメンテー
  ションコメントのみ更新済み（`explain.rs` 本体は `Display` 経由のパス
  スルーのため変更不要）。
- wire-server への HNSW／精度 opt-in CLI 追加は対象外（f16 版 D12 と同型）。
