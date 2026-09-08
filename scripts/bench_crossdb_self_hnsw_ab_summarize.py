#!/usr/bin/env python3
"""`scripts/bench_crossdb_self_hnsw_ab.sh --summarize <dir> [session_ts]` の実体
（Issue #658）。

`<dir>` に集まった per-run 生データ（`crossdb_bench/run.py --db self
--config {exact,hnsw}` の結果 JSON。`<ts>-pair<N>-{exact,hnsw}/self_<arm>.json`）
を読み、対象フェーズごとに exact（baseline）/hnsw（candidate）の
min-of-N・median・`ratio = hnsw/exact` を求めて TSV へ出力する。
計測規約は `docs/design/benchmark-judgement-policy.md` 参照——本スクリプトは
数値を機械的に並べるだけで、pass/fail の最終判断は doc（人間）が行う。

計測セッションの分離: `<dir>` に複数回の実行（別の `<ts>`）の生データが
混在すると min-of-N が別セッションの run から選ばれてしまい比較が成立
しなくなる（`scripts/bench_scalar_index_crossdb_ab_summarize.py` と同方針）。
既定で `<dir>` 内に存在する ts を検出し、ちょうど 1 つに定まらない場合は
（省略可能な）第 2 引数 `<session_ts>` での明示指定を要求して拒否する。
"""

from __future__ import annotations

import json
import os
import re
import statistics
import sys

# 前後比較の対象にする区間（bulk 系・フィルタ付き ANN 系を含む）。
PHASES = [
    "vector_knn",
    "vector_knn_where",
    "hybrid_rrf",
    "bulk_knn_k200",
    "bulk_knn_k1000",
    "bulk_knn_where_k200",
    "bulk_hybrid_k200",
]

MIN_PAIRS = 5

PAIR_DIR_RE = re.compile(r"^(?P<ts>[0-9TZ]+)-pair(?P<n>\d+)-(?P<arm>exact|hnsw)$")


def discover_session_ts(dir_path: str) -> str:
    ts_set = set()
    for name in os.listdir(dir_path):
        m = PAIR_DIR_RE.match(name)
        if m:
            ts_set.add(m.group("ts"))
    if len(ts_set) == 0:
        print(f"error: no pair<N>-{{exact,hnsw}} directories found under {dir_path}", file=sys.stderr)
        sys.exit(1)
    if len(ts_set) > 1:
        print(
            f"error: multiple sessions found under {dir_path} ({sorted(ts_set)}); "
            "pass session_ts explicitly to disambiguate",
            file=sys.stderr,
        )
        sys.exit(1)
    return next(iter(ts_set))


def load_result(dir_path: str, ts: str, n: int, arm: str) -> dict | None:
    path = os.path.join(dir_path, f"{ts}-pair{n}-{arm}", f"self_{arm}.json")
    if not os.path.exists(path):
        return None
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def collect(dir_path: str, ts: str, ab_pairs: int) -> dict[str, dict[int, dict]]:
    """`{arm: {pair_n: result_json}}` を返す（欠損 run は含まれない）。"""
    out: dict[str, dict[int, dict]] = {"exact": {}, "hnsw": {}}
    for n in range(1, ab_pairs + 1):
        for arm in ("exact", "hnsw"):
            result = load_result(dir_path, ts, n, arm)
            if result is not None:
                out[arm][n] = result
    return out


def common_pairs(collected: dict[str, dict[int, dict]], phase: str) -> list[int]:
    pairs = []
    for n in collected["exact"]:
        if n not in collected["hnsw"]:
            continue
        exact_phase = collected["exact"][n].get("phases", {}).get(phase)
        hnsw_phase = collected["hnsw"][n].get("phases", {}).get(phase)
        if not isinstance(exact_phase, dict) or exact_phase.get("unsupported"):
            continue
        if not isinstance(hnsw_phase, dict) or hnsw_phase.get("unsupported"):
            continue
        if "p50_us" not in exact_phase or "p50_us" not in hnsw_phase:
            continue
        pairs.append(n)
    return pairs


def emit_row(collected: dict[str, dict[int, dict]], phase: str) -> None:
    pairs = common_pairs(collected, phase)
    if len(pairs) < MIN_PAIRS:
        print(
            f"{phase}\tSKIPPED (only {len(pairs)} common pairs, need >= {MIN_PAIRS})"
        )
        return
    exact_p50 = [collected["exact"][n]["phases"][phase]["p50_us"] for n in pairs]
    hnsw_p50 = [collected["hnsw"][n]["phases"][phase]["p50_us"] for n in pairs]
    exact_min, hnsw_min = min(exact_p50), min(hnsw_p50)
    exact_med, hnsw_med = statistics.median(exact_p50), statistics.median(hnsw_p50)
    ratio_min = hnsw_min / exact_min if exact_min else float("nan")
    ratio_med = hnsw_med / exact_med if exact_med else float("nan")
    print(
        f"{phase}\t{len(pairs)}\t"
        f"{exact_min:.1f}\t{exact_med:.1f}\t"
        f"{hnsw_min:.1f}\t{hnsw_med:.1f}\t"
        f"{ratio_min:.4f}\t{ratio_med:.4f}"
    )


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: bench_crossdb_self_hnsw_ab_summarize.py <dir> [session_ts]", file=sys.stderr)
        return 2
    dir_path = sys.argv[1]
    ts = sys.argv[2] if len(sys.argv) > 2 else discover_session_ts(dir_path)

    # ペア数はディレクトリ列挙から動的に求める（`AB_PAIRS` を summarize 側で
    # 知らないため、存在する最大 pair 番号を上限とする）。
    max_n = 0
    for name in os.listdir(dir_path):
        m = PAIR_DIR_RE.match(name)
        if m and m.group("ts") == ts:
            max_n = max(max_n, int(m.group("n")))
    if max_n == 0:
        print(f"error: no pairs found for session {ts} under {dir_path}", file=sys.stderr)
        return 1

    collected = collect(dir_path, ts, max_n)
    print(f"# session {ts}: exact runs={len(collected['exact'])} hnsw runs={len(collected['hnsw'])}")
    print(
        "phase\tn_pairs\texact_min_us\texact_median_us\thnsw_min_us\thnsw_median_us\t"
        "ratio_min\tratio_median"
    )
    for phase in PHASES:
        emit_row(collected, phase)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
