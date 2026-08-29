# CORE-7 動的窓ゲート（`bench-batch`）の測定設計是正

- ステータス: Accepted（本コミットで `batch_bench.rs` の CORE-7 ゲート・診断を
  実装。マージ後のリポジトリ管理者作業〔受け入れ条件 3〕は下記「申し送り」参照）
- 対応: Issue #302（`fix(engine): bench-batch の dynamic_window_degradation 失敗の切り分け`）。
  親: #300（閾値ゲート初回 strict 評価の後続対応）
- 前提: TASK-130（`docs/spec/05-tasks.md`・対象ビヘイビア CORE-7。
  `docs/spec/04-behavior/core-engine.md` ポインタ参照）、
  `docs/design/ci-gate-variables.md`（`BENCH_BATCH_MAX_DEGRADATION_PCT` の secrets 注入）

## 背景

`.github/workflows/bench.yml` の `bench-batch` job（CORE-7 ゲート
`dynamic_window_degradation`）が、閾値 secret 設定後の `workflow_dispatch` 2 回で
いずれも `pass=false` になった。本 ADR は、その原因切り分けと測定方式の是正を
記録する。数値（閾値・実測値）は一切含めない
（`.claude/rules/spec-confidentiality.md`）。

## 原因調査

ローカル／隔離環境での再現実行と独立プローブにより、以下を切り分けた。

1. **ノイズではない**: 複数回のローカル再現で被検側（動的窓集約経由）の p95 が
   対照側（検証なしの `Vec::push`）より系統的に大きく、失敗は再現性のある構造的な
   ものだった。
2. **engine の実退行ではない**: 集約器実装（`engine::batch_search::
   DynamicWindowAggregator`）に直近の関連変更はなく、独立プローブでも
   push/drain 自体のコストは対照と同オーダーであり、有意な退行ではなかった
   （実測の時間スケールは書かない——`.claude/rules/spec-confidentiality.md`）。
3. **根本原因はベンチ側の測定設計**:
   - 旧実装は `harness::ab::run_ab` の戻り値（`Vec<Vec<f32>>`。256 本 ×
     dim 768 規模）を測定区間の内側で drop していた。`run_ab` はワークロードの
     戻り値を `black_box` 経由で測定区間内に drop する契約（`harness/ab.rs`・
     `harness/protocol.rs` に本コミットで明記）であり、解放コストが push/drain
     本体の差分に対して支配的だった。
   - 加えて、対照側・被検側のクエリプールを「対照側を全確保 → 被検側を全確保」
     の順で構築していたため、両者の heap 配置が非対称になり、glibc の `free` 挙動
     （top chunk への併合・trim 誘発）が経路間で異なるコストを生んでいた。
   - この 2 点により、実際には測定しているのが集約器のオーバーヘッドではなく
     アロケータの挙動になっていた。
   - さらに、drop コストを外して push/drain 本体だけを比較しても、対照側が
     「検証なしの `Vec::push`」という最小限の操作である以上、検証・amortized
     成長を持つどんな集約器実装も百分率上限を安定して満たせない。CORE-7 が
     定める量（動的窓構成における**単発クエリ経路**の p95 劣化）と、旧来の
     「256 本まとめて push/drain するだけ」の比較は測定対象がそもそも異なって
     いた。

## 設計方針

### CORE-7 ゲート本体（`batch_bench.rs::run_core7_gate`）

- A（対照）・B（被検）とも **同一カーネル**（`engine::batch_search::
  BatchEngine::batch_search`。f16 常駐・CPU-SIMD）を経由させ、CORE-6/CORE-16 と
  同じ合成データセット（`build_gate_dataset`）を再利用する。
- A は事前生成済みクエリ 1 本を直接 `batch_search` へ渡す経路、B は同じ形状の
  クエリを `DynamicWindowAggregator::push`/`drain` に通してから同じ
  `batch_search` を呼ぶ経路とし、差分を「窓の push/drain・所有権移動・dispatch
  相当の分岐」だけに絞る。単発クエリは実運用では動的窓に入らない
  （`should_aggregate_into_batch` が `false` を返す文脈）ため、B は「窓を通った
  場合に単発クエリが払いうる最大オーバーヘッド」を課す**保守側**の構成である。
- 複数試行（`CORE7_TRIALS`）を行う。試行内の劣化率（%）は CORE-7 が定義する量
  そのまま、**経路ごとに独立算出した p95 の差分**
  （`degradation_pct(p95_from_samples(a), p95_from_samples(b))`）で算出し、
  試行間はその値の列の**中央値**を閾値と比較する（`median_degradation_pct`）。
  単一試行だけでは hosted runner 上の突発的な計測スパイクが 1 回でも起きると
  誤 fail しうるため、試行間の中央値採用でその影響を緩和する（本コミットの
  ローカル検証でも、複数試行中 1 試行だけが大きく外れる事象を実際に観測して
  おり、試行間中央値がその対策として機能することを確認した）。
  - **ペア化差分方式の撤回（PR #305 codex-review 指摘。詳細は下記「追補」節）**:
    試行内統計量には過去 2 回のレビュー対応で「反復ペアの絶対差分
    （`b_i - a_i`）の分布から p95 を取り対照側 p95 で正規化する」ペア化差分
    方式（`paired_p95_degradation_pct`）を採用していたが、この方式は A/B が
    完全に同一分布でも構造的に正の値へ偏る欠陥があり撤回した。現在の
    実装（経路別独立 p95 差分）に一本化している。
- 解放コストの測定区間外化: 各ワークロードの戻り値（B の `drain()` 結果等）は
  試行内の sink へ退避し、`run_ab` 完了後にまとめて drop する
  （`harness::ab::run_ab`・`harness::protocol::run` のドキュメンテーション
  コメントに明記した「戻り値の drop は測定区間内」契約への対応）。
- pool の対称化・クエリ内容の一致: 対照用・被検用のクエリ確保を「全確保 →
  全確保」の順にせず 1 つのループで交互に確保し、かつ反復ごとに 1 本だけ
  生成した内容を両者へ複製する。A/B が異なる内容のクエリを使うと
  `batch_search` の類似度計算・上位 k 候補更新コストがクエリ値へ依存する
  ぶんが測定対象（push/drain のオーバーヘッド）より大きいノイズ・系統差に
  なりうるため（レビュー対応）。
- 判別力の限界（希釈）: A（対照）・B（被検）が共有する支配的なコスト（全走査）
  は、経路ごとに独立算出した p95 の差分では反復間ノイズが両者へ別々に乗って
  測定対象（push/drain の差分）を希釈しうる。一時期はこの希釈対策として
  「反復ペアの絶対差分（秒）を集めてその分布の p95 を取る」ペア化差分方式を
  採ったが、PR #305 codex-review 指摘で撤回した（詳細は下記「追補」節）。
  現在は判別力より統計的健全性（構造的偽陽性を持たないこと）を優先し、
  希釈への対処は試行数・反復数を増やして分位点推定のノイズを下げる方向へ
  委ねる（下記「本ゲートの感度の限界」節）。
- `batch_search` が 1 件でもエラーを返した場合は判定不能として `pass=false`
  （CORE-6/CORE-16 と同一の fail-closed 方針。エラー経路は通常大幅に軽量なため、
  計測サンプルへ計上すると誤って劣化なしと判定しうる）。

### 診断への降格（`batch_bench.rs::run_dynamic_window_push_drain_diagnostic`）

旧来の「256 本まとめて push/drain するだけ」の比較は、集約器実装そのものの
退行を可視化する判別力の高い参考値として、**合否に数えない診断**として残す
（`simd_bench.rs::diagnostic_ab` と同型）。`BatchEngine::batch_search` を経由
しないため CORE-7 の定義量そのものではないが、集約器単体の実装変更を素早く
検知する用途では引き続き有用と判断した。診断側にも「解放コストの測定区間
外化」「pool の対称化」を適用する（そのままだと診断値も heap 配置効果を
映してしまうため）。

## PR #154 の判別力優先レビュー対応との関係

旧実装（本コミットで置き換えた `main` 内の直接比較）は PR #154 のレビュー対応で
確立されたものだった。当時の設計は「A/B 双方が同一の `BatchEngine::batch_search`
（全走査）を測定区間へ含めると、全走査の支配的なコストの前で push/drain の
小さな差分が埋もれ、実質どんな劣化を注入しても pass してしまう判別力のない
ゲートになる」という指摘への対応であり、`batch_search` を測定区間から除外
することで判別力を確保していた。

本コミットは、CORE-7 の定義量（動的窓構成における単発クエリ経路の p95 劣化）を
測るには `batch_search` を経由する必要があるという理由から、その除外を**ゲート
本体については取り消す**。判別力優先の判断そのものが誤りだったのではなく、
「判別力の高い比較」と「CORE-7 が定義する量」が両立しない設計だったため、
前者を `run_dynamic_window_push_drain_diagnostic` として合否に数えない診断へ
切り出し、両方を別々に保持する形で解決した（アサーション弱体化ではなく、
ゲート本体の測定対象を定義量へ合わせ直したうえで判別力は診断側に温存する構成）。

## 本ゲートの感度の限界

B（被検）は A（対照）に窓の push/drain・所有権移動を上乗せしただけの経路であり、
理論上 B が A より速くなることはない。しかし実測では負の劣化率（B が A より
速く見える試行）が観測されることがある。これは push/drain 自体のコストが、
全走査を含む 1 反復全体の計測ノイズ（キャッシュ・スケジューラ・周波数遷移等）
に対して非常に小さいためである。**本ゲートは全走査コストに対して意味のある
規模の劣化（実装の重大な退行）を安定して検出する**という設計であり、
push/drain 自体のコストが計測ノイズの床を下回るごく軽微な退行までは
（統計的に健全な範囲では）検出しない。この限界は試行数・反復数を増やし
分位点推定のノイズを下げることで緩和する対象であり、ペア化のような
推定量そのものの変更では埋めない（下記「追補」節）。より軽微な退行の検知は
`run_dynamic_window_push_drain_diagnostic`（合否に数えない診断。
`batch_search` を経由せず push/drain 自体を直接比較するため感度が高い）と
`tests/batch_accept.rs` の単体テストが引き続きその役割を担う。

- **検出力検証（Issue #302 codex-review 指摘対応）**: 「感度の高い比較を合否
  判定に残すか、既知の退行を注入して新ゲートが確実に失敗する検出力検証を
  追加せよ」という指摘に対し、後者を選んだ。`run_core7_gate` と同型の統計
  パイプライン（複数試行 → 試行内 `degradation_pct(p95_from_samples(a),
  p95_from_samples(b))` → 試行間 `median_degradation_pct` →
  `check_degradation_pct_within_limit`）を実測タイマーなしの合成サンプルへ
  適用し、「反復ごと一定のオーバーヘッドを注入した push/drain 退行」を与えると
  本ゲートの判定パイプラインが確実に `pass=false` を返すこと、かつノイズのみ
  （実退行なし）では `pass=true` を維持することを `tests/batch_accept.rs::
  core7_gate_pipeline_fails_when_a_push_drain_regression_is_injected` として
  固定した（`make ci` 対象。合成値のみを使い spec 実測値・閾値は書かない）。

## 追補: ペア化差分方式の撤回（PR #305 codex-review 指摘）

上記「設計方針」節・「本ゲートの感度の限界」節に記した `paired_p95_degradation_pct`
（反復ペアの絶対差分 `b_i - a_i`（秒）の分布から p95 を取り、対照側 p95
レイテンシで 1 回だけ正規化する方式）は、Issue #302 の 2 回のレビュー対応
（Cursor Bugbot・codex-review）を経て採用したが、その後の PR #305 codex-review
指摘により統計的に成立しないことが判明し撤回した。現在の実装は本追補が
定める方式（経路別独立 p95 差分）に一本化している。

- **指摘の核心**: `run_ab`（`harness::ab::run_ab`）は同一反復番号の `a_i`/
  `b_i` を直後に連続実行するだけであり、厳密な同時計測ではない。したがって
  A/B が完全に同一分布（実退行なし）であっても `delta_i = b_i - a_i` は
  平均 0・分散非 0 の分布になる。その**分布の p95**（0 を中心とする対称分布の
  上側裾）は、A/B の分布が一致していようといまいと構造的に正の値を取り
  続ける（対称分布の上側 5 パーセンタイルは定義上ほぼ常に正）。この構造的
  バイアスは「ペア化によるノイズ低減」という設計意図とは無関係に生じるため、
  ペア化差分の p95 は CORE-7 が定義する量（B の p95 と A の p95 の差。負にも
  なりうる対称な量）と一致しない。
- **旧実装の合成テストが検証できていなかった理由**: 撤回前の
  `core7_gate_pipeline_fails_when_a_push_drain_regression_is_injected`
  （無退行ケース `injected_overhead_ns=0`）は `JITTER_NS=1ms`（対照側基準
  50ms に対し小さい値）を使っていたため、注入ノイズによる delta 分布の p95
  が `TEST_MAX_PCT` を偶然下回っていただけで、構造的バイアスの有無を検証
  していなかった（三角分布 `delta ~ Triangular(-J, J)` の p95 は概ね
  `0.684*J` であり、`J` を大きくすれば同一分布でも容易に閾値を超える）。
- **是正**: 試行内の統計量を CORE-7 の定義そのまま
  `degradation_pct(p95_from_samples(a), p95_from_samples(b))`（経路別に
  独立算出した p95 の差分。A が速くも遅くもなりうる対称な統計量で、上記の
  構造的バイアスを持たない）へ戻した。`tests/batch_accept.rs::
  core7_gate_pipeline_does_not_false_positive_under_identical_ab_distributions`
  で、(1) ノイズ幅を意図的に大きく取った場合に撤回済みのペア化差分方式が
  実際に偽陽性を起こすこと、(2) 同じ合成サンプルで新方式（経路別独立 p95
  差分）は偽陽性を起こさないことの両方を固定した（`make ci` 対象）。
- **判別力とのトレードオフ**: ペア化を検討した動機（軽微な push/drain 退行が
  全走査コストの反復間ノイズへ埋もれ判別力を失う）自体は解消していない。
  この弱点は推定量をペア化差分へ戻すのではなく、試行数（`CORE7_TRIALS`）・
  反復数（`run_core7_gate` が `MeasurementConfig::new` へ渡す測定反復回数）を
  増やして分位点推定のノイズを下げることで緩和する対象とする（詳細な軽微
  退行の検知は引き続き `run_dynamic_window_push_drain_diagnostic` が担う）。

## 検討したが採らなかった案

- **旧来の micro ゲートを維持し spec 側で閾値を見直す**: 対照が検証なしの
  最小限の操作（`Vec::push`）である限り、百分率上限をどこに置いても検証
  コストを持つ実装は安定して満たせない。CORE-7 の定義量（単発クエリ p95）
  とも一致しない。
- **`DynamicWindowAggregator` の最適化で被検側を対照側に寄せる**: 検証コストが
  残る以上、対照との比率は根本的には改善しきれず、hosted runner でのノイズ
  耐性も得られない。本 Issue のスコープ外（engine 本体は変更しない）。
- **`bench.yml` の変更**: 不要。job 定義・環境変数・実測値非出力の運用方針は
  変更していない。

## 実測値・閾値の非出力方針（維持）

既定出力は pass/fail と非数値状態のみ。実測値（試行別劣化率・中央値等）は
`BENCH_VERBOSE` opt-in 時のみ追加出力し、`GITHUB_ACTIONS` 下では
`BENCH_VERBOSE` 自体を fail-closed で拒否する（Issue #279 の既定方針を維持。
本 ADR にも数値を書かない）。

## 申し送り

- **受け入れ条件 3（連続 3 回 green）**: Environment `bench-gate` は main 限定の
  ため、マージ後にリポジトリ管理者が `gh workflow run bench.yml --ref main` を
  3 回連続で実行して確認する（`README.md` 参照）。
- **`bench-c1` job の別失敗**: 同じ失敗 run で `p95_latency(sql_c1)` も
  `pass=false` だったが、`workflow_dispatch` 限定・専有環境前提の基準であり
  本 Issue の対象外。ユーザーへ別途報告済み（起票要否はユーザー判断待ち）。
- **Actions ログのマスクによる secret 値の推定経路**: 失敗 run のログで、bench
  出力中の固定文字列（ID・数値等）の一部が secret の値と部分一致してマスク
  された痕跡があり、マスクされた位置から間接的に値を推定できる余地がある。
  対策候補の検討は spec-confidentiality に関わる別 Issue としてユーザーへ
  報告する（本 ADR・PR には推定値を書かない）。
- **wire 層経由の再測定**: 引き続き未実施（既存の申し送りのまま）。
