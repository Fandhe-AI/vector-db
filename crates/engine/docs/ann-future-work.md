# ANN 併用時の RLS 再評価（将来検討課題）

## 目的・位置づけ

本メモは TASK-132（`docs/spec/05-tasks.md`）・対象ビヘイビア CORE-10 に対応する
将来検討課題の記録である。**実装タスクではない**。ここに記すのは再評価すべき
観点の整理であり、方式の選定・意思決定は行わない。

方式選定の意思決定は、`SearchEngineKind`（CORE-9 の拡張点）へ ANN variant を
実際に追加するタイミングで、オーナー（人間）が行う。本メモはその判断のための
たたき台としてのみ位置づける。

## 現行アーキテクチャの前提

現行実装（public なドキュメンテーションコメントの範囲）は次の 2 点で構成される。

- 検索エンジンの選択は `search_engine.rs` の `SearchEngineKind`（`#[non_exhaustive]`）
  が担い、`kernel.rs::SearchProvider` の provider 注入機構に一本化されている。
  将来の ANN 実装は `SearchEngineKind` に variant を追加し、`build` に分岐を
  1 つ加えるだけで組み込める設計になっている（CORE-9・CORE-13）。
- RLS（行レベルセキュリティ相当）は `rls.rs` が事前フィルタ（`PrefilterIndex`:
  構築時に `PolicyContext` を束縛し、可視行のみの縮約ビューを作る）と検索時
  フィルタ（`SearchTimeFilter`: ポリシーが動的な場合のフォールバックで、縮約
  ビューを保持しない）の 2 方式を提供する。使い分けは呼び出し元の責務である。

## 将来検討課題（本題）

グラフベース ANN（HNSW 等）と事前フィルタリングを組み合わせる場合、一般に
知られている課題として、フィルタ後の縮約集合上ではグラフの接続性が変化し、
総当たり探索と同等の再現率が保証されなくなるリスクがある（いわゆる filtered
ANN search の一般的な性質）。現行の `PrefilterIndex` は総当たり系 provider を
前提に「可視行のみの縮約ビューを構築してから検索する」設計であり、ANN
provider をそのまま組み合わせた場合にこの性質が問題化しうるかどうかは、
ANN 導入時に個別に検証が必要である。

このため、`SearchEngineKind` に ANN variant を追加するタスクに着手する際は、
以下を再評価タスクとして扱う。

### 再評価の観点

- 事前フィルタ（`PrefilterIndex` 方式）・事後フィルタ・filter-aware ANN
  （フィルタを考慮したグラフ探索アルゴリズム）の方式比較
- 可視率帯（テナントの可視行がデータセット全体に占める割合）ごとの再現率計測
- 既存の RLS ビヘイビアの受け入れ基準の再検証（`rls.rs` が担保する契約が
  ANN provider でも成立するかの確認）
- `SearchInput`（`kernel.rs`）の「可視行のみの縮約ビュー」契約と、選択した
  ANN 方式との整合性

### 再評価のトリガー条件

`SearchEngineKind` へ ANN variant を追加するタスク（CORE-9 の拡張点を実際に
行使するタイミング）の着手時。

Issue #407 で `SearchEngineKind::Hnsw` variant・`build` 結線・`EngineCore`
構築時 opt-in（`open_with_engine`／`from_storage_with_engine`）を追加し、この
トリガー条件に到達した。ただし #407 の `HnswSearchProvider`
（`crates/engine/src/hnsw/provider.rs`）は本メモが挙げる再評価観点（事前
フィルタとの組み合わせ・可視率帯ごとの再現率等）に触れる索引探索を一切行わず、
常に総当たり系 `ParallelSearchProvider` へ委譲する（全件 brute-force
フォールバック）。RLS（`rls.rs::PrefilterIndex`）との実質的な組み合わせ・
本メモの再評価は、索引を実際に探索へ使うタスク（世代整合キャッシュの #408、
RLS 統合・切替の #409／#410）が担う（詳細は
`docs/design/hnsw-search-engine-wiring.md` 参照）。

## スコープ外

本メモの時点では ANN の実装・具体的な方式決定は行わない。数値的な受け入れ
基準・PoC の詳細は spec 側（`docs/spec/05-tasks.md` の TASK-132）を参照する。

## 参照

- タスク: TASK-131（#43・完了）・TASK-132（本メモ）・TASK-133（#44・完了）
- ビヘイビア: CORE-9・CORE-10
- 関連ソース: `crates/engine/src/search_engine.rs`・`crates/engine/src/rls.rs`
- 採否判断材料: `docs/design/ann-index-adoption.md`（Issue #367。判断確定済み:
  B 案・Issue #402／#403）
