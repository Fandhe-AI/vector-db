"""MongoDB（`mongo:8`、mongot 非搭載＝ベクトル検索機能を持たない「素の NoSQL」代表）の
機能別ベンチマーク実装。127.0.0.1:37018 既定。環境変数 `CROSSDB_MONGODB_PLAIN_PORT` で
上書き可（`containers.sh` と同じ変数名を読む契約は他モジュールに合わせる）。

pymongo 4.18.1 を使う。Atlas Search（mongot）を同梱しない Community イメージのため、
`$vectorSearch`・`$search`（全文検索）はいずれも使えない。ベクトル検索・ハイブリッド
検索・モード切替・UDF 呼び出しはすべて unsupported とし、代わりに集約パイプライン
（`$zip`＋`$reduce` による内積計算・`$sort`・`$limit`）で「索引を使わない総当たり
KNN」を `vector_knn_pipeline_bruteforce` フェーズとして計測する（DB ネイティブの
ベクトル検索機能ではない旨をフェーズ内の note に明記する）。

RLS 相当は他モジュールと同じく `visibility == "public"` フィールドへの素朴な
フィルタで模する（`tenant` では絞り込まない。`common.doc_visibility` を投入側で使う）。
"""

from __future__ import annotations

import os
import random
import sys
import time

from pymongo import MongoClient
from pymongo.collection import Collection

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from common import TENANT_VISIBLE, build_meta, doc_visibility, env_port, measure, unsupported
from recall import build_ground_truth, recall_at_k, recall_at_k_tie_tolerant

HOST = "127.0.0.1"
# containers.sh 側の `${CROSSDB_MONGODB_PLAIN_PORT:-37018}` と一致させること。
PORT = env_port("CROSSDB_MONGODB_PLAIN_PORT", 37018)
DB_NAME = "mongodb_plain"
COLLECTION = "docs"


def _connect() -> MongoClient:
    return MongoClient(f"mongodb://{HOST}:{PORT}/", serverSelectionTimeoutMS=10_000)


def _setup_collection(client: MongoClient, config: str) -> Collection:
    if config != "exact":
        # mongot（Atlas Search）非搭載イメージのため ANN 索引（$vectorSearch 相当）を
        # 一切構築できない。hnsw 構成の要求は「対応する DB が別にある」前提で
        # fail-closed に拒否し、無索引のまま黙って exact 相当を返すことを避ける。
        raise ValueError(
            "mongodb_plain (mongot 非搭載イメージ) は hnsw 構成（ベクトル ANN 索引）をサポートしない"
        )
    db = client[DB_NAME]
    db.drop_collection(COLLECTION)
    coll = db[COLLECTION]
    # visibility・lang・topic の複合/単独索引（task 指示）。where_compound_count・
    # scan_where_nosort_k500 は visibility+lang の複合索引で絞り込める形にする。
    coll.create_index("visibility")
    coll.create_index("lang")
    coll.create_index("topic")
    coll.create_index([("visibility", 1), ("lang", 1)])
    coll.create_index([("visibility", 1), ("topic", 1)])
    return coll


def _ingest_bulk(coll: Collection, docs: list[dict]) -> dict:
    t0 = time.perf_counter()
    batch = 2000
    for i in range(0, len(docs), batch):
        chunk = docs[i : i + batch]
        coll.insert_many(
            [
                {
                    "_id": d["id"],
                    "id": d["id"],
                    "tenant": d["tenant"],
                    # 旧フィクスチャ（visibility 未導入）との互換のため fail-closed で
                    # private 扱いにする（common.doc_visibility を投入側・正解生成側で共有）。
                    "visibility": doc_visibility(d),
                    "lang": d["lang"],
                    "topic": d.get("topic", ""),
                    "body": d["body"],
                    "embedding": d["embedding"],
                }
                for d in chunk
            ],
            ordered=False,
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": len(docs),
        "seconds": elapsed,
        "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None,
    }


def _ingest_single_stmt(coll: Collection, dim: int, n_rows: int = 1000) -> dict:
    t0 = time.perf_counter()
    for n in range(n_rows):
        rid = 10_000_000 + n
        emb = [random.random() * 2 - 1 for _ in range(dim)]
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        # MongoDB は既定で w:1（プライマリ確認まで待つ durable write）。insert_one を
        # 1,000 回逐次呼び出すことで自作 DB の単文 INSERT ×1,000 と同じ計測条件にする。
        coll.insert_one(
            {
                "_id": rid,
                "id": rid,
                "tenant": TENANT_VISIBLE,
                "visibility": "private",
                "lang": lang,
                "topic": topic,
                "body": f"crossdb bench ingest row {n}",
                "embedding": emb,
            }
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": n_rows,
        "seconds": elapsed,
        "rows_per_sec": n_rows / elapsed if elapsed > 0 else None,
    }


def _dot_score_stage(qv: list[float]) -> dict:
    """`$zip` で `embedding` とクエリベクトル（定数配列）を要素ペア化し、`$reduce`
    で積和（内積）を計算する集約ステージを組み立てる（索引を使わない総当たり
    KNN。DB ネイティブのベクトル検索機能ではない）。"""
    return {
        "$addFields": {
            "score": {
                "$reduce": {
                    "input": {"$zip": {"inputs": ["$embedding", [float(x) for x in qv]]}},
                    "initialValue": 0.0,
                    "in": {
                        "$add": [
                            "$$value",
                            {"$multiply": [{"$arrayElemAt": ["$$this", 0]}, {"$arrayElemAt": ["$$this", 1]}]},
                        ]
                    },
                }
            }
        }
    }


def _bruteforce_knn(coll: Collection, qv: list[float], k: int = 10) -> list[int]:
    pipeline = [
        {"$match": {"visibility": "public"}},
        _dot_score_stage(qv),
        {"$sort": {"score": -1}},
        {"$limit": k},
        {"$project": {"_id": 0, "id": 1}},
    ]
    return [d["id"] for d in coll.aggregate(pipeline, allowDiskUse=True)]


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    client = _connect()
    dim = len(docs[0]["embedding"]) if docs else 128
    coll = _setup_collection(client, args.config)
    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(coll, docs)

    query_vecs = [q["embedding"] for q in queries]

    # --- ベクトル検索機能なし: vector_knn・vector_knn_where・bulk_knn_*・hybrid_rrf・
    # bulk_hybrid_k200・mode_*・udf_call はすべて unsupported ---
    no_vector_search = unsupported(
        "mongot（Atlas Search）非搭載イメージ（mongo:8）のため $vectorSearch が使えず、"
        "ベクトル検索機能そのものが無い"
    )
    phases["vector_knn"] = no_vector_search
    phases["vector_knn_where"] = no_vector_search
    phases["point_where"] = {
        "note": "vector_knn_where と同一クエリ形のため統合",
        **no_vector_search,
    }
    phases["bulk_knn_k200"] = no_vector_search
    phases["bulk_knn_k1000"] = no_vector_search
    phases["bulk_knn_where_k200"] = no_vector_search
    phases["bulk_hybrid_k200"] = unsupported(
        "ベクトル検索機能が無いため hybrid（ベクトル+全文検索の RRF 融合）も構成不能"
    )
    phases["hybrid_rrf"] = unsupported(
        "ベクトル検索機能が無く、かつ mongot（Atlas Search）非搭載のため全文検索 $search も使えない"
        "（DB ネイティブ hybrid/RRF 機能を計測する task 指示のため対象外）"
    )
    phases["mode_recall"] = unsupported("MongoDB にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("MongoDB にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported("自作 DB の宣言的 UDF 呼び出し相当の機能が無い")

    # --- vector_knn_pipeline_bruteforce: $vectorSearch の代替としての集約パイプライン
    # 総当たり KNN（ベクトル検索機能ではない。索引を一切使わず毎回全行を走査する）。
    def knn_bruteforce(qv):
        return _bruteforce_knn(coll, qv, k=10)

    stats, _ = measure(knn_bruteforce, query_vecs)
    knn_ids_all = [knn_bruteforce(qv) for qv in query_vecs]
    strict_top_k, tie_boundaries = build_ground_truth(args.rows_file, queries, k=10)
    recall_strict = recall_at_k(knn_ids_all, strict_top_k)
    recall_tie = recall_at_k_tie_tolerant(knn_ids_all, tie_boundaries, k=10)
    phases["vector_knn_pipeline_bruteforce"] = {
        **stats,
        "k": 10,
        "recall_at_10_self_computed": recall_tie,
        "recall_at_10_strict_self_computed": recall_strict,
        "note": (
            "$vectorSearch ではなく $zip+$reduce による内積の集約パイプライン（索引を使わない"
            "総当たり全件走査）。DB ネイティブのベクトル検索機能ではない。recall は run.py の"
            "vector_knn ゲート対象外のためここで自己計算する"
        ),
    }

    # --- where_compound_count: pgvector_db.py と同一条件（visibility=public AND
    # lang='ja' AND id>100）---
    def compound_count(_):
        return coll.count_documents({"visibility": "public", "lang": "ja", "id": {"$gt": 100}})

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        return coll.count_documents({"visibility": "public"})

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    def agg_multi(_):
        pipeline = [
            {"$match": {"visibility": "public"}},
            {
                "$group": {
                    "_id": None,
                    "count": {"$sum": 1},
                    "sum_id": {"$sum": "$id"},
                    "avg_id": {"$avg": "$id"},
                    "min_id": {"$min": "$id"},
                    "max_id": {"$max": "$id"},
                }
            },
        ]
        res = list(coll.aggregate(pipeline))
        return res[0] if res else None

    stats, last = measure(agg_multi, [None])
    if last is not None:
        last = {k: v for k, v in last.items() if k != "_id"}
    phases["agg_multi"] = {**stats, "result": last}

    def group_by(_):
        pipeline = [
            {"$match": {"visibility": "public"}},
            {"$group": {"_id": "$lang", "count": {"$sum": 1}}},
            {"$match": {"count": {"$gt": 1}}},
            {"$sort": {"count": -1}},
            {"$limit": 5},
        ]
        return list(coll.aggregate(pipeline))

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {**stats, "result": [(d["_id"], d["count"]) for d in last] if last else []}

    # --- scan_where_nosort_k500: ソートなし visibility=public AND lang='ja' 500 件 ---
    def scan_nosort(_):
        cursor = coll.find(
            {"visibility": "public", "lang": "ja"},
            {"_id": 0, "id": 1, "body": 1},
        ).limit(500)
        return [(d["id"], d.get("body")) for d in cursor]

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

    # MongoDB にはテナント別セッション（wire セッション相当）の概念が無いため、
    # 自作 DB の RLS 相当契約（どのテナントからも public のみ可視。実機確認済み）を
    # 同じ public-only フィルタの再実行で模する（agg_count と同値になる一致確認）。
    def rls_count(_):
        return coll.count_documents({"visibility": "public"})

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "MongoDB にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    # --- explain: find().explain() で実行計画を取得（計測対象はコマンド呼び出し自体） ---
    def explain_query(_):
        return coll.find({"visibility": "public", "lang": "ja"}).limit(500).explain()

    stats, last = measure(explain_query, [None])
    winning_plan = None
    if isinstance(last, dict):
        qp = last.get("queryPlanner", {})
        winning_plan = qp.get("winningPlan", {}).get("stage")
    phases["explain"] = {**stats, "winning_plan_stage": winning_plan}

    # ingest_single_stmt はテーブルへ合成行を追加するため、他フェーズ（特に
    # vector_knn_pipeline_bruteforce の recall 検算）を汚染しないよう最後に実行する。
    phases["ingest_single_stmt"] = _ingest_single_stmt(coll, dim)

    server_version = client.server_info().get("version", "unknown")
    row_count = coll.count_documents({})
    meta = build_meta(
        db="mongodb_plain",
        version=f"mongodb {server_version} (image mongo:8, mongot 非搭載)",
        connection="pymongo (loopback TCP)",
        config=args.config,
        rows=len(docs),
        dim=dim,
        extra={
            "distance": "dot product ($zip+$reduce による自前計算。ネイティブベクトル検索なし)",
            "indexes": ["visibility", "lang", "topic", "(visibility,lang)", "(visibility,topic)"],
            "row_count_after_ingest": row_count,
        },
    )
    client.close()
    return {"meta": meta, "phases": phases}
