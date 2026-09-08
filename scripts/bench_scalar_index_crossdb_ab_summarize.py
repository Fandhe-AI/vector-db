#!/usr/bin/env python3
"""`scripts/bench_scalar_index_crossdb_ab.sh --summarize <dir>` の実体（Issue #633）。

`<dir>` に集まった per-run 生データ（crossdb self の `self_exact.json`・
`hybrid_after_where.py` の JSON 出力）を読み、対象区間ごとに arm
（before/after/ref）別の min-of-N・median・`ratio = after/before` を求めて
TSV へ出力する。判定クラスの算出は
`docs/design/benchmark-judgement-policy.md` §4 の 2 種ノイズ帯
（固定 ±5% と参照区間実測帯）に従う——本スクリプトは数値を機械的に
並べるだけで、pass/fail の最終判断は doc（人間）が行う。
"""

from __future__ import annotations

import glob
import json
import os
import re
import statistics
import sys

# crossdb self フェーズ側で前後比較の対象にする区間（Issue #633 本文の 4 フェーズ）。
CROSSDB_PHASES = ["hybrid_rrf", "bulk_hybrid_k200", "vector_knn_where", "where_compound_count"]
# 参照区間（WHERE 前に実行される・状態非依存であるはずの区間）。
CROSSDB_REFERENCE_PHASES = ["vector_knn", "mode_recall"]

HYBRID_MODES = ["hybrid", "warm_where_then_hybrid", "body_predicate"]

RUN_DIR_RE = re.compile(r"^(?P<ts>\d{8}T\d{6}Z)-crossdb-(?P<arm>before|after|ref)-run(?P<pair>\d+)$")
HYBRID_FILE_RE = re.compile(
    r"^(?P<ts>\d{8}T\d{6}Z)-hybrid-(?P<mode>hybrid|warm_where_then_hybrid|body_predicate)-"
    r"(?P<arm>before|after|ref)-run(?P<pair>\d+)\.json$"
)


def collect_crossdb(dir_path: str) -> dict:
    """`<dir>/<ts>-crossdb-<arm>-run<N>/self_exact.json` を arm → phase → [p50_us,...] へ集約する。"""
    out: dict = {}
    for entry in sorted(os.listdir(dir_path)):
        m = RUN_DIR_RE.match(entry)
        if not m:
            continue
        arm = m.group("arm")
        json_path = os.path.join(dir_path, entry, "self_exact.json")
        if not os.path.isfile(json_path):
            continue
        with open(json_path, "r", encoding="utf-8") as f:
            payload = json.load(f)
        phases = payload.get("phases", {})
        for phase_name, phase_data in phases.items():
            if not isinstance(phase_data, dict) or phase_data.get("unsupported"):
                continue
            p50 = phase_data.get("p50_us")
            p95 = phase_data.get("p95_us")
            if p50 is None:
                continue
            out.setdefault(arm, {}).setdefault(phase_name, {"p50": [], "p95": []})
            out[arm][phase_name]["p50"].append(p50)
            if p95 is not None:
                out[arm][phase_name]["p95"].append(p95)
    return out


def collect_hybrid(dir_path: str) -> dict:
    """`<dir>/<ts>-hybrid-<mode>-<arm>-run<N>.json` を arm → mode → {p50/p95/rss} へ集約する。"""
    out: dict = {}
    for fname in sorted(os.listdir(dir_path)):
        m = HYBRID_FILE_RE.match(fname)
        if not m:
            continue
        arm = m.group("arm")
        mode = m.group("mode")
        with open(os.path.join(dir_path, fname), "r", encoding="utf-8") as f:
            payload = json.load(f)
        stats = payload.get("stats", {})
        rss = payload.get("rss", {})
        bucket = out.setdefault(arm, {}).setdefault(
            mode, {"p50": [], "p95": [], "rss_start": [], "rss_after_warm": [], "rss_end": []}
        )
        if stats.get("p50_us") is not None:
            bucket["p50"].append(stats["p50_us"])
        if stats.get("p95_us") is not None:
            bucket["p95"].append(stats["p95_us"])
        for key, out_key in (("start", "rss_start"), ("after_warm", "rss_after_warm"), ("end", "rss_end")):
            v = rss.get(key, {}).get("vm_rss_kib")
            if v is not None:
                bucket[out_key].append(v / 1024.0)
    return out


def min_median(values: list[float]) -> tuple[float, float]:
    return min(values), statistics.median(values)


def classify(ratio: float, band: float = 0.05) -> str:
    """固定 ±5% 帯での粗い分類（`benchmark-judgement-policy.md` §4 の一部）。
    最終判定は参照区間実測帯とあわせて doc 側で行う。"""
    if ratio > 1.0 + band:
        return "regressed"
    if ratio < 1.0 - band:
        return "improved"
    return "within_band"


def emit_row(rows: list, section: str, name: str, unit: str, data: dict, arms: list[str]) -> None:
    before = data.get("before")
    if not before:
        return
    before_min, before_median = min_median(before)
    row = {
        "section": section,
        "name": name,
        "unit": unit,
        "before_min": f"{before_min:.2f}",
        "before_median": f"{before_median:.2f}",
    }
    for arm in arms:
        if arm == "before":
            continue
        vals = data.get(arm)
        if not vals:
            row[f"{arm}_min"] = ""
            row[f"{arm}_median"] = ""
            row[f"{arm}_ratio_min"] = ""
            row[f"{arm}_class"] = ""
            continue
        arm_min, arm_median = min_median(vals)
        ratio = arm_min / before_min if before_min else float("nan")
        row[f"{arm}_min"] = f"{arm_min:.2f}"
        row[f"{arm}_median"] = f"{arm_median:.2f}"
        row[f"{arm}_ratio_min"] = f"{ratio:.4f}"
        row[f"{arm}_class"] = classify(ratio)
    rows.append(row)


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <dir>", file=sys.stderr)
        return 2
    dir_path = sys.argv[1]
    if not os.path.isdir(dir_path):
        print(f"ERROR: not a directory: {dir_path}", file=sys.stderr)
        return 2

    crossdb = collect_crossdb(dir_path)
    hybrid = collect_hybrid(dir_path)
    arms_present = [a for a in ("before", "after", "ref") if a in crossdb or a in hybrid]
    if "before" not in arms_present:
        print("ERROR: no before-arm data found", file=sys.stderr)
        return 2
    arms = ["before"] + [a for a in arms_present if a != "before"]

    rows: list[dict] = []
    for phase in CROSSDB_PHASES + CROSSDB_REFERENCE_PHASES:
        section = "reference" if phase in CROSSDB_REFERENCE_PHASES else "crossdb"
        data_p50 = {arm: crossdb.get(arm, {}).get(phase, {}).get("p50", []) for arm in arms}
        emit_row(rows, section, f"{phase}.p50", "us", data_p50, arms)
        data_p95 = {arm: crossdb.get(arm, {}).get(phase, {}).get("p95", []) for arm in arms}
        emit_row(rows, section, f"{phase}.p95", "us", data_p95, arms)

    for mode in HYBRID_MODES:
        data_p50 = {arm: hybrid.get(arm, {}).get(mode, {}).get("p50", []) for arm in arms}
        emit_row(rows, "hybrid_loop", f"{mode}.p50", "us", data_p50, arms)
        data_p95 = {arm: hybrid.get(arm, {}).get(mode, {}).get("p95", []) for arm in arms}
        emit_row(rows, "hybrid_loop", f"{mode}.p95", "us", data_p95, arms)
        for rss_key in ("rss_start", "rss_after_warm", "rss_end"):
            data_rss = {arm: hybrid.get(arm, {}).get(mode, {}).get(rss_key, []) for arm in arms}
            emit_row(rows, "hybrid_loop_rss", f"{mode}.{rss_key}", "MiB", data_rss, arms)

    header = ["section", "name", "unit", "before_min", "before_median"]
    for arm in arms:
        if arm == "before":
            continue
        header += [f"{arm}_min", f"{arm}_median", f"{arm}_ratio_min", f"{arm}_class"]

    print("\t".join(header))
    for row in rows:
        print("\t".join(str(row.get(h, "")) for h in header))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
