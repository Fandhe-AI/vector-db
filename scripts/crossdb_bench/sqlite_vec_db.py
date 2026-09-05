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

FTS5 拡張が同梱されていないビルドの SQLite では `docs_fts` の作成自体が
`no such module: fts5` で失敗する。この場合はスキーマ作成時点で検出し
（`_setup_schema` の戻り値 `fts5_available`）、FTS テーブルの作成・投入を
省略したうえで hybrid 系フェーズ（`hybrid_rrf`・`bulk_hybrid_k200`）だけを
unsupported として記録する。他フェーズは通常どおり計測する（codex-review P2）。
"""

from __future__ import annotations

import os
import shutil
import sqlite3
import tempfile
import time

import sqlite_vec

from common import (
    doc_visibility,
    TENANT_VISIBLE,
    build_meta,
    measure,
    unsupported,
)


def _connect(db_path: str) -> sqlite3.Connection:
    """ファイルベースの sqlite DB へ接続する（永続ストレージ前提の他 DB と
    投入速度の比較条件を揃えるため in-memory ではなくファイルを使う。
    codex-review P2 指摘）。PRAGMA は変更せず SQLite の既定
    （`journal_mode=DELETE`・`synchronous=FULL` 相当）のまま運用し、他 DB の
    既定 durability 設定と揃える意図を維持する。"""
    conn = sqlite3.connect(db_path)
    conn.enable_load_extension(True)
    sqlite_vec.load(conn)
    conn.enable_load_extension(False)
    return conn


def _pack(vec: list[float]) -> bytes:
    return sqlite_vec.serialize_float32(vec)


def _is_unsupported_fts5_error(e: sqlite3.Error) -> bool:
    """FTS5/vec0 の機能未対応を示す `sqlite3.OperationalError` か判定する
    （実機で確認済みの文言: FTS5 拡張未同梱の "no such module: fts5"、MATCH
    構文が受理されない "fts5: syntax error"、vec0 auxiliary 列を KNN の
    WHERE に使った "illegal where constraint"）。ロック・I/O 障害等の実行
    障害まで unsupported へ丸めて成功扱いにしない（codex-review P1）ため、
    これらに一致しない例外は呼び出し元で再送出する。"""
    msg = str(e).lower()
    unsupported_markers = (
        "no such module: fts5",
        "fts5: syntax error",
        "illegal where constraint",
    )
    return any(marker in msg for marker in unsupported_markers)


def _setup_schema(conn: sqlite3.Connection, dim: int) -> bool:
    """スキーマを作成し、FTS5 が使えたかどうかを返す。

    リンクされた SQLite に FTS5 拡張が同梱されていない環境では
    `CREATE VIRTUAL TABLE ... USING fts5(...)` 自体が `no such module: fts5`
    で失敗する。この失敗をここで検出できないと計測全体が例外で終了する
    （`_is_unsupported_fts5_error` は従来 hybrid フェーズでしか呼ばれておらず
    到達不能だった。codex-review P2）ため、スキーマ作成時に検出し、未搭載
    なら FTS テーブルの作成・投入を省略して呼び出し元へ知らせる。"""
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
    fts5_available = True
    try:
        cur.execute("CREATE VIRTUAL TABLE docs_fts USING fts5(body, content='docs', content_rowid='id')")
    except sqlite3.OperationalError as e:
        if not _is_unsupported_fts5_error(e):
            raise
        fts5_available = False
    conn.commit()
    return fts5_available


def _ingest_bulk(conn: sqlite3.Connection, docs: list[dict], fts5_available: bool) -> dict:
    t0 = time.perf_counter()
    cur = conn.cursor()
    cur.executemany(
        "INSERT INTO docs (id, tenant, visibility, lang, topic, body) VALUES (?, ?, ?, ?, ?, ?)",
        [
            # 旧フィクスチャ（visibility 未導入）との互換のため欠落時は
            # fail-closed で private 扱いにする。
            (d["id"], d["tenant"], doc_visibility(d), d["lang"], d.get("topic", ""), d["body"])
            for d in docs
        ],
    )
    cur.executemany(
        "INSERT INTO vec_docs (rowid, embedding, visibility, tenant, lang, topic) VALUES (?, ?, ?, ?, ?, ?)",
        [
            (
                d["id"],
                _pack(d["embedding"]),
                doc_visibility(d),
                d["tenant"],
                d["lang"],
                d.get("topic", ""),
            )
            for d in docs
        ],
    )
    if fts5_available:
        cur.executemany(
            "INSERT INTO docs_fts (rowid, body) VALUES (?, ?)",
            [(d["id"], d["body"]) for d in docs],
        )
    conn.commit()
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": len(docs), "seconds": elapsed, "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None}


def _ingest_single_stmt(
    conn: sqlite3.Connection, dim: int, fts5_available: bool, n_rows: int = 1000
) -> dict:
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
        # FTS5 未搭載時は `docs_fts` 自体が無い（`_setup_schema`）ため FTS への投入を
        # 省略し、hybrid 以外のフェーズは継続する（`_ingest_bulk` と同じ契約）。
        if fts5_available:
            cur.execute("INSERT INTO docs_fts (rowid, body) VALUES (?, ?)", (rid, body))
        # pgvector（autocommit）・self（文ごとに永続化）と粒度を揃え、1 行（vec0 + FTS の
        # 2 文）ごとに commit する。
        conn.commit()
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": n_rows,
        "seconds": elapsed,
        "rows_per_sec": n_rows / elapsed if elapsed > 0 else None,
        "commit_granularity": "per_row",
    }


def _fts5_quote(text: str) -> str:
    """自然文を FTS5 の語句クエリへ変換する（各語を二重引用符で囲み、内部の引用符は
    二重化。空文字なら 1 件も一致しない語句を返す）。"""
    terms = [t for t in text.split() if t]
    if not terms:
        return '""'
    return " ".join('"' + t.replace('"', '""') + '"' for t in terms)


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    # 他 DB（永続ストレージ）と条件を揃えるため、`--workdir` 配下の専用一時
    # ディレクトリにファイルベースの DB を作る。終了時（正常・異常いずれも）
    # 削除する（codex-review P2 指摘）。
    workdir = getattr(args, "workdir", None) or tempfile.gettempdir()
    os.makedirs(workdir, exist_ok=True)
    run_dir = tempfile.mkdtemp(prefix=f"sqlite_vec_bench_{os.getpid()}_", dir=workdir)
    db_path = os.path.join(run_dir, "sqlite_vec_bench.db")
    try:
        return _run_with_db(db_path, docs, queries)
    finally:
        shutil.rmtree(run_dir, ignore_errors=True)


def _run_with_db(db_path: str, docs: list[dict], queries: list[dict]) -> dict:
    conn = _connect(db_path)
    dim = len(docs[0]["embedding"]) if docs else 128
    fts5_available = _setup_schema(conn, dim)
    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(conn, docs, fts5_available)

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
        # 自然文をそのまま MATCH に渡すとピリオド等が FTS5 構文として解釈され構文エラーに
        # なる。各語を二重引用符で囲んだ語句の並び（暗黙 AND）へ変換し、失敗は握りつぶさず
        # 伝播させる（呼び出し側でフェーズ全体を理由付き unsupported にする）。
        cur.execute(
            "SELECT docs_fts.rowid FROM docs_fts JOIN docs ON docs.id = docs_fts.rowid "
            "WHERE docs_fts MATCH ? AND docs.visibility = 'public' ORDER BY bm25(docs_fts) LIMIT 50",
            (_fts5_quote(qt),),
        )
        fts_ids = [r[0] for r in cur.fetchall()]
        # RRF(k=60) を Python 側で計算（sqlite-vec/FTS5 にネイティブ融合機能は無い）。
        scores: dict[int, float] = {}
        for rank, rid in enumerate(knn_ids, start=1):
            scores[rid] = scores.get(rid, 0.0) + 1.0 / (60 + rank)
        for rank, rid in enumerate(fts_ids, start=1):
            scores[rid] = scores.get(rid, 0.0) + 1.0 / (60 + rank)
        top = sorted(scores.items(), key=lambda kv: -kv[1])[:10]
        return [rid for rid, _ in top]

    idxs = list(range(len(query_vecs)))
    if not fts5_available:
        # スキーマ作成時点で FTS5 未搭載と判明済み（`docs_fts` 自体が存在しない）
        # ため hybrid フェーズだけを unsupported にし、他フェーズは通常どおり
        # 計測する（codex-review P2）。
        phases["hybrid_rrf"] = unsupported("FTS5 module not available (no such module: fts5)")
    else:
        try:
            stats, _ = measure(hybrid, idxs)
            phases["hybrid_rrf"] = stats
        except sqlite3.Error as e:  # 機能未対応のみ捕捉し、それ以外は再送出する
            if not _is_unsupported_fts5_error(e):
                raise
            phases["hybrid_rrf"] = unsupported(f"hybrid (FTS5 MATCH) failed: {e!r}")

    # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
    # LLM のコンテキストへ丸ごと渡す想定のため本文（body）の送出コストを含める。
    # vec0 の KNN（MATCH + k）はサブクエリに閉じ込め、body は外側で docs と JOIN する
    # （MATCH を含む FROM へ直接 JOIN すると計画が KNN でなくなり得るため）。
    def bulk_knn(k: int, lang_ja: bool):
        lang_clause = " AND lang = 'ja'" if lang_ja else ""

        def _run(qv):
            cur = conn.cursor()
            cur.execute(
                "SELECT v.rowid, docs.body FROM ("
                "SELECT rowid, distance FROM vec_docs WHERE embedding MATCH ? AND k = ? "
                f"AND visibility = 'public'{lang_clause}"
                ") AS v JOIN docs ON docs.id = v.rowid ORDER BY v.distance",
                (_pack(qv), k),
            )
            return cur.fetchall()

        return _run

    for k in (200, 1000):
        stats, last = measure(bulk_knn(k, lang_ja=False), query_vecs)
        phases[f"bulk_knn_k{k}"] = {**stats, "k": k, "rows_returned": len(last)}

    stats, last = measure(bulk_knn(200, lang_ja=True), query_vecs)
    phases["bulk_knn_where_k200"] = {**stats, "k": 200, "rows_returned": len(last)}

    # hybrid_rrf と同じ RRF 形。候補プールが各 50 のままでは融合後の総数が最大 100 に
    # とどまり Top-200 を満たせないため両プールを 200 へ広げ、融合後に body を 1 回の
    # `IN (...)` で取り出す（本文取得コストも計測区間に含める）。
    bulk_hybrid_pool = 200

    def bulk_hybrid(i):
        qv = query_vecs[i]
        qt = queries[i].get("text", "")
        cur = conn.cursor()
        cur.execute(
            "SELECT rowid FROM vec_docs WHERE embedding MATCH ? AND k = ? "
            "AND visibility = 'public' ORDER BY distance",
            (_pack(qv), bulk_hybrid_pool),
        )
        knn_ids = [r[0] for r in cur.fetchall()]
        cur.execute(
            "SELECT docs_fts.rowid FROM docs_fts JOIN docs ON docs.id = docs_fts.rowid "
            "WHERE docs_fts MATCH ? AND docs.visibility = 'public' ORDER BY bm25(docs_fts) LIMIT ?",
            (_fts5_quote(qt), bulk_hybrid_pool),
        )
        fts_ids = [r[0] for r in cur.fetchall()]
        scores: dict[int, float] = {}
        for rank, rid in enumerate(knn_ids, start=1):
            scores[rid] = scores.get(rid, 0.0) + 1.0 / (60 + rank)
        for rank, rid in enumerate(fts_ids, start=1):
            scores[rid] = scores.get(rid, 0.0) + 1.0 / (60 + rank)
        top = [rid for rid, _ in sorted(scores.items(), key=lambda kv: -kv[1])[:200]]
        if not top:
            return []
        placeholders = ",".join("?" for _ in top)
        cur.execute(f"SELECT id, body FROM docs WHERE id IN ({placeholders})", top)
        body_by_id = {r[0]: r[1] for r in cur.fetchall()}
        return [(rid, body_by_id.get(rid)) for rid in top]

    if not fts5_available:
        phases["bulk_hybrid_k200"] = unsupported("FTS5 module not available (no such module: fts5)")
    else:
        try:
            stats, last = measure(bulk_hybrid, idxs)
            phases["bulk_hybrid_k200"] = {
                **stats,
                "k": 200,
                "rows_returned": len(last),
                "candidate_pool": bulk_hybrid_pool,
            }
        except sqlite3.Error as e:  # 機能未対応のみ捕捉し、それ以外は再送出する
            if not _is_unsupported_fts5_error(e):
                raise
            phases["bulk_hybrid_k200"] = unsupported(f"hybrid (FTS5 MATCH) failed: {e!r}")

    def scan_nosort(_):
        cur = conn.cursor()
        cur.execute(
            "SELECT id, body FROM docs WHERE visibility = 'public' AND lang = 'ja' LIMIT 500"
        )
        return cur.fetchall()

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

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
    journal_mode = cur.execute("PRAGMA journal_mode").fetchone()[0]
    synchronous = cur.execute("PRAGMA synchronous").fetchone()[0]

    # ingest_single_stmt はテーブルへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する
    # （self_db.py と同じ順序方針）。
    phases["ingest_single_stmt"] = _ingest_single_stmt(conn, dim, fts5_available)

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
            "storage": "file",
            "journal_mode": journal_mode,
            "synchronous": synchronous,
            "fts5_available": fts5_available,
        },
    )
    conn.close()
    return {"meta": meta, "phases": phases}
