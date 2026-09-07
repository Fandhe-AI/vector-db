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

## Issue #522: VNNI（512bit／256bit）と i16 widen フォールバックの i8 dot カーネル

- ステータス: **Implemented**（`NeonDotprodToken` 経路は #525 の担当のまま）
- 対応: Issue #522（親 #520・前提 #521）
- 準拠 ADR: `docs/design/simd-intrinsics-adoption.md` 決定 1〜5
- 関連コード: `crates/engine/src/isa.rs`（`I8Kernel`／`AvxVnniToken`／
  `Avx512VnniToken`。`Avx2FmaToken` を widen 経路に再利用）・
  `crates/engine/src/isa/x86_i8.rs`（新設。3 カーネル本体）・
  `crates/engine/src/sq8.rs`（`I8_DOT_MAX_DIM`／`row_sums`／
  `Sq8QueryCodes`／`prepare_query`／`score_from_int_dot`）・
  `crates/engine/src/hnsw/i8_query.rs`（新設。`PreparedI8Source`）・
  `crates/engine/src/hnsw.rs`（`NodeVectors::I8` へ `row_sums` 追加・
  `search_masked_with_hop` の per-search 準備クエリ結線）

Issue #521 の申し送りどおり、候補生成スコアを「クエリ非量子化の復号 dot」
（`sq8::dot_i8_f32`）から「クエリ側も量子化する二重量子化」＋整数
`i8×i8→i32` カーネルへ切り替える。

### 導出（u8 シフト＋行和補正）

`vpdpbusd`（u8×s8→i32 の非飽和積和）は一方のオペランドを符号なしにする
必要があるため、ノード側コード `code_d ∈ [-127, 127]`（`sq8.rs` 既存の
対称量子化）は変更せず、クエリ側だけを符号なしへシフトする:

1. `q'_d = scale_d * q_d`（ノードと同じ次元別スケール空間へ写像）
2. `s_q = max_d|q'_d| / 127`（クエリ単一スケール）
3. `qq_d = clamp(round_half_away(q'_d / s_q), -127, 127)`（符号付きコード。
   i16 widen カーネルはこれを直接使う）
4. `u_d = (qq_d as i16 + 128) as u8 ∈ [1, 255]`（VNNI 系カーネルが使う
   符号なしコード）

VNNI 系カーネルは `acc = Σ u_d * code_d` を計算し、`row_sum = Σ_d code_d`
（`sq8::row_sums`。`hnsw.rs::NodeVectors::I8` が凍結時に 1 回だけ計算し
`codes`／`params` と寿命・対応関係を一致させて保持）を使って
`int_dot = acc − 128 * row_sum` で真の内積を復元する（`acc = Σ(qq_d+128)*
code_d = int_dot + 128*row_sum` の代数的帰結）。i16 widen（`vpmaddwd`）・
スカラー参照実装は `qq_d` を直接使うため補正不要。全経路とも `i32` の
wrapping 加算で計算するため、[`I8_DOT_MAX_DIM`]（=65,536。`255*127*dim ≤
i32::MAX` を満たす上限の逆算）以下では ISA に依らずビット同一になる
（`isa.rs::i8_kernel_tests`・`tests/isa.rs` が dim 0..=300 の全 `available_
i8_kernels()` で機械検証）。

### fail-closed 縮退

`sq8::prepare_query` が失敗（`dim > I8_DOT_MAX_DIM`・クエリ側スケール
`s_q` の復号後最悪値が `f32` で非有限になり得る）した場合、
`hnsw/i8_query.rs::PreparedI8Source` はそのクエリに限り既存の
`dot_i8_f32`（復号 dot）へ縮退する——索引の再構築・空集合の誤返却を
招かない。`PreparedI8Source::score` は準備済みクエリと異なるスライスで
呼ばれた場合（構造上到達しない防御的経路）を `ptr::eq` で検出し
`HnswError::InvalidParams` を返す（fail-closed）。

### `NodeSource` への結線

`NodeSource` trait 自体は無変更。`HnswIndex::search_masked_with_hop` が
探索呼び出し 1 回につき 1 回だけ `PreparedI8Source::new`（`NodeVectors::I8`
の場合のみ）を構築し、`greedy_descend_masked`／`search_layer_with_hop` の
`vectors` 引数型を `&NodeVectors` から `&dyn NodeSource`（trait object。
`NodeVectors`／`PreparedI8Source` いずれも自動 unsized coercion で渡せる）
へ一般化した。`search_layer_in`（Issue #494 で既にジェネリック
`S: NodeSource + ?Sized`）はこの変更を要しない。索引ヒットの最終スコアは
常に `kernel::dot`（f32・アリーナ再計算）のまま不変（ADR 決定 5）。

### `unsafe`・codegen ガード

`isa.rs` の `unsafe { }` は 8 → 11（`I8Kernel::dot_i8` の Avx2Widen・
AvxVnni・Avx512Vnni ディスパッチ 3 箇所。`x86_i8.rs` 本体は `unsafe` を
持たない safe fn のみ）。`scripts/check_simd_codegen.sh` に必須シンボル・
期待命令規則（メモリオペランド付き `vpdpbusd`〔%zmm／%ymm〕・`vpmaddwd`＋
`vpmovsxbw`）・self-test fixture を追加した。実装中に発見した
`mnemonic_of` のバグ（AVX-VNNI の `{vex}` encoding hint 接頭辞を先頭
トークンと誤認し、以降の禁止命令・期待命令検査の双方が素通りしていた。
`{vex}`／`{evex}` 等の波括弧接頭辞を除去してから判定する形へ修正）を
本 Issue の一環で修正した——既存 f16／block4 カーネルへの影響は無い
（`{vex}` は AVX-VNNI の VEX 符号化明示にのみ現れる）。

### 判断事項（自動運転モードで安全側に確定）

| 論点 | 決定 | 理由 |
| ---- | ---- | ---- |
| 符号なし化する側 | クエリ側（u8 シフト）＋ノード行和補正 | `codes: Arc<[i8]>`・`vector_i8`・`node_matches`・GPU `packed_i8` を無変更に保てる |
| i16 widen 経路のトークン | 既存 `Avx2FmaToken` を再利用（新規トークンなし） | ADR 決定 3 の表にある既存トークンで `avx2+fma ⊇ avx2` の SAFETY 根拠が成立するため |
| クエリ準備の縮退先 | `dot_i8_f32`（#521 の復号 dot）を残置して利用 | 索引再構築・空集合を招かない fail-closed |
| `NodeSource` の拡張形 | 新規メソッド・trait 変更なし。`&dyn NodeSource` への一般化のみ | 変更範囲を `search_masked_with_hop` とその直接の呼び出し先に限定できる |
| `dot_i8_scalar` の per-lane 命令（punpcklbw／aarch64 `mov v.b[..]`） | codegen ガードの対象外として関数名で除外（`#[inline(never)]` で `I8Kernel::dot_i8` への巻き込みも防止） | スカラー参照実装の自動ベクトル化はガードが検出したい「手書き intrinsics の `set` 構築が gather/stride 由来で退化した」ケースとは別物。`dot_scalar`／`dot_f16_scalar` が偶然この命令を出さないだけで、除外規則自体は既存の `_scalar` 系関数と同じ立ち位置 |

### 検証の限界（申し送り）

本開発環境の CPU は VNNI 非対応（`avx avx2 f16c fma` のみ）のため、
`Avx512Vnni`／`AvxVnni` 2 経路は実行できず、コンパイル・codegen 検査
（`--emit asm`）・ローカル `rustc` での命令列確認までに留まる。実機での
実行検証（ビット同一性・チップ別性能）は CI（GitHub ホステッド runner が
対応 CPU の場合）・#523（Recall 3 ゲート同一閾値検証）・#530（チップ別
前後比較）へ申し送る。

## 既知の限界・スコープ外（後続 Issue へ申し送り）

- NEON dotprod（`vdotq_s32`）版の i8 整数カーネルは #525 の担当。
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
