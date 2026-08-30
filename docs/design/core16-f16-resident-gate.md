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

### 規模スイープ（2 通りの測定方法を区別して記録する）

規模点（行数・次元の組。最小規模〔ワークグループ数個分〕から既定規模の数倍
まで、次元は既定値の数分の一から数倍まで）を振った実測は、測定方法によって
**結果が一致しない**ことを確認した。以下の 2 方法は独立した知見として区別する
（どちらか一方だけを信頼できる結論として扱わない）。

**方法 A（1 規模点 = 1 プロセス。定数を差し替えて規模点ごとに個別実行）**:
測定したすべての規模点で f16 パック常駐経路が f32 常駐対照経路と同等以上に
高速だった（f16 が明確に劣後する規模点は確認できなかった）。短縮率は一般に
**データ量（行数 × 次元）が大きいほど拡大する傾向**を示し、次元を既定値より
小さくした点では短縮率が明確に縮小した（帯域削減の効果がペイロードサイズに
依存することと整合する）。最小規模（dispatch 固定コストの比重が相対的に
大きい条件）でも f16 が劣後することはなく、短縮率が縮小するにとどまった。

**方法 B（本コミットで追加した opt-in 診断 `run_core16_scaling_diagnostic`。
CORE-7/CORE-6/CORE-16 ゲート本体の実行後、同一プロセス内で規模点を連続測定）**:
方法 A と一致しない結果が出た。既定規模を含む一部の中間規模点で f16 が
f32 と同等〜劣後する結果が観測され、既定規模点の絶対値自体も同一プロセス
内でゲート本体が測定した値から大きく外れていた（同一規模点でも呼び出す
タイミングによって値が変わる＝**この逐次診断が返す値・符号は方法 A の
結論を検証する証拠として使えない**）。診断の実行順（GPU バックエンドの
繰り返し構築・破棄、先行するゲートジョブの GPU 占有状態）が後続の測定へ
持ち越されるプロセス内連続実行特有のノイズが疑われる。

この不一致自体が本 Issue にとって重要な発見であるため、「判断」節・
「申し送り」節でそれぞれ扱いを分けて記録する。

## 判断

- **H3（engine 側シェーダの性能不足）は方法 A（クリーンな単独計測）の範囲で
  否定される**: 同一のシェーダ・同一の dispatch ロジックが、方法 A では
  最小規模から既定規模の数倍まで一貫して f16 優位（同等以上）を示した。
  シェーダ自体が構造的に遅いのであれば discrete GPU でも規模を問わず劣後
  するはずだが、クリーンな計測ではそのような結果は得られなかった。この
  判定は方法 A の測定条件に限定される（下記 H2-逐次 参照）。
- **H2 は 2 つの下位仮説に分けて評価する**:
  - **H2-固定コスト**（測定区間が dispatch の固定コストに支配される）は
    主要因ではないと判断する。方法 A の最小規模点でも f16 が f32 に対して
    劣後することはなく、固定コストが差分を完全に打ち消すには至っていない
    （短縮率が縮小するだけで符号は反転しない）。
  - **H2-逐次測定ノイズ**（同一プロセス内で GPU バックエンドを繰り返し
    構築・破棄しながら連続測定すると値・符号が不安定になる）は、方法 B の
    実測で**確認された**。この現象は「CORE-6 → CORE-7 → CORE-16 ゲート
    本体 → 追加測定」という**ゲートが後続で測るほど不利になりうる順序**
    を持つ点で、Metal 環境の報告（CORE-6・CORE-7 は pass、CORE-16 のみ
    fail）の構造と一致する。H1 と並ぶ**共同主因候補**として扱う。
  - なお本コミットで追加した `run_core16_scaling_diagnostic` 自体が方法 B
    （逐次測定）の実装であるため、**診断が返す実測値・符号は方法 A の
    結論を裏付ける証拠として使えない**（「使い方」節・申し送り参照）。
- **H1（Apple GPU/UMA 環境要因）は Metal 固有の fail を説明しうる仮説として
  残るが、本環境の実測では検証も反証もできない**: 本環境（discrete GPU・
  専用 VRAM・Vulkan backend）の方法 A では f16 パックの帯域削減効果が
  一貫して観測され、データ量に応じて拡大する傾向まで確認できた。しかし
  discrete GPU での結果が UMA 環境で同様に成立する保証はなく、Apple 実機
  での直接測定は本 Issue のスコープ外（エージェントからアクセス不能）の
  ため、H1 は依然として**未検証の仮説**である。

## 環境別 pass/fail 表

| 環境 | opt-in 実行 | 結果 |
| --- | --- | --- |
| Apple M4 Max（Metal 経由 wgpu・承認済み計測環境） | 実施済み（Issue #313 報告） | fail（CORE-6・CORE-7 は pass） |
| 本開発環境（NVIDIA GeForce RTX 3060・Vulkan backend） | 実施済み（本 ADR） | pass（CORE-16 ゲート本体。方法 A の規模スイープでも f16 が一貫して優位または同等。ただし方法 B〔本コミットの逐次診断〕では既定規模を含む一部規模点で優劣が反転し不安定——上記「判断」参照） |
| DGX Spark 等（NVIDIA・Vulkan／別ハードウェア） | 未実施 | 運用者追記欄: `_______________`（Actions 外の承認済み計測環境で `BENCH_CORE6=1 BENCH_CORE16=1 make bench-batch` を実行し pass/fail のみ追記） |

## 使い方（規模点診断）

```sh
BENCH_BATCH_MAX_DEGRADATION_PCT=<値> BENCH_CORE16_DIAG=1 \
  BENCH_CORE16_DIAG_SCALE_INDEX=<0..5> BENCH_VERBOSE=1 make bench-batch
```

- `BENCH_BATCH_MAX_DEGRADATION_PCT` は `batch_bench.rs::main` 冒頭で CORE-7
  ゲート用に無条件で要求される（未設定は fail-closed で診断に到達する前に
  終了する）。診断のみが目的の実行でも設定が必要。
- `BENCH_CORE16_DIAG` 単独（`BENCH_VERBOSE` 未設定）では、`BENCH_VERBOSE` の
  設定を促す 1 行のみを出力し診断は実行しない。
- `BENCH_CORE16_DIAG` 未設定時は診断コード自体が一切出力しない（既定挙動は
  無変更）。
- `GITHUB_ACTIONS` 下では既存の `verbose_requested_from_env` の fail-closed
  拒否（Issue #279）にそのまま乗るため、本診断専用の追加ガードは設けていない。
  `bench.yml` には `BENCH_CORE16_DIAG` を注入しない運用とする。
- 診断は CORE-6/CORE-16 ゲート本体とは独立の合成データセットを、選択した
  1 規模点についてのみ構築するため、合否には数えない参考出力である。
- **1 プロセス = 1 規模点（本 ADR の「判断」節参照。PR #326 codex-review
  指摘対応）**: 本診断は当初、複数規模点を同一プロセス内で逐次測定していた
  （方法 B）。この方式は GPU バックエンドの繰り返し構築・破棄に由来すると
  疑われる測定ノイズにより値・符号が不安定になることを本 ADR で確認して
  おり、方法 A（クリーンな単独計測）の結論を裏付ける証拠として使えない
  ことが判明したため、`BENCH_CORE16_DIAG_SCALE_INDEX`（`CORE16_DIAG_SCALE_POINTS`
  への添字。`0`〜`5`。未設定・範囲外は fail-closed）で 1 回の実行につき
  1 規模点だけを測定する形へ変更した（方法 A と同型の測定形）。複数規模点間
  の傾向を見たい場合は、`BENCH_CORE16_DIAG_SCALE_INDEX` を変えてプロセスを
  分けて複数回実行すること（同一プロセス内での連続測定はしない）。

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
- **規模点診断を「1 規模点 = 1 プロセス」で実行できる形へ改める**
  （「判断」節の H2-逐次測定ノイズ）: PR #326（codex-review 指摘対応）で
  対応済み。`run_core16_scaling_diagnostic` は `BENCH_CORE16_DIAG_SCALE_INDEX`
  で選んだ 1 規模点のみを測定する形へ変更し、複数規模点を同一プロセス内で
  逐次測定する経路は撤去した。規模点間の傾向確認は環境変数を変えてプロセス
  を分けて複数回実行する運用とする（「使い方」節参照）。
- **CORE-16 ゲート本体が「CORE-6 → CORE-7 → CORE-16」の順で後続に測定
  される構成そのものの妥当性検証**: Metal 環境の fail 報告
  （CORE-6・CORE-7 は pass、CORE-16 のみ fail）が本 ADR で確認した
  H2-逐次測定ノイズと同型の順序依存性で説明できるかを、独立プロセスで
  CORE-16 単体を測定した場合との比較で検証する。
- **プロセス内連続 GPU 測定のノイズ低減**（一般対応）: GPU バックエンドの
  構築・破棄コスト分離、または `wgpu::Features::TIMESTAMP_QUERY` による
  カーネル時間の直接測定（初期化契約の変更を伴うため別 Issue）。CORE-7
  （Issue #302）で確立した「測定区間外での確保・戻り値退避」パターンを
  GPU 経路へも横展開できないか含め検討。
- **Metal 固有の対処**: H1 が実機で確認された場合、環境別に CORE-16 の判定を
  「対象外」とする運用ルールを spec 側へ提案するかはオーナー判断（spec リポ
  側の課題としてユーザーへ報告する）。
- **naga の Metal backend 変換差分の調査**: `unpack2x16float` を含む WGSL が
  Metal Shading Language へどう変換されるかを wgpu/naga のソースレベルで
  確認し、UMA 帯域モデルとの相互作用を definitively に切り分ける調査
  （実機なしでは限定的にしか進められない）。
