# RRF 融合結果の同点タイブレーク: 現状維持の判断記録

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-84（Issue #61。ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-103（RRF 融合。Issue #36 / PR #144 でマージ済み）・TASK-75（SQL-4 表層統合。
  Issue #56 / PR #184 でマージ済み）
- 関連ビヘイビア: SEARCH-1・SEARCH-3・SQL-4（ポインタ: `docs/spec/04-behavior/search.md`・
  `docs/spec/04-behavior/sql-surface.md`）
- 出典: PoC-10（`docs/spec/03-poc/query-protocol-comparison`。private spec 側の PoC
  実装。本文は転記せずポインタのみ参照）

## 目的

PoC-10（詳細は非公開のため本文へ転記しない。ポインタのみ参照）で確認された
RRF 融合スコアの同点タイブレークに関する非決定性の懸念が、本リポの `engine`
クレートへ持ち込まれていないかを判定し、判定結果を記録する。修正が必要な場合は
修正し、不要な場合は根拠とともに現状維持の判断を残す（TASK-84 の成果物は
「`hybrid.rs` の修正」または「現状維持の判断記録」のいずれか）。

## 調査結果

`crates/engine/src` のスコア順ソート経路をすべて確認した。

| 経路 | ファイル:関数 | 現状 |
| ---- | ------------ | ---- |
| 密 Top-k 選出 | `kernel.rs`（`MinHeapItem::cmp`・`TopKSelector::into_sorted_vec`） | ヒープ比較・最終ソートともスコアは `total_cmp`、同点は id 昇順でタイブレーク。最終整列は安定ソート（`sort_by`） |
| 並列・バッチ選出 | `parallel_search.rs`・`batch_search.rs` | いずれも `TopKSelector` を共用するため分割数・スレッド数に依存しない |
| 疎（BM25）Top-k | `sparse.rs`（`Candidate::cmp`） | `total_cmp` + `doc_id` 昇順、`sort_by`（安定） |
| RRF 融合 | `hybrid.rs`（`rrf_fuse`） | 累積は `HashMap` ではなく `BTreeMap`（走査順序が id 昇順で決定的）。最終整列は `sort_by`（安定）+ スコア降順・id 昇順タイブレーク |
| 融合結果の `LIMIT` 適用 | `hybrid.rs`（`hybrid_search`） | `rrf_fuse` が返す順序をそのまま `truncate(k)` するのみで、`truncate` 自体は順序を変えない |
| 再ランク | `rerank.rs` | 同様に `BTreeMap` 累積 + `sort_by_key`（安定） |
| SQL-4 実行 | `sql/exec.rs` | `hybrid_search`/`rrf_fuse` の順序をそのまま `LIMIT` 相当の件数へ切り詰めるのみ |

`crates/engine/src` に `sort_unstable_by`／`sort_unstable_by_key`／
`select_nth_unstable_by(_key)` の使用は 0 件（`scripts/check_sort_determinism.sh` で
機械確認）。`sort_unstable`（比較関数なし）の使用は `storage.rs` の整数 id 列 1 件のみで、
全順序を持つ整数値の並び替えであり同点タイブレークの問題を生じない。

入力側の契約検証も揃っている: `hybrid.rs` は密・疎双方の入力が「スコア降順・同点は
id 昇順」で事前ソート済みであることを検証し（`is_sorted_desc_id_asc`）、非有限スコアを
拒否し、同一リスト内の id 重複を拒否する。

## 判断

**修正不要・現状維持**。`engine` 本体の RRF 融合経路は、PoC-10 が指摘した非決定性の
原因（詳細は転記しない。ポインタのみ参照）を持たない。

判断を将来にわたって検証可能にするため、以下を本 PR で追加する（`hybrid.rs` 自体の
ロジックは変更しない）:

1. **回帰テスト**（既存テストが薄かった「同点グループが `LIMIT`/`k` 境界を跨ぐ」
   ケースを補う）
   - `crates/engine/src/hybrid.rs`（`mod tests`）:
     `rrf_fuse_multiple_tie_groups_are_ordered_score_desc_id_asc`
   - `crates/engine/src/kernel.rs`（`mod tests`）:
     `tied_scores_are_ordered_by_id_ascending_when_multiple_survive`
   - `crates/engine/tests/hybrid.rs`:
     `hybrid_search_tie_group_across_limit_boundary_is_deterministic_and_matches_oracle`
     （`ParallelSearchProvider` での 20 回反復実行の bit 一致 + 独立オラクルとの一致）
   - `crates/engine/tests/sql_surface.rs`:
     `sql4_hybrid_tie_group_across_limit_boundary_is_deterministic`（SQL-4 end-to-end
     で同一の反復実行検証 + `hybrid_rrf(...)`/`HYBRID(...)` 2 構文形の一致）
2. **軽量ガードスクリプト**（`scripts/check_sort_determinism.sh`。`make
   sort-determinism-check`・CI の `sort-determinism-check` ジョブから実行。詳細は
   スクリプト冒頭コメント参照）: `sort_unstable_by`/`sort_unstable_by_key`/
   `select_nth_unstable_by(_key)` の再混入を機械的に検知する。

## 維持すべき不変条件

以後 `hybrid.rs`・`kernel.rs`・`sparse.rs`・`rerank.rs`（および将来 wire 経由で結果を
再ソートする経路を追加する場合はその経路も含む）が壊してはならない契約:

- スコア順に並べる箇所は必ず**安定ソート**（`sort_by`/`sort_by_key`。`sort_unstable_by`
  系は使わない）を使う。
- 同点（スコアが等しい）要素のタイブレークは常に **id 昇順**とする。
- スコアの比較には（NaN を含む非全順序を扱う場合）`total_cmp` を使い、非有限値は
  比較前に事前拒否する（`hybrid.rs`・`kernel.rs`・`sparse.rs` の既存契約と同じ）。
- RRF 等の融合スコアを id ごとに累積する構造は `HashMap` ではなく `BTreeMap`（または
  他の決定的走査順序を持つ構造）を使う。
- `LIMIT`/`k` による切り詰め（`truncate`）は、切り詰め前の順序が上記契約を満たして
  いる限り、それ自体は非決定性を生まない（`truncate` は要素の順序を変更しない）。
  ただし切り詰め前の順序が同点タイブレークを欠いていれば、どの位置で切っても
  非決定的になりうる点に注意する。

## 例外として許容する箇所

- `storage.rs` の整数 id 列に対する `sort_unstable`（比較関数を渡さない版）。id は
  全順序を持ち、同一 id の重複が意味を持つペイロードでもないため、不安定ソートでも
  結果の並びは一意に定まる。`scripts/check_sort_determinism.sh` はこのケースを検知
  対象パターン（比較関数を伴う `_by`/`_by_key` 系のみ）から除外している。

## スコープ外・申し送り

- PoC 側（`docs/spec/03-poc/query-protocol-comparison`）の実装は spec リポ管理のため
  本リポでは変更しない。
- wire-server 側で結果順序を再ソートする経路は現時点で存在しない。将来 wire 経由の
  結果返却を実装する際も、本 ADR の順序契約（安定ソート + id 昇順タイブレーク）を
  維持すること。
- `sort-determinism-check` の branch protection 必須チェック登録はユーザー作業
  （既存 `core-api-check` と同運用。PR 本文参照）。

## 参照

- TASK-84（ポインタ: `docs/spec/05-tasks.md`）
- SEARCH-1・SEARCH-3（ポインタ: `docs/spec/04-behavior/search.md`）
- SQL-4（ポインタ: `docs/spec/04-behavior/sql-surface.md`）
- `crates/engine/src/hybrid.rs`・`crates/engine/src/kernel.rs`・
  `crates/engine/src/sparse.rs`・`crates/engine/src/rerank.rs`
- `scripts/check_sort_determinism.sh`
