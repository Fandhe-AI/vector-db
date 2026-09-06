# ADR: wgpu での部分 Top-k（bitonic／radix select）と SUBGROUP 可用性

- ステータス: Proposed（実装 #536・前後比較 #537 の実測で Accepted／Rejected を
  確定する。オーナー承認ゲートではない——`docs/spec/05-tasks.md` TASK-128〜130
  の実装契約に沿った設計判断のうえで、実測により方式の採否を決める性質のため）
- 対応: Issue #535（親 #534・Phase 5 親 #460・ルート #455）
- 関連ポインタ: `docs/spec/04-behavior/core-engine.md`（CORE-6・CORE-8・
  CORE-16）・`docs/spec/05-tasks.md`（TASK-128・TASK-129・TASK-130）。
  spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/gpu_batch.rs`（`DOT_SHADER_WGSL`・
  `DOT_SHADER_F32_WGSL`・`init_gpu_context`・`run_tiled_batch_search`・
  `plan_query_tile`・`finalize_gpu_hits`）・`crates/engine/src/kernel.rs`
  （`TopKSelector`・`MinHeapItem`）・`crates/engine/src/batch_fallback.rs`
  （`revalidate_primary_hits`）・`crates/engine/src/batch_search.rs`
  （`MAX_BATCH_K`・`MAX_BATCH_QUERIES`・`MAX_BATCH_ROWS`・`MAX_BATCH_WORK`）
- 関連 doc: [`gpu-batch-wgpu-enablement.md`](gpu-batch-wgpu-enablement.md)・
  [`hotpath-implementation-survey.md`](hotpath-implementation-survey.md) §8・
  [`chip-kernel-guidelines.md`](chip-kernel-guidelines.md) §1-5／§4・
  [`crossdb-bench.md`](crossdb-bench.md)（GPU 節）・
  [`benchmark-judgement-policy.md`](benchmark-judgement-policy.md)
- 本 doc は #536（部分 Top-k シェーダ＋CPU 側最終マージ実装）・#537（readback
  バイト数とレイテンシの前後比較）がそのまま着手できる粒度で設計を固定する
  ものであり、`crates/engine/src/` の実装コード変更は含まない
  （[out-of-scope-tracking](../../.claude/rules/out-of-scope-tracking.md)）

## 背景・目的

現状（`gpu_batch.rs`。Issue #532 適用後）: `DOT_SHADER_WGSL` は
`PolicyContext` 単位にグループ化したクエリを最大 `GPU_QUERY_TILE_MAX`
（実装既定値 16）本まで 1 dispatch へタイル化し、各 invocation が 1 行の
内積を計算して `scores[q * row_count + i]` へ**全行分の f32 を書き出す**。
ホスト `run_tiled_batch_search` は `GPU_SCORE_BUFFER_BUDGET_BYTES`
（実装既定値 32 MiB）を上限に行チャンクへ分割し、readback した全スコアを
CPU の `TopKSelector`（`kernel.rs`。同点は候補識別子＝常駐行列スロット番号
昇順）へ push して Top-k を選出する。

課題: readback 量が `クエリ本数 Q × row_count × 4 バイト` に比例する。
`crossdb-bench.md` の GPU 節（Issue #532 適用前）の実測では、行数規模が
大きく一括処理クエリ本数が多い条件で GPU 経路が CPU-SIMD 経路と同等〜
逆転する場面があり、`hotpath-implementation-survey.md` §8（faiss
`WarpSelect`／`BlockSelect` の「GPU 上で Top-k まで完結させ readback を
`k` 件程度に絞る」設計）が本 Issue ツリー（#534）の優先候補になっている。

本 doc の目的は、#536 がそのまま実装へ移せる粒度で以下を 1 つに固定する
こと。

1. GPU 側部分 Top-k のアルゴリズム選定（bitonic／radix select／subgroup 縮約）
2. `Features::SUBGROUP` の backend 別可用性と naga 30.0.1 の WGSL 制約
3. SUBGROUP 非対応時の fail-closed 縮退
4. 決定性・fail-closed・DoS 上限の維持契約
5. RTX 3060 実機での `adapter.features()` 実測

## 機械検証した事実

### wgpu =30.0.1／naga 30.0.1

`Cargo.toml` に固定された wgpu `=30.0.1`（同時に naga も `=30.0.1`）の
ソースを確認した結果。

| 項目 | 事実 |
| ---- | ---- |
| `Features::SUBGROUP` | compute／fragment での subgroup 組み込み・演算を許可する（barrier を除く）native only の feature。対応 backend: Vulkan／DX12／Metal |
| `Features::SUBGROUP_BARRIER` | `subgroupBarrier()` 用。**Vulkan／Metal のみ（DX12 非対応）**。`SUBGROUP` が前提 |
| `Features::SUBGROUP_VERTEX` | vertex 用。Vulkan のみ。本設計では不使用 |
| subgroup サイズの取得先 | `Limits` ではなく `AdapterInfo::subgroup_min_size`／`subgroup_max_size`。取りうる範囲は 4〜128 |
| Vulkan での `SUBGROUP` 判定 | API 1.3 以上または `VK_EXT_subgroup_size_control` 拡張があり、基本演算グループ（basic／vote／arithmetic／ballot／shuffle／shuffle relative／quad）をすべてサポートし、compute と fragment 両ステージに対応するとき true。true のとき `SUBGROUP_BARRIER` も同時に true |
| DX12 での `SUBGROUP` 判定 | Shader Model 6.0 以上かつ wave 演算対応のとき true |
| Metal での `SUBGROUP` 判定 | SIMD scoped operations 対応のとき `SUBGROUP`／`SUBGROUP_BARRIER` ともに true |
| full subgroup の保証 | wgpu-hal は Vulkan の「full subgroups 要求」相当の設定を行わない。`num_subgroups × subgroup_size == workgroup_size` も `local_invocation_id` とレーンの連続対応も**保証されない** |
| WGSL の `enable subgroups;` | naga 30.0.1 は**未実装の enable 拡張として parse エラーを返す**（上流の既知の追跡課題あり）。subgroup 組み込み関数・変数（`subgroupMax`／`subgroupMin`／`subgroupBallot`／`subgroupShuffleXor`／`subgroupBroadcast`／`subgroup_size`／`subgroup_invocation_id`／`num_subgroups`／`subgroup_id` 等）は **`enable` 宣言無しで使用**し、デバイス生成時の `Features::SUBGROUP` 要求 → シェーダ validator の capability 判定で門番される |
| backend 側の lowering | SPIR-V／HLSL／MSL の 3 backend いずれも naga が subgroup 組み込みの変換を実装済み |

### RTX 3060 実測（2026-09-06・本開発環境・Vulkan backend・driver 595.71.05）

`crates/engine/examples/gpu_adapter_info.rs`（`make gpu-adapter-info`。§6
「再現手順」参照）による実測。

| 項目 | 値 |
| ---- | -- |
| adapter | NVIDIA GeForce RTX 3060／Vulkan／DiscreteGpu |
| `subgroup_min_size`／`subgroup_max_size` | 32／32 |
| `SUBGROUP`／`SUBGROUP_BARRIER`／`SUBGROUP_VERTEX` | true／true／true |
| `SHADER_F16`／`SHADER_INT64`／`TIMESTAMP_QUERY`／`MAPPABLE_PRIMARY_BUFFERS` | true／true／true／true（#538・#544 の参考値） |
| `max_compute_workgroup_storage_size` | 49,152 バイト |
| `max_compute_invocations_per_workgroup`／`max_compute_workgroup_size_x` | 1,024／1,024 |
| `max_compute_workgroups_per_dimension` | 65,535（`init_gpu_context` の実装既定フォールバック値と一致） |
| `max_storage_buffer_binding_size`／`max_buffer_size` | 2,147,483,644／4,294,967,295 |

あわせて、`Features::SUBGROUP` を要求したデバイス上で `enable` 宣言無しに
subgroup 組み込み（`subgroupMax`／`subgroupBallot`／`subgroupShuffleXor`／
`subgroup_size`／`subgroup_invocation_id` を含む compute シェーダ）を検証
した結果、validation エラーなしでモジュール生成できることを確認した。
同シェーダの先頭に `enable subgroups;` を追加すると parse エラーになる
ことも確認した。`Features::empty()` のデバイスへ同シェーダを渡すと
エントリポイント無効の validation エラーとなり（panic せず error scope で
捕捉可能）、これが決定 2（fail-closed 縮退）の根拠になる。

### 本リポ側の既存契約

- `kernel.rs::MinHeapItem::cmp`: スコア降順（`f32::total_cmp`）、同点は
  候補識別子（バッチ経路では常駐行列スロット番号）昇順。`TopKSelector::push`
  は非有限スコアを無視する。`batch_fallback.rs::revalidate_primary_hits`
  はこの順序規約を独立に再検証する。
- `run_tiled_batch_search`: readback したスコア数が期待値と不一致なら
  `TransferFailed`、非有限スコアは skip、結果欠落は `KernelLaunchFailed`
  （fail-closed）。`finalize_gpu_hits` は `resolve_batch_slot`＋
  `PolicyContext::is_visible` をすべての候補へ通す（GPU にテナント判定を
  持ち込まない設計を維持する）。
- `batch_search.rs`: `MAX_BATCH_K`・`MAX_BATCH_QUERIES`・`MAX_BATCH_ROWS`・
  総量ガード `MAX_BATCH_WORK` による DoS 上限。
- `init_gpu_context`: `required_features: Features::empty()`（現状は
  無要求）・`Limits::downlevel_defaults()` に実測 limits を一部上書き。
  WGSL 定数とホスト定数の一致は機械検証テストで固定する既存方式
  （`GPU_QUERY_TILE_MAX` の例）を踏襲する。

## 候補比較

| 候補 | 方式 | readback／dispatch | dispatch 数 | k 上限 | SUBGROUP 依存 | 決定性 | 評価 |
| ---- | ---- | ------------------ | ----------- | ------ | ------------- | ------ | ---- |
| A. barrier-only bitonic | workgroup 共有メモリ上で `(score, slot)` の bitonic 全ソート → 先頭 `min(k, ワークグループサイズ)` を出力 | `Q × ワークグループ数 × k_out × 8 バイト` | 1 | ワークグループサイズ（実装既定 256） | 不要（`workgroupBarrier` のみ） | 厳密全順序で保証 | 全 adapter で動く基準形。段数はワークグループサイズの対数オーダー |
| B. subgroup-shuffle bitonic（faiss `WarpSelect` 相当） | A と同一ネットワークで、ストライドが subgroup サイズ未満の段を `subgroupShuffleXor` に置換（barrier 不要な段のみ） | A と同じ | 1 | A と同じ | 必要 | A と同一結果（同一ネットワーク） | 本命。full subgroup が保証されないため実行時分岐が必要 |
| C. radix select | f32 を単調な u32 キーへ変換し、複数 pass の histogram（workgroup 局所＋global atomic）で k 位閾値を決め、compaction pass で閾値以上を出力 | `k + 同点数` × 8 バイト（ワークグループ数に非依存） | 5 以上＋pass 毎の CPU 往復 | `MAX_BATCH_K` まで拡張可能 | 不要（atomics） | compaction 順は非決定的→CPU 側ソートで決定化。同点群が閾値を跨ぐ場合の縮退が別途必要 | 大 k・ワークグループ数が多い規模向けの条件付き候補。#536 では不採用 |
| D. subgroup 反復 argmax | `subgroupMax` → 同点は `subgroupMin(slot)` を k 回反復し、subgroup ごとに k 件を求めてから workgroup 内で統合 | A と同じ | 1 | `min(k, ワークグループサイズ)`（k に比例するコスト） | 必要（サイズ非依存の reduction のみ） | 保証 | レーン連続性の仮定が不要で最も頑健だが計算量が `O(k × n / subgroup_size)`。k が小さい場合の代替候補 |
| 現状 | 全スコア readback＋CPU `TopKSelector` | `Q × row_count × 4 バイト` | 1 | 無制限 | 不要 | 保証 | 縮退先として維持する |

## 決定 1: #536 で実装する方式は「B（A のネットワークを共有し、SUBGROUP 有効時のみ subgroup 段を使う）」＋ CPU 最終マージ

- 1 dispatch 内で現行の内積計算（S0）に続けて、クエリごとに部分 Top-k
  選出（S1）を行う。各ワークグループが `K_OUT = min(k, ワークグループ
  サイズ)` 件を `(score, slot)` のペアとして降順で出力し、ホストは
  `Q × ワークグループ数 × K_OUT` 件だけを readback して既存
  `TopKSelector` へ push する（`finalize_gpu_hits` は無変更）。
- 全順序は「score 降順、同点は slot 昇順」。比較述語は
  `a.score > b.score || (a.score == b.score && a.slot < b.slot)` とし、
  `MinHeapItem::cmp` と一致させる。
- 番兵（`i >= row_count` のレーン・非有限スコア）は `score = 負の無限大`・
  `slot` を無効値とし、ホスト側で無効値または非有限スコアを skip する
  （`TopKSelector::push` の非有限無視と二重防御になる）。
- 正しさの根拠: 厳密全順序のもとで「各ワークグループの上位 `K_OUT` の
  和集合 ⊇ 全体の上位 k（k ≤ K_OUT）」が成り立つため、CPU マージ結果は
  **現行 GPU 経路（全量 readback）とビット同一の id 列**になる（スコアは
  同一シェーダ計算のまま。CPU-SIMD 対照との比較は従来どおり境界同点許容
  方式を維持する）。
- subgroup 段の適用条件（シェーダ内の uniform 分岐）: 「ワークグループ内の
  subgroup 数 × subgroup サイズ = ワークグループサイズ」かつ「subgroup
  サイズが 2 のべき乗」かつ「subgroup サイズが 4 以上 128 以下」の場合に
  限り、subgroup 内部の論理位置（`subgroup_id` と `subgroup_invocation_id`
  から算出）で共有メモリを添字し、ストライドが subgroup サイズ未満の段を
  `subgroupShuffleXor`、それ以外を共有メモリ＋`workgroupBarrier` で実行
  する。条件を満たさない場合は全段を `local_invocation_id` 添字の
  barrier-only（候補 A）で実行する。`local_invocation_id` とレーン番号の
  対応関係は一切仮定しない。`subgroupBarrier`（`SUBGROUP_BARRIER`。DX12
  非対応）は使わない。

## 決定 2: SUBGROUP 可用性の判定と fail-closed 縮退

- 初期化時に `adapter.features().contains(Features::SUBGROUP)` を確認し、
  **含む場合のみ** `required_features` へ `SUBGROUP` を加える。無条件に
  要求すると非対応 adapter で `request_device` が失敗し、現行の GPU 経路
  自体が使えなくなる退行になるため避ける。`SUBGROUP_BARRIER`／
  `SUBGROUP_VERTEX` は要求しない。
- Top-k シェーダ（subgroup 組み込みを含む版）は **`SUBGROUP` 有効デバイス
  でのみ**生成する。WGSL に `enable subgroups;` は**書かない**（naga
  30.0.1 が parse エラーを返すため）。
- Top-k パイプラインが未生成・k が上限超過・adapter の subgroup サイズが
  許容範囲外・パイプライン生成失敗のいずれかに該当する場合は、**現行の
  全量 readback 経路をそのまま使う**。GPU 経路自体を落とすのではなく、
  「Top-k のみ GPU 化を諦める」段階的な fail-closed とする。
- SUBGROUP 可用性・k・パラメータから経路を決める判定は、GPU 非依存の
  純関数として切り出す（引数: features・adapter 情報・k・ワークグループ
  サイズ。戻り値: 部分 Top-k 経路か全量 readback 経路か）。この純関数を
  単体テスト（GPU 不要）で「SUBGROUP 無し→全量 readback」「k が上限超過→
  全量 readback」「subgroup サイズが範囲外→全量 readback」に固定する
  （本開発機の RTX 3060 では SUBGROUP を外せないため、この分離が非対応
  ケースを検証する唯一の手段になる）。
- 縮退の観測可能性: バックエンドへ部分 Top-k dispatch 回数・全量 readback
  縮退回数の統計カウンタを持たせ、#537 の非 vacuous 判定（既存ベンチの
  「呼び出しが実際に発生したことを確認する」方式）に使う。

## 決定 3: 定数・バッファ計画

- Top-k 出力件数の上限をワークグループサイズと同値の新定数として定義し、
  WGSL 側の定数とホスト側の定数が一致することを既存方式（`GPU_QUERY_TILE_MAX`
  の機械検証テストと同型）で固定する。
- チャンク分割の予算計算を Top-k 経路では「クエリ本数 × ワークグループ数 ×
  出力件数 × 8 バイト＋行 ID」に置き換える（readback が小さくなるぶん
  分割回数が減る）。全量 readback 経路は現行の計算式のまま維持する。
- readback 検証: 候補件数が期待値と不一致なら `TransferFailed`。範囲外の
  slot（番兵以外）を検出した場合も readback 破損とみなし `TransferFailed`
  とする。
- 部分 Top-k 用の共有メモリ使用量は `Limits::downlevel_defaults()` の
  `max_compute_workgroup_storage_size`（16,384 バイト）に収まる想定
  （RTX 3060 実測の 49,152 バイトはこの既定値を上回るため制約にならない）。
  limits の変更は不要。

## 決定 4: 維持契約（#536 の受け入れ条件へ転記する）

1. 結果の id 列・スコアは現行 GPU 全量 readback 経路とビット同一（同一
   入力・同一 k）。
2. 同点は常駐行列スロット昇順（`revalidate_primary_hits` が独立に検証する
   契約を維持）。
3. 非有限スコア・番兵を除外する。
4. テナント境界判定は GPU に持ち込まない（`gather_reachable_rows`／
   `finalize_gpu_hits` は無変更のまま維持）。
5. `MAX_BATCH_WORK`・`MAX_BATCH_K`・`MAX_BATCH_QUERIES` の既存ガードは
   無変更。
6. `unsafe`・環境変数上書き（CORE-12）を追加しない。
7. GPU 待機はすべて既存の poll 締切内で完結させる。パイプライン生成は
   error scope の内側で行う。
8. SUBGROUP 非対応・k 超過・パイプライン生成失敗はすべて全量 readback へ
   縮退し、部分結果を返さない。

## 決定 5: #536／#537 への申し送り（実装タスク対応表）

| 項目 | 担当 | 内容 |
| ---- | ---- | ---- |
| Top-k シェーダ・経路判定純関数・統計カウンタ | #536 | 決定 1〜3 |
| 単体テスト（GPU 不要） | #536 | WGSL 定数一致・経路判定の分岐・readback 長不一致・番兵 skip |
| GPU 実機テスト（GPU 検出時のみ） | #536 | 全量 readback 経路との id 列ビット同一・重複行（同点）・NaN 行・k の境界値（1・上限・上限+1）・row_count がワークグループサイズの非倍数 |
| 前後比較（readback バイト数・レイテンシ） | #537 | `benchmark-judgement-policy.md` の計測規約（交互 min-of-N・ノイズ帯併記・生データ保持）に従う |
| 候補 A（barrier-only）を非対応 adapter の縮退先へ昇格するか | #537 後 | 実測で subgroup 段の寄与が小さければ、全量 readback ではなく候補 A を縮退先にする案を再検討する |
| 候補 C（radix select） | 条件付き | k が Top-k 出力上限を超える、またはワークグループ数 × k の readback が予算を超える規模で再検討する |

## 不採用・条件付き（理由付き）

- **候補 C（radix select）を初期実装から除外**: compaction 順が非決定的
  になり、決定性契約（決定 4 の 2）を満たすには追加の CPU 側決定化が
  必要で実装コストが高い。k が小さい現行のバッチ検索用途では候補 B で
  十分なため、初期実装の対象からは外し条件付き候補として記録するに
  留める。
- **`enable subgroups;` の使用を不採用**: naga 30.0.1 が parse エラーを
  返すため、そもそも選択肢にならない（機械検証済みの制約）。
- **`SUBGROUP_BARRIER` の要求を不採用**: DX12 非対応のため、要求すると
  DX12 環境で GPU 経路自体が使えなくなる。決定 1 のネットワークは
  `subgroupBarrier` を使わない設計にすることで回避する。
- **k > Top-k 出力上限のケースでの部分 Top-k 適用を不採用**: 決定 2 の
  とおり全量 readback へ縮退する。ワークグループサイズを超える k への
  対応は候補 D／C の再検討事項として申し送る。

## 実装タスク対応表

決定 5 の表を参照。

## セキュリティ考慮（OWASP Top 10 + AGENTS.md P0）

- **インジェクション**: WGSL は `const` 文字列埋め込みのまま（外部ファイル・
  ユーザー入力からのシェーダ組み立てを行わない）。k・row_count・query_count
  は uniform 経由で渡し、シェーダ側でも上限クランプを行う（ホスト・
  シェーダの二重防御を Top-k 出力添字にも適用する）。
- **アクセス制御（テナント境界・P0）**: GPU 側で扱うのは可視行の row
  インデックスとスコアのみ。部分 Top-k 化後も `finalize_gpu_hits` の
  スロット解決＋可視性判定をすべての候補に通し、`revalidate_primary_hits`
  の独立再検証も維持する（唯一の防御線にしない）。統計カウンタ・
  エラー文字列にテナント名・行数・可視カーディナリティを含めない。
- **不安全な設計（fail-open 禁止・DoS）**: SUBGROUP 非対応・k 超過・
  パイプライン生成失敗・readback 長不一致・slot 範囲外はすべて全量
  readback またはバックエンドエラー（CPU 縮退）へ倒し、部分結果を返さ
  ない。readback バッファ・候補バッファは checked な確保を行い、
  `MAX_BATCH_WORK` 等の既存上限は不変のまま維持する。GPU 待機の締切も
  Top-k dispatch に適用する。
- **セキュリティ設定ミス（CORE-12）**: 本設計・再現用ツールとも環境変数・
  引数で GPU 経路や feature 要求を上書きする機構を設けない。
- **脆弱な依存**: 依存追加・更新なし（wgpu `=30.0.1` 固定のまま）。
  `enable subgroups;` 非対応など naga 30.0.1 の制約はソースを確認済みの
  事実として記録し、将来の wgpu 更新時の再検証項目として申し送る。
- **spec 漏えい（P0）**: 本 doc・関連コミット・PR 本文は TASK-128〜130・
  CORE-6／8／16 のポインタのみを用いる。faiss の参照は手法名・ファイル名・
  ライセンス（MIT）のみでコードは転記しない。
- **秘密情報**: 実測表にホスト名・パス・資格情報を含めない（GPU 名・
  ドライバ版のみを記録する）。

## スコープ外・申し送り

- Top-k シェーダ本体・経路判定純関数・統計カウンタの実装は #536 の担当。
  前後比較・readback バイト数の実測は #537 の担当。
- 非対応 adapter の縮退先を「全量 readback」から「barrier-only bitonic
  （候補 A）」へ昇格する案は、#537 の実測結果を見てから再検討する。
- 候補 C（radix select）は k が Top-k 出力上限を超える場合向けの条件付き
  候補として記録するに留める。
- `SHADER_F16`（#538）・整数ドット積系 feature（#541）・Apple UMA
  （#544）の実機確認は、本 doc の再現用 example（§6）の出力表を流用する。
- wgpu 更新時（naga が `enable subgroups;` を実装した場合）の WGSL
  互換性再確認を申し送る。

## 再現手順

`crates/engine/examples/gpu_adapter_info.rs`（`make gpu-adapter-info`）を
実行すると、本 doc の RTX 3060 実測表と同じ形式で adapter の
features／limits を出力する。`detect_features.rs`（Issue #468）と同じく
手動専用・CI 非配線の位置づけで、出力を本 doc へ手動転記する運用とする。
デバイスは生成せず adapter の情報のみを出力し、環境変数・引数による
上書きは行わない（CORE-12）。GPU 非搭載環境では adapter 未検出をエラー
として明示し非 0 終了する。

## 参照

- wgpu `=30.0.1`／naga `=30.0.1`（`Cargo.toml` 固定バージョン）: `Features`
  定義・adapter 側の `SUBGROUP` 判定ロジック・WGSL フロントエンドの
  enable-extension 処理・シェーダ validator の capability 判定・SPIR-V／
  HLSL／MSL backend の subgroup lowering
- faiss（MIT）: `gpu/utils/Select.cuh`・`WarpSelectKernel.cuh`・
  `BlockSelectKernel.cuh`（手法名・ファイル名のみの参照。コード非転記）
- WGSL 仕様の subgroups 拡張提案（`enable subgroups;` の仕様上の位置づけ）
- naga の `enable subgroups;` 未実装に関する上流の既知の追跡課題
