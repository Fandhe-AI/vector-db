#!/usr/bin/env python3
"""`scripts/bench_crossdb_ab.sh --summarize <dir> <rounds>` の実体（Issue #848）。

`<dir>/results/round1/*.json` .. `round<rounds>/*.json`（`scripts/crossdb_bench/
run_all.sh` が `CROSSDB_RUN_TAG=round<N>` で書く per-round 生データ）を読み、
`(db, config, phase)` ごとに N ラウンド分の `p50_us`（レイテンシ系フェーズ）・
`rows_per_sec`（ingest 系フェーズ）を集め、min-of-N・median・run-to-run 幅
（`(max-min)/min`。`docs/design/benchmark-judgement-policy.md` の実測ノイズ帯の
代理）を求めて Markdown 表を出力する。

self（`self`／`self_hnsw` は同じ db=self・config で区別・`self_nosql`）と
他 DB の min-of-N を比較し、固定 ±5% 帯かつ両 arm 自身の run-to-run 幅を
超える場合のみ勝敗（win/loss）を確定し、それ以外は「僅差」として報告する
（`benchmark-judgement-policy.md` §4 の判定規約に沿う）。pass/fail や
アーキテクチャ上の採否そのものはここでは判断しない（生成した表を人間・
`docs/design/crossdb-bench.md` が判断材料として使う）。
"""

from __future__ import annotations

import glob
import json
import os
import statistics
import sys


def load_round_results(dir_path: str, rounds: int) -> dict[tuple[str, str], list[dict]]:
    """(db, config) -> [round1 の JSON, round2 の JSON, ...]（欠損 round は None）"""
    out: dict[tuple[str, str], list[dict | None]] = {}
    for r in range(1, rounds + 1):
        round_dir = os.path.join(dir_path, "results", f"round{r}")
        found_this_round: set[tuple[str, str]] = set()
        for f in sorted(glob.glob(os.path.join(round_dir, "*.json"))):
            base = os.path.basename(f)[: -len(".json")]
            # ファイル名は `{db}_{config}.json`。db 名自体に `_` を含む
            # （self_nosql・mongodb_plain）ため、末尾の exact/hnsw だけを
            # config として切り出す（それ以外の suffix は db 名の一部と見なす）。
            if base.endswith("_exact"):
                db, config = base[: -len("_exact")], "exact"
            elif base.endswith("_hnsw"):
                db, config = base[: -len("_hnsw")], "hnsw"
            else:
                db, config = base, ""
            with open(f, encoding="utf-8") as fh:
                data = json.load(fh)
            key = (db, config)
            out.setdefault(key, [None] * rounds)
            out[key][r - 1] = data
            found_this_round.add(key)
    return out


def phase_value(phase_data: dict) -> tuple[str, float] | None:
    """(metric_name, value) を返す。unsupported・値なしは None。"""
    if not isinstance(phase_data, dict):
        return None
    if phase_data.get("unsupported"):
        return None
    if "p50_us" in phase_data:
        return ("p50_us", float(phase_data["p50_us"]))
    if "rows_per_sec" in phase_data:
        return ("rows_per_sec", float(phase_data["rows_per_sec"]))
    return None


def collect_phase_series(
    results: list[dict | None], phase: str
) -> tuple[str | None, list[float]]:
    metric_name: str | None = None
    values: list[float] = []
    for r in results:
        if r is None:
            continue
        pv = phase_value(r.get("phases", {}).get(phase, {}))
        if pv is None:
            continue
        metric_name = pv[0]
        values.append(pv[1])
    return metric_name, values


def run_to_run_width(values: list[float]) -> float | None:
    if len(values) < 2:
        return None
    lo = min(values)
    if lo <= 0:
        return None
    return (max(values) - lo) / lo


def main() -> int:
    if len(sys.argv) < 3:
        print(f"usage: {sys.argv[0]} <dir> <rounds>", file=sys.stderr)
        return 1
    dir_path = sys.argv[1]
    rounds = int(sys.argv[2])
    if rounds < 5:
        print(f"error: rounds must be >= 5 (got {rounds})", file=sys.stderr)
        return 1

    by_db_config = load_round_results(dir_path, rounds)
    if not by_db_config:
        print(f"error: no round1..round{rounds} results found under {dir_path}/results/", file=sys.stderr)
        return 1

    # フェーズ名の和集合（db によって計測対象フェーズが異なるため）。
    all_phases: set[str] = set()
    for results in by_db_config.values():
        for r in results:
            if r is not None:
                all_phases.update(r.get("phases", {}).keys())
    all_phases.discard("_ann_index_note")
    phases = sorted(all_phases)

    print("# crossdb 再計測 集計（Issue #848）")
    print()
    print(f"rounds={rounds} dir={dir_path}")
    print()
    print("| phase | db/config | n | metric | min-of-N | median | run-to-run 幅 |")
    print("| --- | --- | --- | --- | --- | --- | --- |")
    per_phase_self: dict[str, tuple[str, float, float]] = {}
    rows_by_phase: dict[str, list[tuple[str, str, str, int, str, float, float, float | None]]] = {}
    for phase in phases:
        for (db, config), results in sorted(by_db_config.items()):
            metric, values = collect_phase_series(results, phase)
            if metric is None or not values:
                continue
            n = len(values)
            mn = min(values)
            med = statistics.median(values)
            width = run_to_run_width(values)
            label = f"{db}/{config}" if config else db
            width_str = f"{width * 100:.1f}%" if width is not None else "n/a"
            print(f"| {phase} | {label} | {n} | {metric} | {mn:.2f} | {med:.2f} | {width_str} |")
            rows_by_phase.setdefault(phase, []).append((db, config, label, n, metric, mn, med, width))
            # baseline は self（SQL 表層・exact 構成）のみに固定する。
            # `self_nosql`（HTTP NoSQL 表層）は別の対照系統であり baseline
            # ではないため、ソート順で後から見つかっても上書きしない。
            if db == "self" and config == "exact" and phase not in per_phase_self:
                per_phase_self[phase] = (metric, mn, width or 0.0)

    print()
    print("## self との比較（固定 ±5% 帯かつ両 arm の run-to-run 幅を超える場合のみ win/loss。それ以外は僅差）")
    print()
    print("| phase | 対照 db/config | self min-of-N | 対照 min-of-N | 比(対照/self) | 判定 |")
    print("| --- | --- | --- | --- | --- | --- |")
    for phase, rows in sorted(rows_by_phase.items()):
        self_entry = per_phase_self.get(phase)
        if self_entry is None:
            continue
        self_metric, self_min, self_width = self_entry
        for db, config, label, n, metric, mn, med, width in rows:
            if db == "self" and config == "exact":
                continue
            if metric != self_metric:
                continue
            ratio = mn / self_min if self_min else float("nan")
            other_width = width or 0.0
            # rows_per_sec は大きいほど良い・p50_us は小さいほど良いため、
            # 「self が優位」の向きを metric ごとに反転させる。
            if metric == "rows_per_sec":
                self_better = ratio < 1.0
                margin = abs(1.0 - ratio)
            else:
                self_better = ratio > 1.0
                margin = abs(ratio - 1.0)
            noise_floor = max(0.05, self_width, other_width)
            if margin <= noise_floor:
                verdict = "僅差"
            elif self_better:
                verdict = "self win"
            else:
                verdict = "self loss"
            print(f"| {phase} | {label} | {self_min:.2f} | {mn:.2f} | {ratio:.3f} | {verdict} |")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
