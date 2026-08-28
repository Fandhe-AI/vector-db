# ADR: 既公開エラー enum への `#[non_exhaustive]` 後付け不採用（Issue #282）

- ステータス: Accepted
- 対応: Issue #282（PR #226・TASK-93 の申し送り事項）
- 関連: `docs/spec/05-tasks.md`（TASK-93・TASK-152）・
  `docs/spec/04-behavior/error-format.md`（ERR-2）

## 概要

PR #226（TASK-93）で `crates/engine/src/tenant.rs` の `pub enum TenantWriteError` に
`LedgerCorrupted(StorageError)` variant を追加した際、`#[non_exhaustive]` が付与されて
いないためソース非互換な破壊的変更として扱われた。一方で `#[non_exhaustive]` の付与
自体もクレート外の網羅的 `match` を壊す破壊的変更であり、公開 API 方針として
PR #226 のスコープ外に切り出されたのが Issue #282 である。

## 判断

`TenantWriteError` を含む、既に公開済みのエラー enum へ `#[non_exhaustive]` を
**後付けしない**。variant の追加は従来どおり PR 本文の Breaking changes 節と
コミットの `BREAKING CHANGE:` フッタで明示する（[conventional-commits](../../.claude/rules/conventional-commits.md)）。

新規に導入するエラー enum については、本 ADR の対象外とし、導入時点で個別に判断する
（`sql/mode.rs::ModeSource`・`search_engine.rs::SearchEngineKind` は新規導入時に
`#[non_exhaustive]` を付与済みの先例。いずれも後付けではない）。

## 根拠

1. **直近の codex-review 判断との整合**: `core.rs::CoreError`（PR #252・codex-review P1
   指摘）は「既に公開済みの enum へ後付けで `#[non_exhaustive]` を付けると下流の
   網羅的 `match` がコンパイル不能になる（それ自体が破壊的変更）。互換性を装わず
   付けないまま、variant 追加は PR で破壊的変更として明記する」と結論づけ済み。
   `storage.rs::StorageError`（PR #193・codex 指摘）でも同種の付与案が「公開 enum・
   既存の網羅的 match への破壊的変更」として差し戻されている。
2. **engine の既定方針との整合**: `error_format.rs::ErrorClass`・`isa.rs::DetectedIsa`・
   `dispatch.rs::ExecutionPath` はいずれも「variant 追加時にコンパイルエラーで
   更新漏れを検出させる」目的で意図的に `#[non_exhaustive]` を付与していない。
3. **恩恵を受ける利用者が現時点で不在**: `crates/wire-server/src/` に
   `TenantWriteError` への `match`・参照は 0 件（wire-server は
   `engine::error_format::ErrorClass` のみを参照する）。engine の結合テストも
   `matches!` による単一 variant 判定のみで、網羅的 `match` を持たない。workspace は
   `publish = false` であり、engine の公開 API 利用者は同一 workspace の wire-server
   のみ。破壊的変更を今行う実益がない。
4. **fail-closed なエラー契約の維持（ERR-2）**: `#[non_exhaustive]` を付与すると
   クレート外の利用者は `_ =>` の包括アームを強制され、将来 variant を追加した際に
   `wire_code` 分類が暗黙に包括アーム側へ落ちる経路ができる。非付与のままなら
   variant 追加はコンパイルエラーとして表面化し、`wire_code` 写像の更新漏れを
   構造的に防げる。

## 不採用にした代替案

- **`TenantWriteError` 等へ `#[non_exhaustive]` を付与する**: クレート外に現状
  `match` する利用者がいないため恩恵がない一方、`_` 包括アームによる `wire_code`
  分類漏れのリスクを新たに生む。既に公開済みの enum への後付けであるため、それ自体が
  ソース非互換な破壊的変更になる点も PR #226 と同様に解消できない。不採用。

## 適用範囲

既公開のエラー enum（`TenantWriteError`・`TenantError`・`CoreError`・
`StorageError`・`CatalogError`・`SqlSurfaceError` 等）には、本 ADR に基づき後付けで
`#[non_exhaustive]` を付与しない。新規に導入する型は本 ADR の対象外とし、導入 PR の
中で個別に判断する。

## 影響を受けるコード

- `crates/engine/src/tenant.rs`（`TenantWriteError`・`TenantError` の doc コメントへ
  本 ADR へのポインタを追加。属性・シグネチャは変更しない）
- `crates/engine/tests/tenant_write_error_exhaustive.rs`（新規: クレート外から
  `TenantWriteError` を `_` アームなしで網羅 `match` し、方針をコンパイル時にピン留め）
- 先例コメント: `crates/engine/src/core.rs`（`CoreError`）・
  `crates/engine/src/storage.rs`（`StorageError`）・
  `crates/engine/src/error_format.rs`（`ErrorClass`）

## 参照

- `docs/spec/05-tasks.md`（TASK-93・TASK-152）
- `docs/spec/04-behavior/error-format.md`（ERR-2）
- PR #226（TASK-93）・PR #252（codex-review P1 指摘）・PR #193（codex 指摘）
