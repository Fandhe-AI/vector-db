# ADR: USING PLAN 世代照合の拒否粒度（テーブル単位・全テナント共通）の維持（Issue #285）

- ステータス: Accepted
- 対応: Issue #285（親 Issue #275「レビュー申し送りのコード品質・設計判断」の子。
  PR #266・TASK-77 の申し送り事項）
- 関連ポインタ: `docs/spec/05-tasks.md`（TASK-77・TASK-110・TASK-137）・
  `docs/spec/04-behavior/sql-surface.md`（SQL-5）・`docs/spec/04-behavior/rls.md`
  （RLS-6・RLS-7・RLS-9）・`docs/spec/04-behavior/data-model.md`（TABLE-12）。
  spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 検証コード: `crates/engine/src/core.rs`（`#[cfg(test)] mod tests` 内、下記「今回
  固定したテスト」参照）

## 概要

`USING PLAN`（TASK-77）は LLM I/O（辞書スナップショット構築 → LLM 展開 → 再埋め込み）
の前後でテーブル単位の世代カウンタ（`crates/engine/src/catalog.rs` の
`TABLE_GENERATION_TABLE`）を照合し、不一致なら fail-closed に拒否する。この世代は
**テーブル単位・全テナント共通** であり、他テナントの書き込み（要求元には不可視な
`Private` 行の書き込みを含む）でも拒否される。本 ADR はこの粒度を現状維持とし、
併せて将来細分化する際に壊してはならない fail-closed 契約を判別テストとして固定する。
production コードの挙動変更は行っていない。

## 現状の機構

| # | 内容 | コード参照 |
| - | ---- | ---------- |
| F1 | 世代照合は `EngineCore::execute_sql_in_session` の `Statement::Select` アーム（`using_plan()` が `Some` の分岐）で、I/O 前に読んだ `planning_generation` と I/O 後に読んだ `current_generation` を比較。不一致は `SqlSurfaceError::Internal`（固定の一般化メッセージ・存在情報を漏らさない）。再計画は行わない | `crates/engine/src/core.rs`（`Statement::Select` アーム） |
| F2 | バンプ箇所は `tenant.rs` の書き込み系関数群と `catalog.rs` の DDL・書き込み系関数群のすべて（呼び忘れは `crates/engine/tests/table_generation_bump_coverage.rs` がソース走査で構造的に検出） | `crates/engine/src/tenant.rs`・`crates/engine/src/catalog.rs`・`crates/engine/tests/table_generation_bump_coverage.rs` |
| F6 | `DictionaryCache::lookup` はストレージ全体の世代で有効性判定しており、本 ADR の対象である拒否判定（テーブル単位世代）より粗い。`dictionary_snapshot` 自体もストレージ全体世代の不一致で複数回再試行する | `crates/engine/src/core.rs`（`DictionaryCache` 周辺） |

## 可視性境界の事実関係

- 可視性ラベルは `Visibility::{Public, Private}` の 2 値のみ。**`Shared` variant は
  存在しない**（既存コードコメントの「Public/Shared」という表記は用語ずれであり、本
  ADR の作業で `crates/engine/src/core.rs` の該当コメントを是正した）
  （`crates/engine/src/storage.rs`）
- `PolicyContext::is_visible` は「`allowed_visibilities` に含まれる」かつ
  「`Public` または同一テナント」で可視と判定する。したがって要求元テナントの辞書
  内容に影響しうる他テナントの書き込みは **`Public` 行の書き込み・更新・削除** に
  限られる（`crates/engine/src/policy.rs`）
- SQL 表層・wire 経由の書き込み経路は可視性を **常に `Private` に固定** する
  （`crates/engine/src/sql/exec.rs::execute_insert`・ファイル形 `INSERT` の
  `crates/engine/src/incremental.rs`）。`Public` 行を書けるのは engine API 直接呼び出し
  （`crate::tenant::insert_row` 等・`RowInput.visibility`）のみであり、現行の
  SQL 表層・wire 経由の運用ではテーブル単位世代照合が「他テナントの `Public` 行書き込み」
  で拒否される経路（λ_other_public）は発生しない

## 拒否頻度の見積もり

対象テーブルへの write commit（DDL＋DML・全テナント）が平均レート λ [commit/s] の
ポアソン到着、`USING PLAN` 1 回の照合窓（`planning_generation` 読み取り → 辞書構築
〔キャッシュミス時〕→ LLM 往復 → 再埋め込み → `current_generation` 読み取り）の長さを
T [s] とすると、1 クエリあたりの拒否確率は `P = 1 − exp(−λT)`（λT ≪ 1 のとき
`P ≈ λT`）。`USING PLAN` のクエリレート Q [query/s] に対する単位時間あたり期待拒否数は
`Q · P`。

λ は次のように分解できる:

`λ = λ_own（要求元テナント自身の書き込み）+ λ_other_public（他テナントの Public 行）+ λ_other_private（他テナントの Private 行）+ λ_ddl（DDL）`

拒否粒度を可視性境界単位へ細分化して削減できるのは **`λ_other_private` の分のみ**
であり、`λ_own`・`λ_other_public`・`λ_ddl` は辞書内容に影響しうる（または要求元自身の
書き込みである）ため、細分化後も引き続き拒否対象になる。上記のとおり現行の
SQL 表層・wire 経由の運用では `λ_other_public = 0`。

T は private spec 側（PLAN-4/6/7）の数値基準に依存するため、本 ADR には spec 由来の
数値は記載しない。以下は **spec 由来ではない仮置きの例示値** による感度の目安（
`P ≈ λT` 近似）:

| λ（対象テーブルへの write commit 頻度） | T = 0.1 s | T = 1 s | T = 5 s |
| ---------------------------------------- | --------- | ------- | ------- |
| 0.01 commit/s（低頻度） | ≈0.001 | ≈0.01 | ≈0.05 |
| 0.1 commit/s（中頻度） | ≈0.01 | ≈0.1 | ≈0.4（近似の前提が崩れ始める） |
| 1 commit/s（高頻度） | ≈0.1 | 近似不可（`1 − e⁻¹ ≈ 0.63`） | 近似不可（`1 − e⁻⁵ ≈ 0.99`） |

**1 回の拒否そのもの**のクライアント側コストは「同一文の再送」であり、`USING PLAN`
は読み取り専用クエリのため副作用・部分書き込みは発生しない（再送しても状態は破壊
されない）。

ただし、これは**成功までの再送回数が 1 回に限られることを意味しない**。各再送も
同じ照合窓 T にさらされるため、書き込みが継続的に発生する対象テーブルでは再送のたび
に確率 P で再度拒否されうる。再送ごとの拒否がおおむね独立とみなせる場合、成功する
までの期待再送回数は幾何分布で `E[retries] = P / (1 − P)` と近似できる。上表の例示
値で言えば、`P ≈ 0.01`（低頻度・T = 1 s）では期待再送回数は 1 回未満で実務上無視でき
るが、`P` が 0.5 に近づく・またはそれを超える領域（例: 高頻度書き込みで T が長い場合）
では期待再送回数が発散し、**λ が高くクエリ側の再送では都度また拒否されるケースで
は starvation（対象テーブルへの書き込みが途切れない限り `USING PLAN` が成功しない
状態）が理論上あり得る**。したがって「クライアント側コストは再送 1 回に限られる」と
可用性評価をまとめることは過小評価であり、実際の運用判断では対象テーブルの λ・T の
実測値から期待再送回数（またはその発散有無）を判断材料に加える必要がある。

## 選択肢比較

| 選択肢 | 削減できる λ 成分 | fail-open リスク | 実装・テスト複雑度 |
| ------ | ------------------ | ------------------ | -------------------- |
| A. 現状維持（テーブル単位・全テナント共通） | なし | なし（既存の fail-closed 契約を変更しない） | なし |
| B. テナント×テーブル単位 | `λ_other_public` 相当分も誤って削減してしまう | **単独では他テナントの `Public` 行更新を見逃す**（要求元テナントの辞書に実際は影響する書き込みが検出対象から漏れる＝fail-open）。不採用 | 中 |
| C. 可視性境界単位（`public` 世代＋テナント世代の 2 系統） | `λ_other_private` | 呼び忘れ・キー選択誤りが直ちに fail-open になる新規リスク。`table_generation_bump_coverage.rs` の拡張が必須 | 高 |

## 判断

**選択肢 A（現状維持）を採用する。**

根拠:

- 削減できるのは `λ_other_private` の分のみであり、現行の SQL 表層・wire 経由の運用
  では `λ_other_public = 0` であることも踏まえると、細分化で得られる可用性改善は
  限定的
- 1 回の拒否自体は読み取り専用クエリの再送であり副作用を伴わない。ただし成功までの
  期待再送回数は λ・T に依存し 1 回に限定されない（λT が大きい領域では発散し得る。
  「拒否頻度の見積もり」節参照）。この点は starvation の理論的可能性として残るが、
  移行トリガー #2（承認済み計測環境での実測）で λT の運用上の許容性を確認する運用に
  委ねる
- 一方で細分化（選択肢 C）は「書き込み経路ごとに可視性を見てバンプ対象キーを選ぶ」
  ロジックを新設するため、呼び忘れ・キー選択誤りが直ちに fail-open（テナント境界の
  失効検出漏れ）になるリスクを新設する
- `security.md`「fail-closed を維持する」・自動運転モードの安全側判断に従い、リスクが
  新設される変更より現状維持を優先する

## 移行トリガーと細分化案の必須条件（将来案・未採用）

以下のいずれかが成立した場合、選択肢 C への移行を別 Issue で検討する:

1. SQL 表層／wire に可視性指定構文が露出し `λ_other_public > 0` になるとき
2. 承認済み計測環境での実測（Actions 外・非数値の状態のみ public へ記録）で `λT` が
   運用上許容できない値に達したとき
3. `DictionaryCache` のキーをテーブル世代へ切り替える変更を行うとき

細分化する場合の設計スケッチと、fail-open を防ぐ必須条件:

- キー: `(table, "__public__")` と `(table, tenant_id)` の 2 系統。判定は「`public`
  世代」と「要求元テナント世代」の **両方** が不変であること（どちらかが変化したら
  拒否）
- バンプ規則（必須条件）:
  1. DDL は `public` 世代を必ずバンプする
  2. 書き込む行・削除する行・更新前後どちらかの行が `Public` なら `public` 世代を
     バンプする
  3. すべての DML は書き手テナントの世代をバンプする
  4. `Visibility` の variant 追加時にコンパイルエラーで規則の再検討を強制する網羅
     `match` にする
  5. `table_generation_bump_coverage.rs` の走査を「可視性を見たキー選択」まで拡張する
  6. `allowed_visibilities` が `Public` を含まない ctx でも `public` 世代の変化で
     拒否側に倒す（過剰検知を許容する）

## 今回固定したテスト

以下は本 Issue で `crates/engine/src/core.rs` の `#[cfg(test)] mod tests` に追加した
判別テストで、「同一テーブルへの他テナント書き込み（`Public`／`Private` いずれも）で
拒否される」という **現行の意図的な契約**（将来の選択肢 C 移行時に書き換えが必要な
契約）を固定する:

- `execute_sql_in_session_rejects_using_plan_when_other_tenant_writes_public_row_to_same_table_during_io`
- `execute_sql_in_session_rejects_using_plan_when_other_tenant_writes_private_row_to_same_table_during_io`

既存テスト（変更なし。参考として一覧化）:

- `execute_sql_in_session_rejects_using_plan_when_table_generation_changes_during_io`
  （同一テーブルの `DROP`→再作成で拒否）
- `execute_sql_in_session_using_plan_succeeds_when_an_unrelated_table_is_written_during_io`
  （無関係テーブルへの書き込みでは拒否されない）
- `crates/engine/tests/table_generation_bump_coverage.rs`（書き込み経路の世代バンプ
  呼び忘れをソース走査で検出）

## スコープ外・申し送り

- 拒否時の `wire_code`（`XX000`）を「再送で回復可能」と判別可能な分類へ見直す是非
  （ERR-2 分類・`docs/spec/04-behavior/error-format.md` ポインタ。spec 側の判断を
  要する可能性があるため別 Issue 候補）
- `DictionaryCache::lookup` のキーをストレージ全体世代からテーブル世代へ揃える最適化
  （キャッシュヒット率改善。拒否精度とは独立の課題）
- 選択肢 C（可視性境界単位への細分化）の実装は、上記「移行トリガー」の成立時に
  別 Issue で扱う
- 承認済み計測環境での λ・T の実測（数値は非公開記録先へ。public には非数値の状態
  のみを記録する）

## 参照

- `docs/design/plan-rls-boost-interaction.md`
- `docs/design/rls-generalized-read-paths.md`
- `docs/design/operation-id-ledger.md`
