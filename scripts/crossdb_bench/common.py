"""crossdb_bench 共通ユーティリティ。

自作ベクトル DB（wire-server 経由。以下 "self"）と他 DB（pgvector・sqlite-vec・
Qdrant・LanceDB・MySQL）の機能別ベンチマークを横並びで行うための共通部品。
各 DB 専用モジュール（self_db.py・pgvector_db.py 等）がこれを import して使う。

private spec（docs/spec）の内容はここでは参照・転記しない。数値基準（warmup・
反復回数）は本ハーネス独自の実装既定値であり、`crates/engine/examples/feature_bench.rs`
の WARMUP=5・ITERS=50 に合わせてある（比較可能性のため）。
"""

from __future__ import annotations

import json
import os
import platform
import subprocess
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable, Sequence

# feature_bench.rs と同一の反復回数（比較可能性のため合わせる。本リポ独自の既定値）。
WARMUP = 5
ITERS = 50

# 全 DB 共通の RLS 相当フィルタ対象テナント（docs25k.jsonl の約 9 割が tenant-a）。
TENANT_VISIBLE = "tenant-a"
TENANT_OTHER = "tenant-b"

def doc_visibility(doc: dict) -> str:
    """フィクスチャ行の可視性を返す（投入側・正解生成側で共有する唯一の判定）。

    `visibility` キーがあればその値、無い旧フィクスチャ（visibility 未導入）では
    `tenant == TENANT_VISIBLE` を public、それ以外を private とみなす。投入側が
    欠落値を無条件に private として保存すると、正解生成（`recall.py`）が
    tenant-a を可視とみなす契約と食い違い、空結果を非空の正解集合と比較して
    Recall=0 を記録してしまう（codex-review 指摘）ため、両側で本関数を使う。
    """
    vis = doc.get("visibility")
    if vis is not None:
        return str(vis)
    return "public" if doc.get("tenant") == TENANT_VISIBLE else "private"


def public_only_where(table_alias: str = "") -> str:
    """RLS 相当の可視性フィルタ SQL 断片を組み立てる。

    実機確認（wire-server 経由）: 自作 DB の現行契約では、どのテナントの
    wire セッションからも `visibility = 'public'` の行のみが可視であり、
    private 行は所有テナント自身のセッションからも不可視（`docs25k.jsonl`:
    tenant-a 行 = public 23,000・tenant-b 行 = private 2,000。tenant-a・
    tenant-b いずれの wire セッションで `SELECT COUNT(*) FROM docs` しても
    23,000 になることを実機で確認済み）。他 DB は本物の RLS を持たないため、
    この `visibility = 'public'` フィルタを毎クエリの WHERE 句へ素朴に付けて
    模倣する（tenant 列そのものでは絞り込まない）。
    """
    prefix = f"{table_alias}." if table_alias else ""
    return f"{prefix}visibility = 'public'"


DIM = 128


def env_port(name: str, default: int) -> int:
    """環境変数 `name` から接続先ポートを読む（未設定・空文字なら `default`）。

    起動側（`containers.sh`／`gpu/containers_gpu.sh` の `${NAME:-default}`）と
    接続側（各 `*_db.py`）が同じ変数名・同じ既定値を読む契約の Python 側実装。
    shell の `${VAR:-default}` は「未設定」と「空文字」のどちらも既定値へ倒すため、
    ここでも空文字を未設定と同じに扱い両側の解釈を揃える。それ以外の値は
    1〜65535 の整数のみ受理し、不正値は黙って既定へ倒さず `ValueError` で
    fail-closed に拒否する（別サーバーを計測してしまう事故を防ぐ）。
    """
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    # `int()` は前後の空白・符号・`_` 区切りを受理してしまうため、ASCII 十進数字
    # だけの文字列に限定してから変換する。
    if not (raw.isascii() and raw.isdigit()):
        raise ValueError(f"{name} must be an integer port (1-65535), got {raw!r}")
    port = int(raw, 10)
    if not 1 <= port <= 65535:
        raise ValueError(f"{name} must be within 1-65535, got {port}")
    return port


def load_jsonl(path: str) -> list[dict]:
    """1 行 1 JSON のフィクスチャファイルを読み込む（docs25k.jsonl・queries200.jsonl 共通）。"""
    rows = []
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rows.append(json.loads(line))
    return rows


def percentile(sorted_values: Sequence[float], p: float) -> float:
    """sorted_values（昇順）から百分位点 p（0-100）を最近傍ランク法で求める。"""
    if not sorted_values:
        return float("nan")
    n = len(sorted_values)
    idx = max(0, min(n - 1, int(round(p / 100.0 * (n - 1)))))
    return sorted_values[idx]


def latency_stats(latencies_us: Sequence[float]) -> dict:
    """レイテンシ配列（マイクロ秒）から min/p50/p95/p99/mean を求める。"""
    if not latencies_us:
        return {"n": 0}
    arr = sorted(latencies_us)
    return {
        "n": len(arr),
        "min_us": arr[0],
        "p50_us": percentile(arr, 50),
        "p95_us": percentile(arr, 95),
        "p99_us": percentile(arr, 99),
        "mean_us": sum(arr) / len(arr),
        "max_us": arr[-1],
    }


def measure(
    fn: Callable[[Any], Any],
    inputs: Sequence[Any],
    warmup: int = WARMUP,
    iters: int = ITERS,
) -> tuple[dict, Any]:
    """`inputs[i % len(inputs)]` を渡して fn を呼び出すループを warmup 回捨てたうえで
    iters 回計測する（feature_bench.rs の `measure_us` に相当）。

    戻り値は (レイテンシ統計, 最終呼び出しの返り値) のタプル。
    inputs が単一要素（固定クエリの agg 系フェーズ等）でもそのまま動く。
    """
    n = len(inputs)
    if n == 0:
        raise ValueError("measure: inputs is empty")
    for i in range(warmup):
        fn(inputs[i % n])
    lat: list[float] = []
    last = None
    for i in range(iters):
        t0 = time.perf_counter()
        last = fn(inputs[i % n])
        t1 = time.perf_counter()
        lat.append((t1 - t0) * 1_000_000.0)
    return latency_stats(lat), last


def unsupported(reason: str) -> dict:
    """フェーズ未対応を示す統一フォーマット。理由を必ず添える（fail-closed の記録）。"""
    return {"unsupported": True, "reason": reason}


def read_loadavg() -> list[float]:
    try:
        return list(os.getloadavg())
    except OSError:
        return []


def build_meta(
    db: str,
    version: str,
    connection: str,
    config: str,
    rows: int,
    dim: int = DIM,
    extra: dict | None = None,
) -> dict:
    """結果 JSON の meta 部を構築する。

    - db: DB 名（self/pgvector/sqlite_vec/qdrant/lancedb/mysql）
    - version: 実バージョン文字列（`docker inspect` 等で確認した実測値を渡す）
    - connection: 接続経路（loopback TCP／in-process／gRPC）
    - config: exact/hnsw 等の索引構成
    """
    meta = {
        "db": db,
        "version": version,
        "connection": connection,
        "config": config,
        "rows": rows,
        "dim": dim,
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "nproc": os.cpu_count(),
        "loadavg": read_loadavg(),
        "python": platform.python_version(),
        "host": platform.platform(),
        "warmup": WARMUP,
        "iters": ITERS,
    }
    if extra:
        meta.update(extra)
    return meta


def _json_default(o: Any):
    """`decimal.Decimal`（MySQL/PostgreSQL の集計結果に現れる）等、標準の
    `json` が扱えない型を JSON へ落とすためのフォールバック。"""
    import decimal

    if isinstance(o, decimal.Decimal):
        return float(o)
    if isinstance(o, (bytes, bytearray)):
        return o.decode("utf-8", errors="replace")
    return str(o)


def write_result(out_dir: str, db: str, config: str, meta: dict, phases: dict) -> str:
    """`<out_dir>/<db>_<config>.json` へ結果を書き出す。"""
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, f"{db}_{config}.json")
    payload = {"meta": meta, "phases": phases}
    with open(path, "w", encoding="utf-8") as f:
        json.dump(payload, f, ensure_ascii=False, indent=2, default=_json_default)
    return path


def vec_literal(vec: Iterable[float]) -> str:
    """自作 DB の `'[v1,v2,...]'` ベクトルリテラルへ整形する（SQL 文字列組み立て用）。

    数値のみを埋め込む（untrusted な文字列を連結しない。coding-rust.md の
    「SQL 文字列組み立てへの未検証入力連結禁止」を Python 側でも踏襲する）。
    """
    return "[" + ",".join(f"{float(x):.8f}" for x in vec) + "]"


def sql_escape_literal(s: str) -> str:
    """SQL 文字列リテラル用の `'` エスケープ（固定語彙・フィクスチャ由来の文字列専用）。"""
    return s.replace("'", "''")


def wait_for_port(host: str, port: int, timeout_s: float = 30.0) -> bool:
    """TCP ポートが受理可能になるまで待つ（wire-server 起動待ち・DB コンテナ起動待ち共用）。"""
    import socket

    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            with socket.create_connection((host, port), timeout=1.0):
                return True
        except OSError:
            time.sleep(0.2)
    return False


def run_cmd(args: list[str], **kwargs) -> subprocess.CompletedProcess:
    """補助コマンド実行のラッパー（呼び出し元の意図が読めるよう一箇所に集約）。"""
    return subprocess.run(args, capture_output=True, text=True, **kwargs)
