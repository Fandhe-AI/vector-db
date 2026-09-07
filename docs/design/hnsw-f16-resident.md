# HNSW 索引ノードの f16 常駐表現と F16C／NEON fp16 デコード付き dot カーネル

- ステータス: **Implemented（既定 F32・F16 は opt-in）**
- 対応: Issue #514（親 #513・Phase 4 親 #459・ルート #455）
- 準拠 ADR: `docs/design/simd-intrinsics-adoption.md`（決定 1〜5。ステータスは
  ファイル上 Proposed のままだが、同 ADR 配下の先行実装〔Issue #528・PR #582〕が
  既にマージ済みのため、本 Issue は ADR の決定を拘束条件として踏襲する。ADR
  ステータス自体の更新はオーナー作業のため本 Issue では変更しない）
- ポインタ: TASK-132・TASK-156・CORE-16（`docs/spec` 側のビヘイビア定義。
  本文は転記しない）
- 関連コード: `crates/engine/src/f16.rs`（新設）・`crates/engine/src/isa.rs`
  （`F16cToken`／`NeonFp16Token`／`F16Kernel`）・`crates/engine/src/hnsw.rs`
  （`ResidentPrecision`／`NodeVectors`／`NodeSource`）・
  `crates/engine/src/hnsw/prefetch.rs`・`crates/engine/src/hnsw/parallel_build.rs`・
  `crates/engine/src/hnsw/provider.rs`・`crates/engine/src/search_engine.rs`・
  `crates/engine/src/sql/hnsw_cache.rs`・`crates/engine/src/sql/explain.rs`・
  `scripts/check_simd_codegen.sh`
- 関連 doc: `docs/design/hnsw-generation-cache.md`・
  `docs/design/explain-search-engine-exposure.md`・
  `docs/design/simd-codegen-guard.md`・`docs/design/chip-kernel-guidelines.md`

## 背景・目的

`sql::hnsw_cache`（#408〜#411）が保持する HNSW 索引ノードのベクトルは
`HnswIndex.vectors: Arc<[f32]>` として f32 常駐しており、探索中の候補生成
（`search_layer_in`）は毎回 f32 行を読む。本 Issue は ADR #508 決定 5 が定める
唯一の適用 seam（`HnswIndex.vectors`・`sql::hnsw_cache`）に対し、索引ノードを
IEEE binary16（`batch_search.rs::pack_f16x2` と同じ RNE 丸めの `u16` ビット表現）
で常駐させ、探索中の候補生成スコアを「f16→f32 昇格 dot」（x86_64: F16C
`_mm256_cvtph_ps`、aarch64: NEON+FP16 `vcvt_f32_f16`）で計算する。索引ヒットの
最終スコアは既存どおり `sql/hnsw_cache.rs::search_with_overlay` が f32 アリーナ
から `kernel::dot` で再計算する（#408 の契約を維持）。

目的は探索中に移動するバイト数を半減することと、intrinsics 初導入を ADR の
`unsafe` 境界（ポインタ load 不使用・`as_chunks`＋`set` 構築・新規 `unsafe` は
トークン dispatch 箇所のみ）に沿って行うことにある。

## 設計判断

| 論点 | 決定 | 理由 |
| ---- | ---- | ---- |
| D1 常駐精度の既定 | `ResidentPrecision::F32` を既定、`F16` は `ValidatedHnswParams::with_resident_precision(F16)` の opt-in | 既存 HNSW テスト・Recall ゲートをビット同一に保つ |
| D2 パラメータの置き場 | `HnswParams` へフィールド追加せず、`ValidatedHnswParams` の private フィールド（`full_scan_ratio` と同型） | `HnswParams` への追加は破壊的変更（既存コメント参照） |
| D3 索引内の f32 保持 | F16 常駐時は f32 を保持しない（`NodeVectors` enum で排他） | 常駐メモリ半減が目的。最終スコアは既にアリーナ f32 から再計算 |
| D4 差分検出（`Overlay::compute`） | `HnswIndex::node_matches(node, &[f32]) -> Option<bool>` を新設し、F32 は `to_bits` 厳密一致、F16 は現在行を `f32_to_f16_bits` で符号化してから格納ビット列と比較 | `vector()` が F16 常駐時に常に `None` を返す設計（D5）のため、旧 `vectors_bit_equal`（`base.index.vector(node)` 経由）をそのまま使うと F16 索引の差分検出が全行「未変更」と誤判定し、テーブル更新後もキャッシュが古いまま応答し続ける silent fallback になる |
| D5 `vector(node)` の挙動 | F32 常駐は従来どおり `Option<&[f32]>`。F16 常駐は `None`。新設 `vector_f16(node) -> Option<&[u16]>` と `resident_precision()` を追加 | 既存 unit test（F32 既定）は無変更 |
| D6 f16 範囲外（`\|x\| > 65504`）の扱い | 凍結時にスキャンし、1 成分でも範囲外ならその索引は F32 常駐へ自動縮退。`HnswIndexCacheStats` に `f16_residency_fallbacks` を追加 | ±Inf 化による非有限スコアの毎クエリ brute-force 縮退という性能崖を避ける |
| D7 グラフ構築 | 構築（`GraphBuilder`／`parallel_build`）は常に f32 で行い、`freeze_from` で f16 化 | 精度によらずグラフを同一に保つ（`f16_precision_produces_identical_graph_shape_to_f32` で機械検証） |
| D8 tail 処理 | 零埋め固定長バッファ（`[u16; LANES]`／`[f32; LANES]`）を同じ `set` 構築経路へ流す | ADR 決定 2 |
| D9 水平和（x86_64） | `_mm256_extractf128_ps`／`_mm_add_ps`／`_mm_shuffle_ps`／`_mm_add_ss`／`_mm_cvtss_f32` のみ。store intrinsics・`transmute` は使わない | ADR 決定 1 |
| D10 AVX-512 f16 変種 | 本 Issue では作らない（`F16cToken` は `avx2,fma,f16c`） | ADR 決定 3 のトークン表に無い |
| D11 NEON | `NeonFp16Token`（`neon,fp16`）・`dot_f16_neon_fp16` に `#[inline(never)]` | ADR 決定 2（生成コード検査からシンボルを検出可能にする） |
| D12 wire-server | 変更なし | HNSW opt-in の CLI は存在しない |

## データ構造

- `f16.rs`: `f32_to_f16_bits`／`f16_bits_to_f32`（`batch_search.rs` から移設。
  `pack_f16x2`／`unpack_f16x2` は移設前とビット同一）・`fits_f16`・
  `encode_rows`（凍結時の 1 回エンコード。範囲外は `F16EncodeError::OutOfRange`）。
- `isa.rs`: `F16cToken`／`NeonFp16Token`（sealed。`Avx2FmaToken` 等と同じ構築
  契約）・`F16Kernel`（`Scalar`／`F16c`／`NeonFp16`）・`detect_f16`／
  `current_f16`（`SimdKernel`／`current` とは独立の `OnceLock`）・
  `dot_f16_f16c`／`dot_f16_neon_fp16`（intrinsics 本体）・`dot_f16_scalar`
  （参照実装）。新規 `unsafe {` は `F16Kernel::dot_f16` のディスパッチ 2 箇所
  のみ（isa.rs 全体で 3→5）。
- `hnsw.rs`: `ResidentPrecision`（`F32`／`F16`。`Display` は `EXPLAIN` の
  閉じた語彙）・`NodeVectors`（`F32(Arc<[f32]>)`／`F16(Arc<[u16]>)`）・
  `NodeSource` trait（`score`／`touch_prefetch`。`impl NodeSource for [f32]`
  は構築経路〔`GraphBuilder`〕、`impl NodeSource for NodeVectors` は探索経路
  〔`HnswIndex`〕）。`search_layer_in`／`prefetch::PrefetchPolicy::
  prefetch_neighbor` を `NodeSource` でジェネリック化し、構築経路・探索経路が
  同じ関数を常駐精度に依存せず共有する。

## `unsafe` の立証

新規 `unsafe {` はいずれも sealed トークン（`F16cToken`／`NeonFp16Token`）の
所持を根拠とするディスパッチ呼び出し 1 箇所ずつ（`isa.rs::F16Kernel::
dot_f16`）。カーネル本体（`dot_f16_f16c`／`dot_f16_neon_fp16`）自体は
`#[target_feature]` を付けた safe fn で、intrinsics 呼び出しは
target_feature 1.1 の規則により `unsafe` ブロックを要さない（`rustc 1.96.0`
で実機確認済み）。ポインタ load/store intrinsics・`transmute`・raw pointer
キャストは使わない（ADR 決定 1）。`tests/isa.rs::
unsafe_is_confined_to_isa_module_with_safety_comments` が個数（5）・
局在（`isa.rs` 以外に `unsafe` 無し）・`SAFETY:` コメント付与を機械検証する。

## 生成コード検査（`scripts/check_simd_codegen.sh`。受入条件 A1）

`dot_f16_f16c`／`dot_f16_neon_fp16` を必須シンボルへ追加したうえで、
「期待命令が 1 件以上出現する」非 vacuous 検査（`expected_rules_for`／
`scan_expected_missing`）を新設した（既存の禁止命令検査は「あってはならない
命令」のみを検査しており、intrinsics 呼び出しがソフトウェア復号へ静かに
縮退していても検出できないため）。

- x86_64: `dot_f16_f16c` に `vcvtph2ps` がメモリオペランド付き（`vcvtph2ps
  -16(%rdi,%r8), %ymm1` の形。レジスタ→レジスタの `vcvtph2ps %xmm, %ymm` の
  みでは不合格）で 1 件以上出現することを要求する。
- aarch64: `dot_f16_neon_fp16` に `fcvtl`／`fcvtl2`（`v_.4s` 幅拡張）が 1 件
  以上出現することを要求する。

`--self-test` に pass fixture（実際に `vcvtph2ps`／`fcvtl` を含む）・fail
fixture（関数名は一致するがソフトウェア復号のみ）を追加し、双方の判定が
機械検証済み（`make simd-codegen-check`／`make simd-codegen-check-cross` で
x86_64・aarch64 の双方を確認）。基線命令カウントの抜粋:

```text
ok: ...dot_f16_f16c...: ... vcvtph2ps=3 vfmadd132ps=3 ...
ok: ...dot_f16_neon_fp16...: ... fcvtl=1 fmla=1 ...
```

（x86_64 側は `vcvtph2ps` がループ展開により 3 回出現。aarch64 側は
`fcvtl=1` が要求パターンを満たす。いずれも禁止命令〔`vpinsrw`／`vunpck*`・
`ins`／`mov v.[bhsd][n]`／レーン指定 `ld1`〕は 0 件）。

## EXPLAIN 露出（R5）

既存 `hnsw_params: m=...,ef_construction=...,ef_search=...` の末尾へ
`,resident=f32|f16`（構築時の静的設定値。`ValidatedHnswParams::
resident_precision()`）を追記する。実行時の自動縮退結果（D6）・実行時の
可視カーディナリティ・行数等は #411 の契約どおり非露出のまま維持する。

## 検証

- `hnsw.rs::tests`: T-H1〜T-H5 相当（グラフ形状の f32/f16 完全一致・
  `approx_heap_bytes` 削減・`node_matches`/`vector`/`vector_f16` の D4/D5
  契約・f16 範囲外での D6 自動縮退・brute-force 対照 Recall@10）。
- `sql/hnsw_cache.rs::tests::
  f16_resident_precision_hits_the_ann_path_and_matches_default_engine_scores_exactly`:
  F16 opt-in が実際に索引探索へ到達すること（非 vacuous・`hits >= 1`・
  `f16_residency_fallbacks == 0`）と、返るヒットのスコアが既定エンジン
  （`CpuScalarProvider`。f32 brute-force）と `to_bits()` 完全一致すること
  （R3・#408 契約）を固定する。
- `tests/isa.rs`：`F16cToken`／`NeonFp16Token` の sealed 方針・`unsafe`
  個数（5）・f16 昇格 dot のスカラー参照実装との許容差内一致・決定性を検証。
- `tests/sql_explain.rs`・`crates/wire-server/tests/wire_explain.rs`：
  `resident=f32` の既存アサートを更新。

## 既知の限界・スコープ外（後続 Issue へ申し送り）

- AVX-512 `_mm512_cvtph_ps` 変種は未実装（ADR トークン表に無い）。
- `RecallEngine` fixture（`crates/engine/tests/fixtures/recall_engine.rs`）
  への `hnsw_f16` 追加・`recall.yml` matrix 拡張・Recall 3 ゲート同一閾値
  検証は Issue #515 で実施済み（`docs/design/ann-recall-gate-verification.md`
  「Issue #515 追記」節参照）。
- 25k／100k／500k × dim 128／768 規模での常駐メモリ・レイテンシの前後比較
  実測は対象外（#516）。
- 既定常駐精度を F16 へ反転するかどうかは #515／#516 の実測後のオーナー判断。
- wire-server への HNSW／精度 opt-in CLI 追加は対象外。
- NEON カーネルの実機（Apple Silicon 等）での生成コード・性能確認はクロス
  コンパイル確認までが本 Issue の範囲（`make check-cross`／
  `make simd-codegen-check-cross` で確認済み）。
