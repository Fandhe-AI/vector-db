#!/usr/bin/env python3
"""crossdb_bench のエントリポイント。

`--db` で選んだモジュール（self_db.py 等）へフィクスチャを渡して全フェーズを
実行させ、`vector_knn` フェーズの返却 id を ground truth（recall.py）と
突き合わせて `recall_at_10` を計算したうえで `<out-dir>/<db>_<config>.json`
へ書き出す。

対照 DB のコンテナ起動・停止は `containers.sh up|down <db>` に分離してある
（このスクリプトは「既にコンテナが起動している」ことを前提にする。self は
wire-server 子プロセスを db モジュール側で直接起動・停止する）。

使い方:
    python run.py --db self --config exact \\
        --rows-file $S/docs25k.redb --queries-file $S/queries200.jsonl
    python run.py --db pgvector --config hnsw \\
        --rows-file $S/docs25k.jsonl --queries-file $S/queries200.jsonl
"""

from __future__ import annotations

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from common import load_jsonl, write_result  # noqa: E402
from recall import build_ground_truth, recall_at_k, recall_at_k_tie_tolerant  # noqa: E402

DB_MODULES = ["self", "pgvector", "sqlite_vec", "qdrant", "lancedb", "mysql"]


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="crossdb_bench: 機能別ベンチマーク実行")
    p.add_argument("--db", required=True, choices=DB_MODULES)
    p.add_argument("--config", required=True, choices=["exact", "hnsw"])
    p.add_argument(
        "--rows-file",
        required=True,
        help="self の場合は redb ファイルパス。他 DB は docs jsonl（docs25k.jsonl 等）パス",
    )
    p.add_argument("--queries-file", required=True, help="queries200.jsonl のパス")
    p.add_argument(
        "--docs-file",
        default=None,
        help="recall ground truth 計算用の docs jsonl（省略時は self 以外は --rows-file と同じ、"
        "self は --rows-file と同じディレクトリの docs25k.jsonl を既定で探す）",
    )
    p.add_argument("--out-dir", default=None, help="結果 JSON の出力先（既定: <queries-file と同じディレクトリ>/results）")
    p.add_argument("--workdir", default=None, help="作業用一時ディレクトリ（既定: --rows-file と同じディレクトリ）")
    p.add_argument(
        "--expect-dim",
        type=int,
        default=None,
        help="docs／queries の埋め込み長がこの値と一致することを要求する（Issue #466。"
        "dim 別 fixture を取り違えたまま計測を続けさせず、不一致は非 0 終了で拒否する）",
    )
    return p.parse_args()


def resolve_docs_file(args: argparse.Namespace) -> str:
    """recall ground truth 計算用の docs jsonl パスを決める。

    self は `--rows-file`（redb）と対になる docs jsonl を `--docs-file` 省略時に
    自動探索する。dim 別 fixture（`docs25k-d768.redb` 等）でも見つけられるよう、
    固定名 `docs25k.jsonl` ではなく `--rows-file` と同じ basename（拡張子のみ
    `.jsonl` へ）を使う（Issue #466。`docs25k.redb` → `docs25k.jsonl` は従来どおり
    同じ結果になるため後方互換）。
    """
    if args.docs_file:
        return args.docs_file
    if args.db == "self":
        base = os.path.splitext(os.path.basename(args.rows_file))[0]
        candidate = os.path.join(os.path.dirname(os.path.abspath(args.rows_file)), f"{base}.jsonl")
        return candidate
    return args.rows_file


def _peek_embedding_dim(jsonl_path: str) -> int | None:
    """`jsonl_path`（docs25k.jsonl 等）の先頭 1 行だけを読み `embedding` の長さを
    返す（Issue #466。dim 一致検査のためだけに数万行のフィクスチャ全体を
    `load_jsonl` で読み込むのは dim=768／1536 で数百 MB になり無駄なため、
    行単位で最初の非空行のみをパースする）。"""
    import json as _json

    with open(jsonl_path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            doc = _json.loads(line)
            embedding = doc.get("embedding")
            return len(embedding) if embedding is not None else None
    return None


def main() -> int:
    args = parse_args()
    if args.out_dir is None:
        args.out_dir = os.path.join(os.path.dirname(os.path.abspath(args.queries_file)), "results")
    if args.workdir is None:
        args.workdir = os.path.dirname(os.path.abspath(args.rows_file))

    queries = load_jsonl(args.queries_file)

    # docs／queries の埋め込み長が食い違ったまま計測を続けると、dim 別 fixture
    # の取り違え（例: dim=128 の docs に dim=768 の queries）を検出できないまま
    # 不正な結果 JSON を書き出してしまう。self は queries と `--docs-file`（省略時
    # は `resolve_docs_file`）の docs を、他 DB は `--rows-file`（docs 本体）を
    # 突き合わせる（Issue #466。unsupported へ丸めず非 0 終了で拒否する）。
    dim_source_docs = resolve_docs_file(args) if args.db == "self" else args.rows_file
    query_dim = len(queries[0]["embedding"]) if queries else None
    docs_dim = None
    if os.path.exists(dim_source_docs):
        docs_dim = _peek_embedding_dim(dim_source_docs)
    if query_dim is not None and docs_dim is not None and query_dim != docs_dim:
        print(
            f"error: embedding dim mismatch between docs ({docs_dim}) and queries ({query_dim})",
            file=sys.stderr,
        )
        return 1
    if args.expect_dim is not None:
        actual_dim = query_dim if query_dim is not None else docs_dim
        if actual_dim != args.expect_dim:
            print(
                f"error: --expect-dim {args.expect_dim} does not match actual dim {actual_dim}",
                file=sys.stderr,
            )
            return 1

    if args.db == "self":
        import self_db

        result = self_db.run(args, queries)
    else:
        docs = load_jsonl(args.rows_file)
        module = {
            "pgvector": "pgvector_db",
            "sqlite_vec": "sqlite_vec_db",
            "qdrant": "qdrant_db",
            "lancedb": "lancedb_db",
            "mysql": "mysql_db",
        }[args.db]
        db_mod = __import__(module)
        result = db_mod.run(args, docs, queries)

    meta = result["meta"]
    phases = result["phases"]

    # --- recall_at_10: vector_knn フェーズが ids_per_query を返していれば ground truth と照合 ---
    docs_file = resolve_docs_file(args)
    knn_phase = phases.get("vector_knn", {})
    ids_per_query = knn_phase.get("ids_per_query") if isinstance(knn_phase, dict) else None
    if ids_per_query and os.path.exists(docs_file):
        strict_top_k, tie_boundaries = build_ground_truth(docs_file, queries, k=10)
        recall_strict = recall_at_k(ids_per_query, strict_top_k)
        recall_tie = recall_at_k_tie_tolerant(ids_per_query, tie_boundaries, k=10)
        # recall_at_10 は同点許容版を主指標とする（同点境界の順序差で exact
        # 構成でも 1.0 にならない問題を解消するため）。従来の厳密一致値は
        # recall_at_10_strict として併記する。
        phases["recall_at_10"] = {
            "recall_at_10": recall_tie,
            "recall_at_10_strict": recall_strict,
            "queries": len(tie_boundaries),
        }
    else:
        phases["recall_at_10"] = {
            "unsupported": True,
            "reason": "vector_knn が unsupported、または docs ファイルが見つからない",
        }

    # ids_per_query は正解照合専用の中間データであり、結果 JSON を肥大化させるため保存しない。
    if isinstance(knn_phase, dict) and "ids_per_query" in knn_phase:
        del knn_phase["ids_per_query"]

    out_path = write_result(args.out_dir, args.db, args.config, meta, phases)
    print(f"wrote: {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
