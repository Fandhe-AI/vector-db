"""MySQL 9.7.2 Community（`bench-mysql` コンテナ、127.0.0.1:33306 既定。環境変数
`CROSSDB_MYSQL_PORT` で上書き可。`containers.sh` と同じ変数を読む）の機能別
ベンチマーク実装。

実機確認済み: `VECTOR(128)` 列自体は作成・`STRING_TO_VECTOR()`/`VECTOR_TO_STRING()`
で入出力できるが、KNN 用の `DISTANCE()` 関数は存在せず
（`ERROR 1305 (42000): FUNCTION bench.DISTANCE does not exist`）、
`CREATE VECTOR INDEX ...` も構文エラー（`ERROR 1064 (42000)`）になる。
そのため本モジュールは ingest・COUNT・多重集計・GROUP BY のみを実装し、
KNN 系フェーズは fail-closed に `unsupported` として実測エラーコードを記録する。

RLS 相当は、自作 DB の現行契約（実機確認: どのテナントの wire セッションから
も `visibility = 'public'` の行のみが可視。private 行は所有テナント自身から
も不可視）に合わせ、`visibility` 列への `WHERE visibility = 'public'` で
模する（`tenant` 列では絞り込まない）。
"""

from __future__ import annotations

import time

import mysql.connector

from common import TENANT_VISIBLE, build_meta, env_port, measure, unsupported, vec_literal

HOST = "127.0.0.1"
# 既定値 33306 は containers.sh の `${CROSSDB_MYSQL_PORT:-33306}` と一致させること。
PORT = env_port("CROSSDB_MYSQL_PORT", 33306)
USER = "root"
PASSWORD = "bench"
DATABASE = "bench"


def _connect():
    return mysql.connector.connect(
        host=HOST, port=PORT, user=USER, password=PASSWORD, database=DATABASE
    )


def _setup_schema(conn, dim: int) -> None:
    cur = conn.cursor()
    cur.execute("DROP TABLE IF EXISTS docs")
    cur.execute(
        f"""
        CREATE TABLE docs (
            id BIGINT PRIMARY KEY,
            tenant VARCHAR(32),
            visibility VARCHAR(8),
            lang VARCHAR(8),
            topic VARCHAR(64),
            body TEXT,
            embedding VECTOR({dim})
        )
        """
    )
    cur.execute("CREATE INDEX docs_tenant_idx ON docs (tenant)")
    cur.execute("CREATE INDEX docs_visibility_idx ON docs (visibility)")
    cur.execute("CREATE INDEX docs_lang_idx ON docs (lang)")
    conn.commit()


def _ingest_bulk(conn, docs: list[dict]) -> dict:
    t0 = time.perf_counter()
    cur = conn.cursor()
    cur.executemany(
        "INSERT INTO docs (id, tenant, visibility, lang, topic, body, embedding) "
        "VALUES (%s, %s, %s, %s, %s, %s, STRING_TO_VECTOR(%s))",
        [
            (
                d["id"],
                d["tenant"],
                # 旧フィクスチャ（visibility 未導入）との互換のため欠落時は
                # fail-closed で private 扱いにする。
                d.get("visibility", "private"),
                d["lang"],
                d.get("topic", ""),
                d["body"],
                vec_literal(d["embedding"]),
            )
            for d in docs
        ],
    )
    conn.commit()
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": len(docs), "seconds": elapsed, "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None}


def _ingest_single_stmt(conn, dim: int, n_rows: int = 1000) -> dict:
    import random

    t0 = time.perf_counter()
    cur = conn.cursor()
    for n in range(n_rows):
        rid = 10_000_000 + n
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        body = f"crossdb bench ingest row {n}"
        emb = vec_literal([random.random() * 2 - 1 for _ in range(dim)])
        cur.execute(
            "INSERT INTO docs (id, tenant, visibility, lang, topic, body, embedding) "
            "VALUES (%s, %s, %s, %s, %s, %s, STRING_TO_VECTOR(%s))",
            (rid, TENANT_VISIBLE, "private", lang, topic, body, emb),
        )
        # pgvector（autocommit）・self（文ごとに永続化）と粒度を揃え、1 文ごとに commit する。
        conn.commit()
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": n_rows,
        "seconds": elapsed,
        "rows_per_sec": n_rows / elapsed if elapsed > 0 else None,
        "commit_granularity": "per_statement",
    }


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    conn = _connect()
    dim = len(docs[0]["embedding"]) if docs else 128
    _setup_schema(conn, dim)
    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(conn, docs)

    knn_reason = (
        "VECTOR 型は作成できるが KNN 用の DISTANCE() 関数が存在しない"
        "（実機確認: ERROR 1305 (42000): FUNCTION bench.DISTANCE does not exist）"
    )
    index_reason = (
        "CREATE VECTOR INDEX は構文エラー"
        "（実機確認: ERROR 1064 (42000): syntax error near 'VECTOR INDEX ...'）"
    )
    phases["vector_knn"] = unsupported(knn_reason)
    phases["vector_knn_where"] = unsupported(knn_reason)
    phases["point_where"] = unsupported(knn_reason)
    phases["hybrid_rrf"] = unsupported(knn_reason + "（hybrid は KNN 側が成立しないため対象外）")
    phases["mode_recall"] = unsupported("MySQL にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("MySQL にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported("自作 DB の宣言的 UDF 呼び出し相当の機能が無い")
    phases["_ann_index_note"] = index_reason

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
        row = cur.fetchone()
        return [float(x) if x is not None else None for x in row]

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {**stats, "result": last}

    def group_by(_):
        cur = conn.cursor()
        cur.execute(
            "SELECT lang, COUNT(*) AS n FROM docs WHERE visibility = 'public' "
            "GROUP BY lang HAVING n > 1 ORDER BY n DESC LIMIT 5"
        )
        return cur.fetchall()

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {**stats, "result": last}

    # --- 広域取得（bulk fetch） ---
    # KNN 関数が存在しないため類似度順の Top-N は成立せず、既存の理由をそのまま流用する。
    # ORDER BY なしの WHERE スキャンのみ実行可能（id と body を返し本文送出コストを含める）。
    phases["bulk_knn_k200"] = unsupported(knn_reason)
    phases["bulk_knn_k1000"] = unsupported(knn_reason)
    phases["bulk_knn_where_k200"] = unsupported(knn_reason)
    phases["bulk_hybrid_k200"] = unsupported(knn_reason + "（hybrid は KNN 側が成立しないため対象外）")

    def scan_nosort(_):
        cur = conn.cursor()
        cur.execute(
            "SELECT id, body FROM docs WHERE visibility = 'public' AND lang = 'ja' LIMIT 500"
        )
        return cur.fetchall()

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

    # MySQL には自作 DB の wire セッションに相当するテナント別接続の概念が
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
        "note": "MySQL にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    def explain(_):
        cur = conn.cursor()
        cur.execute("EXPLAIN SELECT id FROM docs WHERE visibility = 'public' AND lang = 'ja'")
        return cur.fetchall()

    stats, last = measure(explain, [None])
    phases["explain"] = {**stats, "sample_output": last}

    cur = conn.cursor()
    cur.execute("SELECT VERSION()")
    version = cur.fetchone()[0]

    # ingest_single_stmt はテーブルへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する
    # （self_db.py と同じ順序方針）。
    phases["ingest_single_stmt"] = _ingest_single_stmt(conn, dim)

    meta = build_meta(
        db="mysql",
        version=f"MySQL {version}",
        connection="loopback TCP (mysql-connector-python)",
        config="none (ANN 索引・KNN 関数が存在しないため索引構成の区別なし)",
        rows=len(docs),
        dim=dim,
    )
    conn.close()
    return {"meta": meta, "phases": phases}
