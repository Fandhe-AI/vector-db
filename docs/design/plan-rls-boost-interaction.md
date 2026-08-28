# LLM クエリプランニングのソフトブーストヒントと RLS 事前フィルタの競合検証

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-139（ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-111（PR #257／Issue #73 でマージ済み。`hybrid.rs` のソフトブースト機構）・
  TASK-133（Issue #44 でマージ済み。`rls.rs::PrefilterIndex`／`ImplicitRlsHook`）
- 関連ビヘイビア: なし（基盤タスク。ポインタ: `docs/spec/04-behavior/rls.md`・
  `docs/spec/04-behavior/core-engine.md`・`docs/spec/04-behavior/query-planning.md`）
- 検証コード: `crates/engine/tests/plan_rls_boost.rs`

## 目的

TASK-110（LLM クエリプランニング）が返す `path_hint`/`kind_hint` を RRF 融合後スコアへ
反映するソフトブースト機構（TASK-111）と、RLS 事前フィルタ（TASK-133）はいずれも
独立に実装・検証済みだが、両者を同時に適用した場合の相互作用は検証課題として
引き継がれていた（`docs/spec/04-behavior/rls.md`・`docs/spec/04-behavior/core-engine.md`・
`docs/spec/04-behavior/query-planning.md` のポインタ）。本ドキュメントは両機構が
実際に合流する層のインベントリと `crates/engine/tests/plan_rls_boost.rs` の検証方針を
記録する。個々の検証観点の具体的な内容・結果は private spec 側の SSOT および同テスト
ファイルのコードを参照すること（本ドキュメントには転記しない）。

## 両機構の合流点

`USING PLAN` の SQL 実行器（TASK-77）は本ドキュメント時点で未実装であり、許可リストが
`42601` で拒否する。したがって検証は SQL 表層・wire 経由ではなく、両機構が実際に
合流する engine API 層で行う（`docs/design/rls-generalized-read-paths.md` と同じ
スコープ境界の整理）。

| # | 構成要素 | 役割 |
| - | -------- | ---- |
| C1 | `tenant::visible_rows`（`crates/engine/src/tenant.rs`） | RLS 事前フィルタ済み候補集合（可視行のみ）の構築 |
| C2 | `hybrid::path_hint_matches`／`kind_hint_matches`（`crates/engine/src/hybrid.rs`） | ヒント一致判定 |
| C3 | `hybrid::BoostRule::new` | ブーストルールの検証付き構築 |
| C4 | `hybrid::hybrid_search_boosted` | RRF 融合後・`truncate(k)` 前の加点適用 |
| C5 | `tenant::verify_hits` | C1〜C4 の結果を独立に再検証する検査器（実装と経路分離） |

呼び出し列は C1 → C2 → C3 → C4 であり、`crates/engine/tests/plan_rls_boost.rs` の
各テストはこの列を共有する（`rls_filtered_candidates` ヘルパ）。

## 検証方針

`crates/engine/tests/plan_rls_boost.rs` は実装変更なしの読み取り検証として、上記の
呼び出し列を対象に次の観点を軸としたテストを固定する（各観点の詳細な構成・
判定根拠はテストコード自体および private spec 側の SSOT を参照。本ドキュメントには
転記しない）。

- RLS 事前フィルタ済み候補集合の外側から結果が復活しないこと
- ブーストルールの構築規約を守った場合の可視行への結果の同一性
- ブースト本来の効果（RLS なし構成との定性一致）が保存されること
- RLS 事前フィルタと不可視行の存在が可視行の検索結果へ追加の影響を及ぼさないこと
- ブーストルールの構築時エラーの発生有無が不可視データの状態に依存しないこと

構築規約に反した誤実装を再現する negative test も含めて固定するが、その具体的な
差分の内容は private spec 側の SSOT を参照すること。本タスクは読み取り検証タスクで
あり、現行 production コードへの変更は行っていない。ソフトブースト機構（TASK-111）
は `hybrid.rs` 単体で完結しており、`sql/exec.rs` への実結線自体が TASK-111 の
ドキュメントで明示的にスコープ外とされている（本タスクも同様に読み取り検証に
限定し、実結線は行わない）。

## 検証コードの既知の限界

`crates/engine/tests/plan_rls_boost.rs` の一部テストは、単一 `ctx` の可視集合自体が
同一の数値 id を複数含む合成コーパスの構成可能性（ポインタ: TABLE-12）を対象に、
`kernel.rs::SearchInput::ids` が定める候補識別子の一意性契約に反した構築が
fail-closed に拒否されることを固定する。契約の詳細は同ドキュメントを参照
（ポインタ）。

`path`/`kind` ヒント一致判定の入力（C2 の前段）についても同様の限界がある。
`crates/engine/tests/plan_rls_boost.rs::DocMeta` の `path`/`kind` はテスト側の
グラウンドトゥルースであり、production で `sql/exec.rs`（TASK-77）が可視行
（C1 の出力）から `path`/`kind` メタデータをどう構築するかは本タスクの検証範囲外
（未実装のため構築経路自体が存在しない）。したがって本検証は C1（`visible_rows`）
→ C2（ヒント一致判定）→ C3（`BoostRule::new`）→ C4（`hybrid_search_boosted`）の
呼び出し列そのものを対象にした engine API 層での合成検証であり、`sql/exec.rs` が
可視行メタデータから `path`/`kind` を取り違えて構築するような実装不具合（RLS
適用前のメタデータ源からヒントを構築する等）は本検証の対象外で、混入しても
本テスト群は検出できない。

## `USING PLAN` 実行器（TASK-77）実装後の残課題

TASK-77 実装後、SQL 表層・wire 経由での本検証の再実施が必要（`sql/exec.rs` への
実結線自体は TASK-111 のドキュメントが明記するとおり本タスク・TASK-111 いずれの
スコープ外）。`kernel.rs::SearchInput::ids` の候補識別子契約が `sql/exec.rs` 側の
実装でも守られていることを、`tests/rls_generalized.rs`（TASK-138）と同様の任意
テーブル・任意形状 SELECT の軸で機械検証すること。

これに加え、上記「検証コードの既知の限界」で述べた `path`/`kind` メタデータ構築の
検証空白を埋める回帰テストを TASK-77 実装時の**必須の受け入れ基準**として追跡する。
具体的には、`sql/exec.rs` が RLS 事前フィルタ済みの可視行（production の
`Storage`／query planner 経由）から実際に `path`/`kind` を構築し、その結果を C2〜C4
（ヒント一致判定 → `BoostRule` 構築 → `hybrid_search_boosted`）へ渡す経路を、
本ファイルの合成コーパス流儀ではなく production 経路そのもので end-to-end 検証する
こと。TASK-77 着手時にこの回帰テストが計画へ含まれていない場合は、TASK-77 の
実装を完了扱いにしない。
