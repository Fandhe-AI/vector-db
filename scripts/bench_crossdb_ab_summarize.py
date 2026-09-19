#!/usr/bin/env python3
"""`scripts/bench_crossdb_ab.sh --summarize <dir> <rounds> [<round_dir_prefix>]`
の実体（Issue #848）。

`<dir>/results/<round_dir_prefix>1/*.json` ..
`<round_dir_prefix><rounds>/*.json`（`scripts/crossdb_bench/run_all.sh` が
`CROSSDB_RUN_TAG=<round_dir_prefix><N>` で書く per-round 生データ）を読み、
`(db, config, phase)` ごとに N ラウンド分の `p50_us`（レイテンシ系フェーズ）・
`rows_per_sec`（ingest 系フェーズ）を集め、min-of-N・median・run-to-run 幅
（`(max-min)/min`。`docs/design/benchmark-judgement-policy.md` の実測ノイズ帯の
代理）を求めて Markdown 表を出力する。`round_dir_prefix` 省略時は既定 `round`
（`scripts/bench_crossdb_ab.sh` が旧セッション形式で書いた既存コミット済み
生データ `docs/design/bench-data/crossdb-20260918T142251Z-ab/` 等との後方互換）。

self（`self`／`self_hnsw` は同じ db=self・config で区別・`self_nosql`）と
他 DB の min-of-N を比較し、固定 ±5% 帯かつ両 arm 自身の run-to-run 幅を
超える場合のみ勝敗（win/loss）を確定し、それ以外は「僅差」として報告する
（`benchmark-judgement-policy.md` §4 の判定規約に沿う）。**実測件数が
`rounds` 未満の arm（一部ラウンドで対象 DB が FAILED になった等）は、幅が
未算出でも win/loss を確定させず「欠損」として明示する**（n が少ないほど
run-to-run 幅が偶然狭く出て固定 ±5% 帯だけで勝敗が確定してしまう既知の
不具合を避けるため。値が 1 個しかない場合に `run_to_run_width` が返す
`None` を暗黙に `0.0` へフォールバックしていたのが原因だった）。同様に、
ある db/config がある phase では一度も出現しなかった場合（未サポート／全
ラウンド失敗のいずれか。原因の断定はしない）も、他の phase では観測されて
いる arm 一覧と照合したうえで「非観測」として明示する。
pass/fail やアーキテクチャ上の採否そのものはここでは判断しない（生成した
表を人間・`docs/design/crossdb-bench.md` が判断材料として使う）。
"""

from __future__ import annotations

import glob
import json
import os
import statistics
import sys


def load_round_results(
    dir_path: str, rounds: int, round_dir_prefix: str = "round"
) -> dict[tuple[str, str], list[dict]]:
    """(db, config) -> [round1 の JSON, round2 の JSON, ...]（欠損 round は None）"""
    out: dict[tuple[str, str], list[dict | None]] = {}
    for r in range(1, rounds + 1):
        round_dir = os.path.join(dir_path, "results", f"{round_dir_prefix}{r}")
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
        print(f"usage: {sys.argv[0]} <dir> <rounds> [<round_dir_prefix>]", file=sys.stderr)
        return 1
    dir_path = sys.argv[1]
    rounds = int(sys.argv[2])
    round_dir_prefix = sys.argv[3] if len(sys.argv) > 3 else "round"
    if rounds < 5:
        print(f"error: rounds must be >= 5 (got {rounds})", file=sys.stderr)
        return 1

    by_db_config = load_round_results(dir_path, rounds, round_dir_prefix)
    if not by_db_config:
        print(
            f"error: no {round_dir_prefix}1..{round_dir_prefix}{rounds} results found "
            f"under {dir_path}/results/",
            file=sys.stderr,
        )
        return 1
    # 完全な arm 一覧（いずれかの phase・round で最低 1 回は観測された
    # (db, config)）。特定 phase で 0 回しか観測されなかった arm を
    # 「全ラウンド欠損」として明示するための基準集合として使う。
    all_arms: set[tuple[str, str]] = set(by_db_config.keys())

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
    per_phase_self: dict[str, tuple[str, float, float, int]] = {}
    rows_by_phase: dict[str, list[tuple[str, str, str, int, str, float, float, float | None]]] = {}
    for phase in phases:
        seen_arms: set[tuple[str, str]] = set()
        for (db, config), results in sorted(by_db_config.items()):
            metric, values = collect_phase_series(results, phase)
            if metric is None or not values:
                continue
            seen_arms.add((db, config))
            n = len(values)
            mn = min(values)
            med = statistics.median(values)
            width = run_to_run_width(values)
            label = f"{db}/{config}" if config else db
            n_str = f"{n}" if n == rounds else f"{n} (欠損 {rounds - n})"
            width_str = f"{width * 100:.1f}%" if width is not None else "n/a"
            print(f"| {phase} | {label} | {n_str} | {metric} | {mn:.2f} | {med:.2f} | {width_str} |")
            rows_by_phase.setdefault(phase, []).append((db, config, label, n, metric, mn, med, width))
            # baseline は self（SQL 表層・exact 構成）のみに固定する。
            # `self_nosql`（HTTP NoSQL 表層）は別の対照系統であり baseline
            # ではないため、ソート順で後から見つかっても上書きしない。
            if db == "self" and config == "exact" and phase not in per_phase_self:
                per_phase_self[phase] = (metric, mn, width or 0.0, n)
        missing_arms = sorted(all_arms - seen_arms)
        if missing_arms:
            # この phase で 1 度も値を持たなかった arm（他の phase では
            # 観測されている db/config）。DB がこの phase を構造的に
            # サポートしない場合（`phases[...].unsupported == true`）と、
            # 実行が全ラウンドで failed した場合の両方がこの形で現れるため、
            # ここでは「非観測」の事実だけを記録し、原因の断定はしない
            # （`logs/<round_dir_prefix><N>/*.log` の FAILED 有無で人間が
            # 判別する）。
            missing_labels = ", ".join(
                f"{db}/{config}" if config else db for db, config in missing_arms
            )
            print(
                f"| {phase} | （非観測: {missing_labels}） | 0 | n/a | n/a | n/a | n/a |"
            )

    print()
    print("## self との比較（固定 ±5% 帯かつ両 arm の run-to-run 幅を超える場合のみ win/loss。それ以外は僅差）")
    print()
    print("| phase | 対照 db/config | self min-of-N | 対照 min-of-N | 比(対照/self) | 判定 |")
    print("| --- | --- | --- | --- | --- | --- |")
    for phase, rows in sorted(rows_by_phase.items()):
        self_entry = per_phase_self.get(phase)
        if self_entry is None:
            continue
        self_metric, self_min, self_width, self_n = self_entry
        for db, config, label, n, metric, mn, med, width in rows:
            if db == "self" and config == "exact":
                continue
            if metric != self_metric:
                continue
            # 実測件数が rounds 未満（一部ラウンドで FAILED になった等）の
            # arm は win/loss を確定させない。n が小さいほど run-to-run 幅が
            # 偶然狭く出て固定 ±5% 帯だけで勝敗を誤認しうるため（幅が
            # `None`（n<2）のときに 0.0 へフォールバックしていた旧実装の
            # 根本原因）。
            if n < rounds or self_n < rounds:
                print(
                    f"| {phase} | {label} | {self_min:.2f} | {mn:.2f} | n/a | "
                    f"欠損（self n={self_n}/{rounds}, 対照 n={n}/{rounds}） |"
                )
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
