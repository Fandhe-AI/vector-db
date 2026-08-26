# RLS 暗黙適用の一般化検証: 読み取り経路インベントリと検証方針

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-138（ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-137（PR #186 でマージ済み）・TASK-79（PR #209 でマージ済み）・TASK-75（PR #184 でマージ済み）・TASK-85（PR #23 でマージ済み）
- 関連ビヘイビア: RLS-8（ポインタ: `docs/spec/04-behavior/rls.md`）

## 目的

TASK-137（RLS-6, RLS-7）で機械検証した「RLS の暗黙適用をクライアントが解除できない」
契約は、これまで MVP クエリカタログ C1〜C4 相当の経路（`tests/rls_implicit.rs`）で
のみ検証されていた。RLS-8 はこの契約が **C1〜C4 以外のすべての読み取り経路** へも
一般化されて働くことを求める。本ドキュメントは engine クレートが公開する
クライアント到達可能な読み取り経路を棚卸しし、`tests/rls_generalized.rs` の検証方針を
記録する。契約の具体的な内容は private spec 側の SSOT を参照すること
（本ドキュメントには転記しない）。

## 読み取り経路インベントリ

| # | 経路 | RLS 注入点 | 本 PR 以前のカバレッジ |
| - | ---- | ---------- | ----------------------- |
| R1 | `EngineCore::execute_sql`（任意形状 SELECT） | `rls.rs::ImplicitRlsHook`（`sql/exec.rs::execute_statement` 経由） | C1〜C4・`HINT ORDER` 順列・precision モードに限定 |
| R2 | `EngineCore::execute_sql_in_session`（`SessionState` 経由。`SET search_mode`・`CREATE FUNCTION`・`USING MODE`） | 同上 | UDF 単一テーブルの限定シナリオのみ |
| R3 | `VectorCore::search`（trait 経由。wire-server が依存する窓口） | `rls.rs::ImplicitRlsHook` | 単一テーブルに限定 |
| R4 | `VectorCore::get_row`（`(tenant_id, id)` 点取得） | `rls.rs::ImplicitRlsHook` | 単一テーブルに限定 |
| R5 | `tenant::visible_rows` / `tenant::verify_hits`（行ストア統合層の参照実装） | `PolicyContext::is_visible` 直接 | 単一テーブルに限定 |
| R6 | `rls::PrefilterIndex` / `rls::SearchTimeFilter` | 構築時／検索時に `ctx` 束縛 | 既存の専用テストで検証済み（本 PR の対象外） |
| — | `Storage::get`/`scan`/`scan_page`/`scan_table_page`/`get_row_from_table`、`VectorArena::build` | 認可なしの生 API（内部用。wire 層からは構造的に到達不能） | 対象外（インベントリ記録のみ） |

## 検証方針

`tests/rls_generalized.rs`（TASK-138）は次の軸で C1〜C4 以外の一般化を検証する。

- **任意テーブル**: スキーマ・次元・列名の異なる 3 テーブルを併存させ、行 id を
  テーブル間で再利用してテーブル取り違えも検出対象にする。
- **任意形状 SELECT**: 投影（`*`／`id`）・WHERE（なし／等価／式述語／AND 結合）・
  ORDER BY（距離形／`hybrid_rrf`）・`HINT ORDER`・LIMIT の直積を生成し、独立オラクル
  で混入 0 件・`visible()` 述語の有無による結果不変性を全テナント×可視性設定で検証する。
- **UDF からの読み取り**: 宣言的 UDF（`CREATE FUNCTION`）を結果列・WHERE の両位置から
  呼び、不可視行のゼロベクトルカナリア（`vec_div(v, vec_norm(v))` が評価されれば
  0 除算で失敗する）で「不可視行では評価されない」ことを、可視カナリアで
  「可視行では正しく評価される」ことを両側から固定する。
- **`VectorCore::search`／`get_row`／`tenant::visible_rows`**: 独立オラクルに加え、
  `tenant::verify_hits` の `(tenant_id, id)` 完全キー照合でも重ねて検証する。
- **fail-closed の確認**: `USING PLAN` は許可リストにより `42601` で拒否されることを
  固定する。

## `USING PLAN` 展開後クエリの扱い

TASK-77（プラン文字列実行器）は本 PR 時点で未実装であり、許可リストが `USING PLAN` を
一律 fail-closed に拒否する。したがって「展開後クエリへの RLS 暗黙適用一般化」の
機械検証は実施できない。本 PR では「未実装経路が暗黙適用なしに開いていない」ことの
確認（拒否経路の固定）に留め、展開後クエリの検証は TASK-77 実装後に TASK-117 の
管轄で行う。

## 生 `Storage` API の扱い

`Storage::get`/`scan`/`scan_page` 等・`VectorArena::build` は認可なしの内部 API であり、
`wire-server` の接続ハンドラからは `core::VectorCore` trait 経由でのみ到達する構造の
ため、クライアントから直接到達しない。本 PR ではインベントリへの記録に留め、
API 変更は行わない。
