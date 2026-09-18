#!/usr/bin/env python3
"""`scripts/bench_ingest_durability_ab.sh`（Issue #851）が書いた
`<OUT_DIR>/pair<N>-<arm>.log` 群を集計し、arm（`immediate`／`none`）別の
min-of-N・median（`docs/design/benchmark-judgement-policy.md` §3）を
TSV で出力する。標準ライブラリのみ・依存追加なし。

対象値: E0（`tenant::insert_typed_row`）・S0（`EngineCore::
execute_sql_in_session`）の min/median、I8（redb commit）の median
（ns/row。1 commit = 1 行のため commit あたりのコストと同義）。

使い方: `python3 scripts/bench_ingest_durability_ab_summarize.py <OUT_DIR>`
"""

from __future__ import annotations

import argparse
import glob
import os
import re
import statistics
import sys

_HEADER_RE = re.compile(
    r"^ingest_profile_bench: mode=single statements=(\d+) .*durability=(\S+)\s*$"
)
_TIER_RE = re.compile(
    r"^tier\((\w+)\): stmts=(\d+) min=([\d.]+)ms median=([\d.]+)ms"
)
_STAGE_RE = re.compile(r"^stage\((\w+)\): rows=(\d+) median=([\d.]+)ms ns_per_row=([\d.]+)")


def parse_log(path: str) -> dict | None:
    """1 ログファイルから durability token・tier(E0/S0)・stage(I8) の値を
    取り出す。`ingest_profile_bench: OK` が無ければ（途中で fail-closed に
    終了した run）`None` を返し、呼び出し元が破棄・報告できるようにする。"""
    durability: str | None = None
    tiers: dict[str, dict[str, float]] = {}
    stages: dict[str, dict[str, float]] = {}
    ok = False
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.rstrip("\n")
            m = _HEADER_RE.match(line)
            if m:
                durability = m.group(2)
                continue
            m = _TIER_RE.match(line)
            if m:
                tiers[m.group(1)] = {"min_ms": float(m.group(3)), "median_ms": float(m.group(4))}
                continue
            m = _STAGE_RE.match(line)
            if m:
                stages[m.group(1)] = {"median_ms": float(m.group(3)), "ns_per_row": float(m.group(4))}
                continue
            if line.strip() == "ingest_profile_bench: OK":
                ok = True
    if not ok or durability is None:
        return None
    return {"durability": durability, "tiers": tiers, "stages": stages, "path": path}


def collect(out_dir: str) -> dict[str, list[dict]]:
    """`<out_dir>/pair*-<arm>.log` を全て読み、arm 別のパース結果リストへ
    まとめる。壊れた・未完走のログはスキップし警告を stderr へ出す
    （黙って無視しない。coding-rust.md 相当の方針を Python 側でも踏襲する）。"""
    by_arm: dict[str, list[dict]] = {"immediate": [], "none": []}
    paths = sorted(glob.glob(os.path.join(out_dir, "pair*-*.log")))
    if not paths:
        raise ValueError(f"no pair*-*.log files found under {out_dir}")
    for path in paths:
        parsed = parse_log(path)
        if parsed is None:
            print(f"WARNING: skipping incomplete/failed run: {path}", file=sys.stderr)
            continue
        arm = parsed["durability"]
        if arm not in by_arm:
            print(f"WARNING: unexpected durability arm {arm!r} in {path}", file=sys.stderr)
            continue
        by_arm[arm].append(parsed)
    return by_arm


def _metric_series(runs: list[dict], group: str, key: str, field: str) -> list[float]:
    values = []
    for run in runs:
        entry = run[group].get(key)
        if entry is None:
            continue
        values.append(entry[field])
    return values


def render_tsv(by_arm: dict[str, list[dict]]) -> str:
    lines = ["metric\tarm\tn\tmin\tmedian"]
    metrics = [
        ("tiers", "E0_typed_row_api", "min_ms", "E0_min_ms"),
        ("tiers", "E0_typed_row_api", "median_ms", "E0_median_ms"),
        ("tiers", "S0_sql_surface", "min_ms", "S0_min_ms"),
        ("tiers", "S0_sql_surface", "median_ms", "S0_median_ms"),
        ("stages", "I8_commit", "ns_per_row", "I8_commit_ns_per_row"),
    ]
    for arm in ("immediate", "none"):
        runs = by_arm.get(arm, [])
        for group, key, field, label in metrics:
            series = _metric_series(runs, group, key, field)
            if not series:
                lines.append(f"{label}\t{arm}\t0\tn/a\tn/a")
                continue
            lines.append(
                f"{label}\t{arm}\t{len(series)}\t{min(series):.4f}\t{statistics.median(series):.4f}"
            )
    # ratio（none/immediate、min-of-N・median の両方）。分母が n/a なら比は出さない。
    for group, key, field, label in metrics:
        imm_series = _metric_series(by_arm.get("immediate", []), group, key, field)
        none_series = _metric_series(by_arm.get("none", []), group, key, field)
        if imm_series and none_series:
            ratio_min = min(none_series) / min(imm_series)
            ratio_median = statistics.median(none_series) / statistics.median(imm_series)
            lines.append(
                f"{label}\tratio(none/immediate)\tn/a\t{ratio_min:.4f}\t{ratio_median:.4f}"
            )
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out_dir", help="scripts/bench_ingest_durability_ab.sh の OUT_DIR")
    args = parser.parse_args(argv)

    by_arm = collect(args.out_dir)
    for arm in ("immediate", "none"):
        if not by_arm.get(arm):
            print(f"WARNING: no successful runs for arm={arm}", file=sys.stderr)
    sys.stdout.write(render_tsv(by_arm))
    return 0


if __name__ == "__main__":
    sys.exit(main())
