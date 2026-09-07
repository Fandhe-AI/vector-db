#!/usr/bin/env python3
"""`scripts/bench_chip_ab.sh` が積んだ before/after `chip_bench` の
`summary.json`（`crates/engine/benches/chip_bench.rs`・`harness/chip.rs`）を
読み、ワークロード×メトリクスごとの min/median・比率・判定クラスを TSV へ
集約する（Issue #530・親 #459・ルート #455）。

呼び出し元は `scripts/bench_chip_ab.sh --summarize <dir>`（人間の運用者が
`make bench-chip-ab` 経由で起動）。呼び出し先は無い（stdlib のみ・外部
依存なし。`scripts/crossdb_bench/` と同じ「Cargo 依存追加なし」方針）。

判定クラスは `docs/design/benchmark-judgement-policy.md` §4 の固定 ±5% 帯
（ratio <= 0.95 -> Improved / ratio >= 1.05 -> Regressed / 他 Neutral）。
before/after 双方に存在するメトリクスキーの積集合のみを比較し、片側にしか
無いキーは `skipped` セクションへ明示する（`chip_bench.rs` は before/after
間で opt-in 計測モードの追加差分がありうるため）。
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path
from typing import Any

# `pair<N>-(before|after)/summary.json` のみを受理する許可リスト正規表現。
# untrusted なディレクトリ内容（他プロセスの残骸・悪意あるファイル名）を
# 無条件に読み込まない（coding-rust.md「untrusted 入力の扱い」踏襲）。
PAIR_DIR_RE = re.compile(r"^pair(\d+)-(before|after)$")

# 読み込み前にサイズ上限で拒否する（無制限確保防止）。
MAX_JSON_BYTES = 16 * 1024 * 1024


def classify(ratio: float) -> str:
    """`benchmark-judgement-policy.md` §4 の固定 ±5% 帯による判定。"""
    if ratio <= 0.95:
        return "Improved"
    if ratio >= 1.05:
        return "Regressed"
    return "Neutral"


def load_summary(path: Path) -> dict[str, Any]:
    size = path.stat().st_size
    if size > MAX_JSON_BYTES:
        raise ValueError(f"summary.json exceeds {MAX_JSON_BYTES} bytes: {path}")
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def collect_pairs(root: Path) -> dict[int, dict[str, Path]]:
    """`root` 直下の `pair<N>-(before|after)/summary.json` を列挙する。"""
    pairs: dict[int, dict[str, Path]] = {}
    for entry in sorted(root.iterdir()):
        if not entry.is_dir():
            continue
        m = PAIR_DIR_RE.match(entry.name)
        if not m:
            continue
        n = int(m.group(1))
        side = m.group(2)
        summary_path = entry / "summary.json"
        if not summary_path.is_file():
            continue
        pairs.setdefault(n, {})[side] = summary_path
    return pairs


def gather_values(
    summaries: list[dict[str, Any]],
) -> dict[tuple[str, str], list[float]]:
    """複数ラウンド（pair）分の `summary.json` から
    `(workload, metric_key) -> [値, ...]`（各 run の min 値。交互実行の
    min-of-N 判定基盤として各 run の min を代表値に使う）を集める。"""
    out: dict[tuple[str, str], list[float]] = {}
    for s in summaries:
        results = s.get("results", {})
        for workload, wdata in results.items():
            metrics = wdata.get("metrics", {})
            for key, series in metrics.items():
                v = series.get("min")
                if v is None:
                    continue
                out.setdefault((workload, key), []).append(float(v))
    return out


def env_row(summary: dict[str, Any]) -> dict[str, Any]:
    env = summary.get("env", {})
    cpu = env.get("cpu", {})
    return {
        "detected_isa": env.get("detected_isa"),
        "logical_cpus": env.get("logical_cpus"),
        "model_name": cpu.get("model_name"),
        "runtime_features": env.get("runtime_features", {}),
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(f"usage: {argv[0]} <dir>", file=sys.stderr)
        return 2
    root = Path(argv[1])
    if not root.is_dir():
        print(f"ERROR: not a directory: {root}", file=sys.stderr)
        return 1

    pairs = collect_pairs(root)
    if not pairs:
        print(f"ERROR: no pair<N>-(before|after)/summary.json found under {root}", file=sys.stderr)
        return 1

    before_summaries: list[dict[str, Any]] = []
    after_summaries: list[dict[str, Any]] = []
    for n in sorted(pairs):
        sides = pairs[n]
        if "before" in sides:
            try:
                before_summaries.append(load_summary(sides["before"]))
            except (OSError, ValueError, json.JSONDecodeError) as e:
                print(f"WARN: skip pair{n}-before: {e}", file=sys.stderr)
        if "after" in sides:
            try:
                after_summaries.append(load_summary(sides["after"]))
            except (OSError, ValueError, json.JSONDecodeError) as e:
                print(f"WARN: skip pair{n}-after: {e}", file=sys.stderr)

    if not before_summaries or not after_summaries:
        print("ERROR: need at least one before and one after summary.json", file=sys.stderr)
        return 1

    # 環境ブロック（before/after 各 1 行）。
    print("# env")
    print("side\tdetected_isa\tlogical_cpus\tmodel_name\truntime_features")
    print(
        "before\t"
        + "\t".join(
            str(x)
            for x in (
                env_row(before_summaries[0])["detected_isa"],
                env_row(before_summaries[0])["logical_cpus"],
                env_row(before_summaries[0])["model_name"],
                json.dumps(env_row(before_summaries[0])["runtime_features"], sort_keys=True),
            )
        )
    )
    print(
        "after\t"
        + "\t".join(
            str(x)
            for x in (
                env_row(after_summaries[0])["detected_isa"],
                env_row(after_summaries[0])["logical_cpus"],
                env_row(after_summaries[0])["model_name"],
                json.dumps(env_row(after_summaries[0])["runtime_features"], sort_keys=True),
            )
        )
    )

    before_vals = gather_values(before_summaries)
    after_vals = gather_values(after_summaries)

    common_keys = sorted(set(before_vals) & set(after_vals))
    only_before = sorted(set(before_vals) - set(after_vals))
    only_after = sorted(set(after_vals) - set(before_vals))

    print()
    print("# comparison")
    print(
        "workload\tmetric\tn_before\tn_after\tbefore_min\tbefore_median\t"
        "after_min\tafter_median\tratio_min\tratio_median\tclass_min\tclass_median"
    )
    for workload, key in common_keys:
        bv = sorted(before_vals[(workload, key)])
        av = sorted(after_vals[(workload, key)])
        b_min, a_min = bv[0], av[0]
        b_med = bv[len(bv) // 2] if len(bv) % 2 == 1 else (bv[len(bv) // 2 - 1] + bv[len(bv) // 2]) / 2.0
        a_med = av[len(av) // 2] if len(av) % 2 == 1 else (av[len(av) // 2 - 1] + av[len(av) // 2]) / 2.0
        ratio_min = a_min / b_min if b_min != 0 else float("nan")
        ratio_med = a_med / b_med if b_med != 0 else float("nan")
        print(
            f"{workload}\t{key}\t{len(bv)}\t{len(av)}\t{b_min:.6g}\t{b_med:.6g}\t"
            f"{a_min:.6g}\t{a_med:.6g}\t{ratio_min:.4f}\t{ratio_med:.4f}\t"
            f"{classify(ratio_min)}\t{classify(ratio_med)}"
        )

    print()
    print("# skipped (present on one side only)")
    print("side_only\tworkload\tmetric\tn\tmin\tmedian")
    for workload, key in only_before:
        bv = sorted(before_vals[(workload, key)])
        b_med = bv[len(bv) // 2] if len(bv) % 2 == 1 else (bv[len(bv) // 2 - 1] + bv[len(bv) // 2]) / 2.0
        print(f"before_only\t{workload}\t{key}\t{len(bv)}\t{bv[0]:.6g}\t{b_med:.6g}")
    for workload, key in only_after:
        av = sorted(after_vals[(workload, key)])
        a_med = av[len(av) // 2] if len(av) % 2 == 1 else (av[len(av) // 2 - 1] + av[len(av) // 2]) / 2.0
        print(f"after_only\t{workload}\t{key}\t{len(av)}\t{av[0]:.6g}\t{a_med:.6g}")

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
