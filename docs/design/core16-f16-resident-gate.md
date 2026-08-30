# CORE-16（f16 常駐 vs f32 常駐）ゲートの Apple GPU（Metal）fail 切り分け

- ステータス: Accepted（本コミットで規模スイープ診断〔opt-in〕を `batch_bench.rs`
  へ追加し、NVIDIA/Vulkan 環境での実測に基づき判断を記録。マージ後の Apple GPU
  実機・DGX Spark での追加実測は運用者作業として申し送り）
- 対応: Issue #313（`fix(engine): CORE-16 f16 常駐 vs f32 常駐ベンチが
  Apple GPU（Metal）で fail する原因の切り分け`）
- 前提: TASK-130（`docs/spec/05-tasks.md`・対象ビヘイビア CORE-16。
  `docs/spec/04-behavior/core-engine.md` ポインタ参照）、
  `docs/design/gpu-batch-wgpu-enablement.md`（GPU バックエンドの設計・
  CORE-6/CORE-16 の配線）、`docs/design/core7-dynamic-window-gate.md`
  （同種の測定設計切り分け ADR。本 ADR は体裁を踏襲する）

## 背景

承認済み計測環境（Apple M4 Max・Metal 経由 wgpu）で `BENCH_CORE6=1
BENCH_CORE16=1 make bench-batch` を実行した結果、CORE-6・CORE-7 は pass、
**CORE-16 のみ fail**（f16 パック常駐経路〔`GpuBatchBackend`〕が f32 常駐対照
経路〔`GpuF32ContrastBackend`〕より遅く、短縮率が `BENCH_CORE16_MIN_IMPROVEMENT_PCT`
を満たさない）という報告を受けた。本 ADR は、その原因を「環境要因」「測定設計」
「engine 側（シェーダ）の性能不足」のいずれかに切り分け、判断を記録する。
数値（実測 p95・短縮率）は Issue の方針（数値を書かない）に従い一切含めない
（`.claude/rules/spec-confidentiality.md` はオーナー判断で数値公開を許可済み
だが、本 Issue はより厳しい側の運用を明示的に求めているためそちらに従う）。

## 検討した仮説

| # | 仮説 | 根拠 |
| --- | --- | --- |
| H1 | Apple GPU（ユニファイドメモリ）では f16 パックによる帯域削減の利得が出ない（環境要因） | `dispatch_dot_products` の WGSL シェーダは f16 パック行を `unpack2x16float` 経由で読む。CPU/GPU が物理メモリを共有する UMA では帯域律速の前提が discrete GPU と異なりうる |
| H2 | 測定区間が dispatch の固定コスト（バッファ生成・`write_buffer`・`submit`・`map_async`+poll 待機）に支配され、シェーダ差分が p95 に現れない（測定設計要因） | CORE-16 ゲート本体は 1 クエリ単位で dispatch する経路であり、Issue #302（CORE-7）と同型の「測っているものが違う」可能性がある |
| H3 | f16 シェーダ側の非効率（naga の Metal 向け変換・`unpack2x16float`・クエリ複数回ロード等）による engine 側の性能不足 | f16 版はループ 1 反復で追加のロード・unpack 命令を持つため、帯域が利かない環境では相対的に不利になりうる |
| H4 | 実行順・サーマル等のノイズ | `harness::ab::run_ab` は interleaved 実行のため低確率。ただし後述「測定安定性」節で別種のノイズを確認した |

## 本開発環境（NVIDIA GeForce RTX 3060・Vulkan backend）での実測

Apple 実機・DGX Spark はエージェントからアクセス不能なため、本開発環境
（discrete GPU・Vulkan backend）を「別環境」の実測として使った
（`docs/design/gpu-batch-wgpu-enablement.md` §3 のローカル実測と同一構成）。

### CORE-16 ゲート本体（既定規模）

`BENCH_CORE6=1 BENCH_CORE16=1` の opt-in ゲートを実行し、GPU 初期化・計測とも
正常に完走したうえで **pass** した。f16 パック常駐経路が f32 常駐対照経路より
明確に高速だった（f16 側が優位。具体値は書かない）。

### 規模スイープ（本コミットで追加した opt-in 診断 `run_core16_scaling_diagnostic`）

行数を最小規模（ワークグループ数個分）から既定規模の数倍まで、次元を既定値の
数分の一から数倍まで振って測定した結果:

- 測定したすべての規模点で f16 パック常駐経路が f32 常駐対照経路と同等以上に
  高速だった（f16 が明確に劣後する規模点は確認できなかった）。
- 短縮率は一般に **データ量（行数 × 次元）が大きいほど拡大する傾向**を示した。
  最小規模（データ量が小さく dispatch 固定コストの比重が相対的に大きい条件）
  でも f16 が劣後することはなく、短縮率が縮小するにとどまった。
- 次元を既定値より小さくした点では短縮率が明確に縮小した（帯域削減の効果が
  ペイロードサイズに依存することと整合する）。

### 測定安定性に関する追加知見（H2 に隣接する発見）

診断ロジックを CORE-7/CORE-6/CORE-16 ゲート本体と**同一プロセス内で連続実行**
すると、単独実行時より測定値のばらつきが明確に増加し、一部の中間規模点で
優劣が入れ替わる（f16 が劣後して見える）ケースを観測した。単独・低負荷の
実行では観測されなかった現象であり、GPU バックエンドの繰り返し構築・破棄や
先行するベンチジョブの GPU 占有状態が後続の測定へ持ち越される、プロセス内
連続実行特有のノイズが存在することを示唆する。この知見は Metal 環境の fail
報告そのものを説明するものではないが、CORE-16 単独の判定不能・不安定性の
一因として無視できないため「申し送り」節で後続調査候補として記録する。

## 判断

- **H3（engine 側シェーダの性能不足）は本環境の実測で積極的に否定される**:
  同一のシェーダ・同一の dispatch ロジックが、最小規模から既定規模の数倍まで
  一貫して f16 優位（同等以上）を示した。シェーダ自体が構造的に遅いのであれば
  discrete GPU でも規模を問わず劣後するはずだが、そのような結果は得られな
  かった。
- **H2（測定設計・固定コスト支配）は主要因ではないと判断する**: 最小規模点
  でも f16 が f32 に対して劣後することはなく、固定コストが差分を完全に打ち
  消すには至っていない（短縮率が縮小するだけで符号は反転しない）。ただし
  「測定安定性に関する追加知見」で観測した連続実行時のノイズ増加は、単独の
  CORE-16 判定不能・不安定性を招きうる副次的な測定設計要因として残る。
- **H1（Apple GPU/UMA 環境要因）が Metal 固有の fail を最も自然に説明する**:
  本環境（discrete GPU・専用 VRAM・Vulkan backend）では f16 パックの帯域削減
  効果が一貫して観測され、データ量に応じて拡大する傾向まで確認できた。この
  結果が UMA 環境（GPU/CPU が物理メモリを共有し、discrete GPU とは異なる
  帯域特性を持つ）で同様に成立する保証はなく、Metal 側固有の要因
  （UMA の帯域モデル・naga の Metal 向け `unpack2x16float` 変換・Metal
  ドライバ/wgpu backend 固有のオーバーヘッド）のいずれか、またはその組み合わせ
  が dominant であると推定する。ただし Apple 実機での直接測定は本 Issue の
  スコープ外（エージェントからアクセス不能）のため、これは実測に基づく
  **推定**であり確定ではない。

## 環境別 pass/fail 表

| 環境 | opt-in 実行 | 結果 |
| --- | --- | --- |
| Apple M4 Max（Metal 経由 wgpu・承認済み計測環境） | 実施済み（Issue #313 報告） | fail（CORE-6・CORE-7 は pass） |
| 本開発環境（NVIDIA GeForce RTX 3060・Vulkan backend） | 実施済み（本 ADR） | pass（既定規模・規模スイープとも f16 が一貫して優位または同等） |
| DGX Spark 等（NVIDIA・Vulkan／別ハードウェア） | 未実施 | 運用者追記欄: `_______________`（Actions 外の承認済み計測環境で `BENCH_CORE6=1 BENCH_CORE16=1 make bench-batch` を実行し pass/fail のみ追記） |

## 使い方（規模スイープ診断）

```sh
BENCH_CORE16_DIAG=1 BENCH_VERBOSE=1 make bench-batch
```

- `BENCH_CORE16_DIAG` 単独（`BENCH_VERBOSE` 未設定）では、`BENCH_VERBOSE` の
  設定を促す 1 行のみを出力し診断は実行しない。
- `BENCH_CORE16_DIAG` 未設定時は診断コード自体が一切出力しない（既定挙動は
  無変更）。
- `GITHUB_ACTIONS` 下では既存の `verbose_requested_from_env` の fail-closed
  拒否（Issue #279）にそのまま乗るため、本診断専用の追加ガードは設けていない。
  `bench.yml` には `BENCH_CORE16_DIAG` を注入しない運用とする。
- 診断は CORE-6/CORE-16 ゲート本体とは独立の合成データセットを規模点ごとに
  構築するため、合否には数えない参考出力である。

## 検討したが採らなかった案

- **CORE-16 の閾値を Metal 環境向けに緩和する**: 数値基準の変更は spec 側の
  判断領域であり、本 Issue（engine 側原因調査）のスコープ外。
- **`gpu_batch.rs` のシェーダを本 Issue で変更する**: 実測から H3 は否定的
  だったため、production コードの変更根拠が本 Issue の範囲では得られな
  かった。Metal 実機での再検証結果次第では別 Issue で再検討しうる。

## 申し送り（別 Issue 候補。本 Issue はここでは起票せずユーザー判断へ委ねる）

- **Apple 実機・DGX Spark での実測**: 本 ADR の「環境別 pass/fail 表」に運用者
  追記欄を残した。承認済み計測環境で `make bench-batch` を実行し結果を追記
  する運用者作業。
- **プロセス内連続 GPU 測定のノイズ低減**（「測定安定性に関する追加知見」）:
  GPU バックエンドの構築・破棄コスト分離、または `wgpu::Features::
  TIMESTAMP_QUERY` によるカーネル時間の直接測定（初期化契約の変更を伴うため
  別 Issue）。CORE-7（Issue #302）で確立した「測定区間外での確保・
  戻り値退避」パターンを GPU 経路へも横展開できないか含め検討。
- **Metal 固有の対処**: H1 が実機で確認された場合、環境別に CORE-16 の判定を
  「対象外」とする運用ルールを spec 側へ提案するかはオーナー判断（spec リポ
  側の課題としてユーザーへ報告する）。
- **naga の Metal backend 変換差分の調査**: `unpack2x16float` を含む WGSL が
  Metal Shading Language へどう変換されるかを wgpu/naga のソースレベルで
  確認し、UMA 帯域モデルとの相互作用を definitively に切り分ける調査
  （実機なしでは限定的にしか進められない）。
