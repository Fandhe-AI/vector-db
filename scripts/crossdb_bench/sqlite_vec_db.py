"""sqlite-vec 0.1.9（in-process）の機能別ベンチマーク実装。

vec0 仮想テーブルは `distance_metric=dot`（内積）に相当する指定を持たない
（実機確認: `l2`・`l1`・`cosine` のみ受理。`dot`/`ip`/`inner_product` は
constructor error）。そのため本モジュールはコサイン距離で近似し、
meta に注記する（task 指示「無ければコサイン＋注記」に準拠）。

vec0 の auxiliary 列（`+col` 宣言）は KNN（`MATCH` を伴う）クエリの WHERE 句に
使えない（実機確認: "An illegal WHERE constraint was provided on a vec0
auxiliary column in a KNN query."）。可視性フィルタ（RLS 相当）は必須のため、
`visibility` は `partition key` として宣言し、`lang` は素の列（`+` なし）
として宣言する——実機確認どおり `partition key` 列・素の列はいずれも KNN の
WHERE 句に使える。

可視性モデル（実機確認済みの現行契約）: 自作 DB はどのテナントの wire
セッションからも `visibility = 'public'` の行のみが可視であり、private 行は
所有テナント自身のセッションからも不可視。よってフィルタは `visibility =
'public'` のみで、`tenant` 列では絞り込まない（`tenant` 列自体は参考情報
として保持する）。

exact のみ（vec0 は他 ANN 索引構成を持たない。`--config` は無視して exact 固定）。
"""

from __future__ import annotations

import sqlite3
import time

import sqlite_vec

from common import (
    TENANT_VISIBLE,
    build_meta,
    measure,
    unsupported,
)


def _connect() -> sqlite3.Connection:
    conn = sqlite3.connect(":memory:")
    conn.enable_load_extension(True)
    sqlite_vec.load(conn)
    conn.enable_load_extension(False)
    return conn


def _pack(vec: list[float]) -> bytes:
    return sqlite_vec.serialize_float32(vec)


def _setup_schema(conn: sqlite3.Connection, dim: int) -> None:
    cur = conn.cursor()
    cur.execute(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, tenant TEXT, visibility TEXT, "
        "lang TEXT, topic TEXT, body TEXT)"
    )
    cur.execute(
        f"CREATE VIRTUAL TABLE vec_docs USING vec0("
        f"embedding float[{dim}] distance_metric=cosine, "
        f"visibility TEXT partition key, tenant TEXT, lang TEXT, topic TEXT)"
    )
    cur.execute("CREATE VIRTUAL TABLE docs_fts USING fts5(body, content='docs', content_rowid='id')")
    conn.commit()


def _ingest_bulk(conn: sqlite3.Connection, docs: list[dict]) -> dict:
    t0 = time.perf_counter()
    cur = conn.cursor()
    cur.executemany(
        "INSERT INTO docs (id, tenant, visibility, lang, topic, body) VALUES (?, ?, ?, ?, ?, ?)",
        [
            # 旧フィクスチャ（visibility 未導入）との互換のため欠落時は
            # fail-closed で private 扱いにする。
            (d["id"], d["tenant"], d.get("visibility", "private"), d["lang"], d.get("topic", ""), d["body"])
            for d in docs
        ],
    )
    cur.executemany(
        "INSERT INTO vec_docs (rowid, embedding, visibility, tenant, lang, topic) VALUES (?, ?, ?, ?, ?, ?)",
        [
            (
                d["id"],
                _pack(d["embedding"]),
                d.get("visibility", "private"),
                d["tenant"],
                d["lang"],
                d.get("topic", ""),
            )
            for d in docs
        ],
    )
    cur.executemany(
        "INSERT INTO docs_fts (rowid, body) VALUES (?, ?)",
        [(d["id"], d["body"]) for d in docs],
    )
    conn.commit()
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": len(docs), "seconds": elapsed, "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None}


def _ingest_single_stmt(conn: sqlite3.Connection, dim: int, n_rows: int = 1000) -> dict:
    import random

    t0 = time.perf_counter()
    cur = conn.cursor()
    for n in range(n_rows):
        rid = 10_000_000 + n
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        body = f"crossdb bench ingest row {n}"
        emb = [random.random() * 2 - 1 for _ in range(dim)]
        cur.execute(
            "INSERT INTO docs (id, tenant, visibility, lang, topic, body) VALUES (?, ?, ?, ?, ?, ?)",
            (rid, TENANT_VISIBLE, "private", lang, topic, body),
        )
        cur.execute(
            "INSERT INTO vec_docs (rowid, embedding, visibility, tenant, lang, topic) VALUES (?, ?, ?, ?, ?, ?)",
            (rid, _pack(emb), "private", TENANT_VISIBLE, lang, topic),
        )
        cur.execute("INSERT INTO docs_fts (rowid, body) VALUES (?, ?)", (rid, body))
    conn.commit()
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": n_rows, "seconds": elapsed, "rows_per_sec": n_rows / elapsed if elapsed > 0 else None}


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    conn = _connect()
    dim = len(docs[0]["embedding"]) if docs else 128
    _setup_schema(conn, dim)
    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(conn, docs)

    def knn(qv):
        cur = conn.cursor()
        cur.execute(
            "SELECT rowid FROM vec_docs WHERE embedding MATCH ? AND k = 10 "
            "AND visibility = 'public' ORDER BY distance",
            (_pack(qv),),
        )
        return [r[0] for r in cur.fetchall()]

    query_vecs = [q["embedding"] for q in queries]
    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    def knn_where(qv):
        cur = conn.cursor()
        cur.execute(
            "SELECT rowid FROM vec_docs WHERE embedding MATCH ? AND k = 10 "
            "AND visibility = 'public' AND lang = 'ja' ORDER BY distance",
            (_pack(qv),),
        )
        return [r[0] for r in cur.fetchall()]

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {"note": "vector_knn_where と同一クエリ形のため統合", **stats}

    def compound_count(_):
        cur = conn.cursor()
        cur.execute(
            "SELECT COUNT(*) FROM docs WHERE visibility = 'public' AND id > 100 AND lang = 'ja'"
        )
        return cur.fetchone()[0]

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        cur = conn.cursor()
        cur.execute("SELECT COUNT(*) FROM docs WHERE visibility = 'public'")
        return cur.fetchone()[0]

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    def agg_multi(_):
        cur = conn.cursor()
        cur.execute(
            "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs WHERE visibility = 'public'"
        )
        return cur.fetchone()

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {**stats, "result": list(last) if last else None}

    def group_by(_):
        cur = conn.cursor()
        cur.execute(
            "SELECT lang, COUNT(*) AS n FROM docs WHERE visibility = 'public' "
            "GROUP BY lang HAVING n > 1 ORDER BY n DESC LIMIT 5"
        )
        return cur.fetchall()

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {**stats, "result": last}

    def hybrid(i):
        qv = query_vecs[i]
        qt = queries[i].get("text", "")
        cur = conn.cursor()
        cur.execute(
            "SELECT rowid FROM vec_docs WHERE embedding MATCH ? AND k = 50 "
            "AND visibility = 'public' ORDER BY distance",
            (_pack(qv),),
        )
        knn_ids = [r[0] for r in cur.fetchall()]
        try:
            cur.execute(
                "SELECT docs_fts.rowid FROM docs_fts JOIN docs ON docs.id = docs_fts.rowid "
                "WHERE docs_fts MATCH ? AND docs.visibility = 'public' ORDER BY bm25(docs_fts) LIMIT 50",
                (qt,),
            )
            fts_ids = [r[0] for r in cur.fetchall()]
        except sqlite3.OperationalError:
            # FTS5 のクエリ構文エラー（記号混入等）は空集合扱いにして KNN 側のみで融合する。
            fts_ids = []
        # RRF(k=60) を Python 側で計算（sqlite-vec/FTS5 にネイティブ融合機能は無い）。
        scores: dict[int, float] = {}
        for rank, rid in enumerate(knn_ids, start=1):
            scores[rid] = scores.get(rid, 0.0) + 1.0 / (60 + rank)
        for rank, rid in enumerate(fts_ids, start=1):
            scores[rid] = scores.get(rid, 0.0) + 1.0 / (60 + rank)
        top = sorted(scores.items(), key=lambda kv: -kv[1])[:10]
        return [rid for rid, _ in top]

    idxs = list(range(len(query_vecs)))
    stats, _ = measure(hybrid, idxs)
    phases["hybrid_rrf"] = stats

    phases["mode_recall"] = unsupported("sqlite-vec にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("sqlite-vec にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported(
        "自作 DB の宣言的 UDF 呼び出し相当（SQL 内合成関数）の機能が無い"
    )

    # sqlite-vec には自作 DB の wire セッションに相当するテナント別接続の概念が
    # 無いため、tenant-b「セッション」を模す接続は作らず、同じ public-only
    # フィルタを再実行して agg_count と同値になることの一致確認とする
    # （自作 DB は tenant-a・tenant-b いずれのセッションでも `visibility =
    # 'public'` の行のみ可視という現行契約——実機確認済み）。
    def rls_count(_):
        cur = conn.cursor()
        cur.execute("SELECT COUNT(*) FROM docs WHERE visibility = 'public'")
        return cur.fetchone()[0]

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "sqlite-vec にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    def explain(qv):
        cur = conn.cursor()
        cur.execute(
            "EXPLAIN QUERY PLAN SELECT rowid FROM vec_docs WHERE embedding MATCH ? "
            "AND k = 10 AND visibility = 'public' ORDER BY distance",
            (_pack(qv),),
        )
        return cur.fetchall()

    stats, last = measure(explain, query_vecs)
    phases["explain"] = {**stats, "sample_output": last}

    cur = conn.cursor()
    cur.execute("SELECT vec_version()")
    vec_version = cur.fetchone()[0]

    # ingest_single_stmt はテーブルへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する
    # （self_db.py と同じ順序方針）。
    phases["ingest_single_stmt"] = _ingest_single_stmt(conn, dim)

    meta = build_meta(
        db="sqlite_vec",
        version=f"sqlite3 {sqlite3.sqlite_version} / sqlite-vec {vec_version}",
        connection="in-process",
        config="exact",
        rows=len(docs),
        dim=dim,
        extra={
            "index": "exact (vec0 brute-force)",
            "distance_metric_note": "内積 distance_metric 非対応のためコサイン距離で近似",
        },
    )
    conn.close()
    return {"meta": meta, "phases": phases}
