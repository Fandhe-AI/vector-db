"""pgvector（`pgvector/pgvector:pg17`、127.0.0.1:15433）の機能別ベンチマーク実装。

自作 DB の `<=>` が内積（`crates/engine/src/kernel.rs` 参照。値が大きいほど
上位）であるため、pgvector 側も同じ指標の `<#>`（負の内積。ORDER BY 昇順で
内積の大きい順になる）に合わせる。RLS 相当は、自作 DB の現行契約（実機確認:
どのテナントの wire セッションからも `visibility = 'public'` の行のみが
可視。private 行は所有テナント自身からも不可視）に合わせ、`visibility` 列を
追加したうえで `WHERE visibility = 'public'`（tenant 列では絞り込まない）を
毎クエリに付けることで模する（`common.public_only_where` 参照）。

索引構成は `--config exact|hnsw` で切り替える。exact は索引なし全件探索、
hnsw は `vector_ip_ops` の HNSW（m=16, ef_construction=100, ef_search=64）。
"""

from __future__ import annotations

import random
import time

import psycopg
from pgvector.psycopg import register_vector

from common import (
    TENANT_VISIBLE,
    build_meta,
    load_jsonl,
    measure,
    public_only_where,
    sql_escape_literal,
    unsupported,
    vec_literal,
)

HOST = "127.0.0.1"
PORT = 15433
DBNAME = "bench"
USER = "postgres"
PASSWORD = "bench"


def _connect() -> psycopg.Connection:
    conn = psycopg.connect(
        host=HOST, port=PORT, user=USER, password=PASSWORD, dbname=DBNAME, autocommit=True
    )
    # `vector` 型は CREATE EXTENSION 後でないと register_vector が型情報を
    #引けない（実機確認: "vector type not found in the database"）ため、
    # スキーマ初期化（拡張作成）を先に行ってから型登録する。
    with conn.cursor() as cur:
        cur.execute("CREATE EXTENSION IF NOT EXISTS vector")
    register_vector(conn)
    return conn


def _setup_schema(conn: psycopg.Connection, dim: int) -> None:
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS docs")
        cur.execute(
            f"""
            CREATE TABLE docs (
                id BIGINT PRIMARY KEY,
                tenant TEXT NOT NULL,
                visibility TEXT NOT NULL,
                lang TEXT NOT NULL,
                topic TEXT NOT NULL,
                body TEXT NOT NULL,
                embedding VECTOR({dim}),
                body_tsv TSVECTOR
            )
            """
        )


def _ingest_bulk(conn: psycopg.Connection, docs: list[dict]) -> dict:
    """`COPY ... FROM STDIN (FORMAT binary)` によるバルク投入（pgvector psycopg ヘルパー使用）。"""
    t0 = time.perf_counter()
    with conn.cursor() as cur:
        with cur.copy(
            "COPY docs (id, tenant, visibility, lang, topic, body, embedding) FROM STDIN (FORMAT BINARY)"
        ) as copy:
            copy.set_types(["int8", "text", "text", "text", "text", "text", "vector"])
            for d in docs:
                copy.write_row(
                    (
                        d["id"],
                        d["tenant"],
                        # 旧フィクスチャ（visibility 未導入）との互換のため
                        # 欠落時は fail-closed で private 扱いにする。
                        d.get("visibility", "private"),
                        d["lang"],
                        d.get("topic", ""),
                        d["body"],
                        d["embedding"],
                    )
                )
        cur.execute(
            "UPDATE docs SET body_tsv = to_tsvector('english', body) WHERE body_tsv IS NULL"
        )
        cur.execute("CREATE INDEX IF NOT EXISTS docs_tsv_idx ON docs USING GIN (body_tsv)")
        cur.execute("CREATE INDEX IF NOT EXISTS docs_tenant_idx ON docs (tenant)")
        cur.execute("CREATE INDEX IF NOT EXISTS docs_visibility_idx ON docs (visibility)")
        cur.execute("CREATE INDEX IF NOT EXISTS docs_lang_idx ON docs (lang)")
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": len(docs), "seconds": elapsed, "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None}


def _ingest_single_stmt(conn: psycopg.Connection, dim: int, n_rows: int = 1000) -> dict:
    t0 = time.perf_counter()
    with conn.cursor() as cur:
        for n in range(n_rows):
            rid = 10_000_000 + n
            emb = [random.random() * 2 - 1 for _ in range(dim)]
            lang = "ja" if n % 2 == 0 else "en"
            topic = f"topic-{n % 20:02d}"
            body = f"crossdb bench ingest row {n}"
            cur.execute(
                "INSERT INTO docs (id, tenant, visibility, lang, topic, body, embedding, body_tsv) "
                "VALUES (%s, %s, %s, %s, %s, %s, %s, to_tsvector('english', %s))",
                (rid, TENANT_VISIBLE, "private", lang, topic, body, emb, body),
            )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": n_rows, "seconds": elapsed, "rows_per_sec": n_rows / elapsed if elapsed > 0 else None}


def _build_hnsw_index(conn: psycopg.Connection) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "CREATE INDEX IF NOT EXISTS docs_embedding_hnsw ON docs "
            "USING hnsw (embedding vector_ip_ops) WITH (m = 16, ef_construction = 100)"
        )
        cur.execute("SET hnsw.ef_search = 64")


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    conn = _connect()
    phases: dict = {}
    dim = len(docs[0]["embedding"]) if docs else 128

    _setup_schema(conn, dim)
    phases["ingest_bulk"] = _ingest_bulk(conn, docs)
    
    if args.config == "hnsw":
        _build_hnsw_index(conn)

    query_vecs = [vec_literal(q["embedding"]) for q in queries]
    query_texts = [sql_escape_literal(q.get("text", "")) for q in queries]

    def exec_ids(sql: str) -> list:
        with conn.cursor() as cur:
            cur.execute(sql)
            return [r[0] for r in cur.fetchall()]

    def knn(qv):
        sql = (
            f"SELECT id FROM docs WHERE {public_only_where()} "
            f"ORDER BY embedding <#> '{qv}' LIMIT 10"
        )
        return exec_ids(sql)

    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    def knn_where(qv):
        sql = (
            f"SELECT id FROM docs WHERE {public_only_where()} AND lang = 'ja' "
            f"ORDER BY embedding <#> '{qv}' LIMIT 10"
        )
        return exec_ids(sql)

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {"note": "vector_knn_where と同一クエリ形のため統合", **stats}

    def compound_count(_):
        sql = (
            f"SELECT COUNT(*) FROM docs WHERE {public_only_where()} "
            f"AND id > 100 AND lang = 'ja'"
        )
        with conn.cursor() as cur:
            cur.execute(sql)
            return cur.fetchone()[0]

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        with conn.cursor() as cur:
            cur.execute(f"SELECT COUNT(*) FROM docs WHERE {public_only_where()}")
            return cur.fetchone()[0]

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    def agg_multi(_):
        with conn.cursor() as cur:
            cur.execute(
                f"SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs "
                f"WHERE {public_only_where()}"
            )
            return cur.fetchone()

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {**stats, "result": list(last) if last else None}

    def group_by(_):
        sql = (
            f"SELECT lang, COUNT(*) AS n FROM docs WHERE {public_only_where()} "
            f"GROUP BY lang HAVING COUNT(*) > 1 ORDER BY n DESC LIMIT 5"
        )
        with conn.cursor() as cur:
            cur.execute(sql)
            return cur.fetchall()

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {**stats, "result": last}

    def hybrid(i):
        qv, qt = query_vecs[i], query_texts[i]
        vis = public_only_where()
        sql = f"""
        WITH knn AS (
            SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <#> '{qv}') AS rnk
            FROM docs WHERE {vis}
            ORDER BY embedding <#> '{qv}' LIMIT 50
        ),
        fts AS (
            SELECT id, ROW_NUMBER() OVER (
                ORDER BY ts_rank(body_tsv, plainto_tsquery('english', '{qt}')) DESC
            ) AS rnk
            FROM docs
            WHERE {vis}
              AND body_tsv @@ plainto_tsquery('english', '{qt}')
            ORDER BY ts_rank(body_tsv, plainto_tsquery('english', '{qt}')) DESC LIMIT 50
        )
        SELECT COALESCE(knn.id, fts.id) AS id,
               COALESCE(1.0 / (60 + knn.rnk), 0) + COALESCE(1.0 / (60 + fts.rnk), 0) AS score
        FROM knn FULL OUTER JOIN fts ON knn.id = fts.id
        ORDER BY score DESC LIMIT 10
        """
        return exec_ids(sql)

    idxs = list(range(len(query_vecs)))
    stats, _ = measure(hybrid, idxs)
    phases["hybrid_rrf"] = stats

    phases["mode_recall"] = unsupported("pgvector にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("pgvector にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported(
        "自作 DB の宣言的 UDF 呼び出し相当の機能が無い（plpgsql 関数は別物のため比較対象外）"
    )

    # pgvector には自作 DB の wire セッションに相当するテナント別接続の概念が
    # 無いため、tenant-b「セッション」を模す接続は作らず、同じ public-only
    # フィルタを再実行して agg_count と同値（23,000 相当）になることの一致
    # 確認とする（自作 DB は tenant-a・tenant-b いずれのセッションでも
    # `visibility = 'public'` の行のみ可視という現行契約——実機確認済み）。
    def rls_count(_):
        with conn.cursor() as cur:
            cur.execute(f"SELECT COUNT(*) FROM docs WHERE {public_only_where()}")
            return cur.fetchone()[0]

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "pgvector にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    def explain(qv):
        sql = (
            f"EXPLAIN SELECT id FROM docs WHERE {public_only_where()} "
            f"ORDER BY embedding <#> '{qv}' LIMIT 10"
        )
        with conn.cursor() as cur:
            cur.execute(sql)
            return [r[0] for r in cur.fetchall()]

    stats, last = measure(explain, query_vecs)
    phases["explain"] = {**stats, "sample_output": last}

    with conn.cursor() as cur:
        cur.execute("SELECT version()")
        pg_version = cur.fetchone()[0]
        cur.execute("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
        row = cur.fetchone()
        vector_version = row[0] if row else "unknown"

    # ingest_single_stmt はテーブルへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する
    # （self_db.py と同じ順序方針）。
    phases["ingest_single_stmt"] = _ingest_single_stmt(conn, dim)

    meta = build_meta(
        db="pgvector",
        version=f"{pg_version} / pgvector {vector_version}",
        connection="loopback TCP (psycopg)",
        config=args.config,
        rows=len(docs),
        dim=dim,
        extra={"index": "hnsw(m=16,ef_construction=100,ef_search=64)" if args.config == "hnsw" else "exact (no index)"},
    )
    conn.close()
    return {"meta": meta, "phases": phases}
