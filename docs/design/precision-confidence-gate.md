# `precision` モードの確信度ゲート: 設計判断記録

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-162（Issue #111。ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-161（取得モードの構文・優先順位解決。Issue #110 / PR #188 でマージ済み）・
  TASK-103（RRF 融合）・TASK-136（RLS 実行時安全網）
- 関連ビヘイビア: SEARCH-9（ポインタ: `docs/spec/04-behavior/search.md`）
- 実装: `crates/engine/src/precision.rs`（純粋関数群）・
  `crates/engine/src/sql/exec.rs`（適用位置の配線）・
  `crates/engine/src/core.rs`（`EngineCore::precision_policy` の保持）

## 目的

TASK-161 で構文・優先順位解決までが実装された `precision` モードに対し、実際の
実行契約（確信度判定・空集合 fail-closed 応答）を実装する。TASK-162 完了までの間、
`sql/exec.rs` は「`precision` の名の下に `recall` 相当の結果を返す」fail-open を
避けるため `22000` で一律拒否する暫定ゲートを置いていた（PR #188）。本 PR はこの
暫定ゲートを実際の確信度判定へ置き換える。

## 設計判断

### 確信度指標

`hits` のスコア尺度がランキング方式ごとに異なる（dense: 内積・非正規化／hybrid:
RRF 融合スコア）ため、いずれも「確信度 ∈ 概ね `[0, 1]` の共通尺度」へ写像してから
判定する。

| ランキング | 確信度の定義 |
| ---------- | ------------ |
| dense（`Ranking::Distance`） | クエリと候補 embedding の cosine 類似度（`f64` で再計算） |
| hybrid（`Ranking::Hybrid`） | RRF 融合スコア ÷ 理論最大値 `(dense_weight + sparse_weight) / (k_const + 1)` |

ノルム 0・次元不一致・非有限値は「確信なし」（`0.0` 相当）として空集合へ倒す
（`precision::cosine_similarity`／`precision::rrf_normalized` が `Option::None` を
返し、呼び出し元がこれを許容閾値未満の `0.0` として扱う。閾値は常に厳密に正のため
`0.0` は必ず未達になる）。

hybrid で密のみへ縮退した場合（本文列が全 `NULL` 等）、正規化 RRF スコアの理論最大値
は変わらないが実際に得られるのは単一検索器の寄与のみのため、最大でも理論値の半分
程度にしかならない。既定の hybrid 閾値（0.98）はこれを意図的に通過させない値として
選んでおり（詳細は下記「既定値（仮置き）」参照）、単一検索器の結果に確信を置かない
という設計判断を反映する。

### 判定規則（`precision::apply_gate`）

1. 確信度列が空 → `0` 件
2. Top-1 が非有限 → contract violation として `Err`（`XX000` へ写像。黙って通さない）
3. Top-1 が `min_top1` 未満 → `0` 件
4. Top-2 が存在し、Top-1 と Top-2 の差が `min_margin` 未満 → `0` 件（Top-2 が存在
   しない場合はマージン条件を「満たす」扱いとするが、規則 3 の絶対閾値は常に適用する）
5. それ以外 → `min(limit, max_results, 先頭から連続して min_top1 を満たす件数)`

順位そのものは変更しない。ゲートは候補生成（dense／hybrid）が確定した順位順の
確信度列をそのまま読むだけで、独自の再ソートは行わない。

### 適用位置（リランキング接続時の判断を含む）

`sql/exec.rs::execute_statement` の **DISTANCE 段（＋ `HINT ORDER` で先行する場合の
SCALAR 事後フィルタ）の後**・**`RlsSafetyNet::apply` の前**に適用する。

- SCALAR 事後フィルタの後: `WHERE` を満たす行だけを対象に Top-1／Top-2 を比較する。
  `HINT ORDER(DISTANCE, ...)` で DISTANCE が先行する構成では、SCALAR 条件を満たさない
  高スコア行がゲートの比較対象に混入しない。
- `RlsSafetyNet::apply` の前: 安全網は行を「減らす」ことしかできないため、ゲート
  通過後に安全網が行を落としても「確信のない行が増える」方向にはならず fail-closed が
  保たれる。候補集合自体は `ImplicitRlsHook` により事前フィルタ済みのため、他テナント
  の不可視行が Top-1／Top-2 の比較対象に混入することはない。
- `HINT ORDER` の内容に関係なく無条件に適用する（`is_precision` は `bound.mode` の
  みに依存し、`plan.scalar_prefilter` の分岐に触れない）。

**リランキング層（`rerank.rs`。SEARCH-7）接続時の判断**: 現時点で `rerank.rs` は
`sql/exec.rs` に未接続のため、候補生成スコア＝最終スコアである。将来リランキングを
`sql/exec.rs` へ接続する際も、ゲートの適用位置は変えない設計とする。すなわちゲートは
常に「実行経路の最終順位付けスコア」に対して判定し、リランキング後スコアから確信度を
計算する（候補生成スコアへは戻さない）。理由は 2 点:

1. `precision` の「ピンポイントで確信のある結果だけを返す」という意図に対し、
   ユーザー・クライアントから見える最終順位と確信度判定の基準が食い違うと、
   「なぜこの順位の行が返る／返らないか」を最終応答の順位から説明できなくなる。
2. リランキング層は候補プールの順位を変える機構であり、その変更を経ずに確信度を
   計算すると、リランキングが実質的に確信度判定を迂回できてしまう（fail-open の
   迂回経路になり得る。security.md「不安全な設計」）。

### fail-open 経路の不在

`precision::PrecisionPolicy` は `EngineCore` が保持するサーバー側専有の設定値。
`SessionState`・`BoundStatement`・SQL 構文のいずれにも対応するフィールド・句を
持たず、外部入力（クエリ・セッション変数）から到達する経路は構造的に存在しない
（`crates/engine/tests/sql_precision_mode.rs` の
`no_external_input_can_disable_or_relax_the_confidence_gate` が固定）。

加えて `ConfidenceThresholds::new`／`PrecisionPolicy::new` は閾値 0・負値・非有限値を
型レベルで拒否する（`0.0` は「常に通過」＝fail-open と等価になるため）。`Default`
実装自体がこの検証を通ることを単体テストで固定し、仮置き値の将来の改変が fail-open な
値へ漂流するのを防ぐ。

### 既定値（仮置き）

以下は TASK-163（評価基準の実測・目標値確定）までの**仮置き**である
（`crates/engine/src/precision.rs` の `DEFAULT_*` 定数）:

| パラメータ | 既定値 | 選定理由（仮置きの根拠） |
| ---------- | ------ | ------------------------ |
| dense `min_top1` | 0.80 | cosine 類似度の一般的な「関連あり」の目安 |
| dense `min_margin` | 0.05 | Top-1／Top-2 の分離を要求する下限 |
| hybrid `min_top1` | 0.98 | 「両リストとも 1 位」= 1.0、「両リストとも 2 位」≈ 0.984 の値域を踏まえ、単一検索器のみが 1 位に置いた候補（理論値の約半分）を通過させない |
| hybrid `min_margin` | 0.005 | 1 位・2 位が同点（マージン 0）の場合は空集合に倒す |
| `max_results` | 1 | 「ピンポイントで返す」という `precision` の設計思想に合わせた最小値 |

## 申し送り

- TASK-163: 上記仮置き値の実測・目標値確定。正解不在クエリを含む評価セット拡張
- TASK-164: `ModeSource::PlannerEstimate` 追加時もゲートは `ResolvedMode.mode` のみを
  見るため変更不要
- TASK-165: wire 経由・3 クライアントでの空集合応答受信確認（`wire-server` は現時点で
  `execute_sql*` を呼んでおらず本タスクでの変更なし）
- `EXPLAIN`（SQL-6・TASK-77）未実装のため、実効モード・確信度の可視化は当該タスクで
  扱う
- `rerank.rs` の `sql/exec.rs` への接続時は本 ADR「適用位置」節の判断（リランキング後
  スコアで判定）に従う
