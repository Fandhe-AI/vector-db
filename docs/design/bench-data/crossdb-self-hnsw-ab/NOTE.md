# 訂正注記（Issue #658 codex-review P2 対応）

`20260908T153029Z-pair*-hnsw/self_hnsw.json` の `phases.bulk_hybrid_k200` に
記録されている `ef_effective`（`200`）は、計測時点の旧ハーネス
（`self_db.py::_ef_fields`）が `bulk_knn_k*` と同じ「`ef.max(k)`（`k` は SQL
の `LIMIT`）」計算式を `bulk_hybrid_k200` にも誤って流用した記録誤りである。

`ORDER BY hybrid_rrf(...)` は `sql/hnsw_hybrid.rs::HnswDenseProvider` 経由で
HNSW 密側探索へ結線されるが、実際に渡される候補幅は SQL の `LIMIT`（200）で
はなく `hybrid.rs::hybrid_search_boosted` が内部管理する `dense_fetch_k` で
あり、初回は `pool_depth * 2`（既定 `dense_fetch_k_initial = 400`）から始まり
境界の同点グループが確定できない場合は密側再取得ループが `MAX_FETCH_K`
（`10,000 * 4`）を上限に動的に倍増させる。したがって保存済み JSON の
`ef_effective: 200` は実際に HNSW 探索へ渡された候補幅を表していない。

この記録方式は `cfe93e5`（`self_hnsw.py::hybrid_ef_candidate_fields` 新設）で
修正済みで、修正後のハーネスは `bulk_hybrid_k200` の `ef_effective` を常に
`None` とし、`dense_fetch_k_initial`（初回候補幅の下限値。既定 `400`）と
`dense_fetch_k_may_expand: true`（再取得ループによる動的拡張の可能性）を
代わりに記録する。本ディレクトリの JSON は修正前ハーネスによる計測のため
再取得せず、この注記のみを残す（`bulk_hybrid_k200` のレイテンシ実測値
自体は候補幅の記録方式と無関係のため訂正不要）。詳細は
`scripts/crossdb_bench/README.md`「hybrid（`bulk_hybrid_k200`）の候補幅」節
参照。
