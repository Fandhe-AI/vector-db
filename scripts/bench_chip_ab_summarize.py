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

**ペア完全性（`benchmark-judgement-policy.md` §3 の N≥5 交互ペア契約）**:
メトリクスごとに同一ペア番号 `pair<N>` の before/after 両側が揃っている
ものだけを「完全なペア」として扱う。before/after いずれかが欠損・
読み込み失敗のペア番号はそのメトリクスの比較対象から除外し、完全な
ペア数が 5 未満のメトリクスは判定（Improved/Regressed/Neutral）を出さず
`insufficient_pairs` セクションへ回す（前バージョンは summary.json が
1 件でも両側に存在すれば判定を出しており、途中終了・JSON 欠損時に
5 ペア未満の不完全な判定を返しうる不具合があった。codex-review 指摘・
Issue #530）。

**per-run 生データの保持（同 §3）**: 比較 TSV の各行に、判定へ使った
完全なペアの生値列（`pair<N>=<値>` をカンマ区切り）を before/after 双方
残す。事後の再判定・`benchmark-judgement-policy.md` §4 の参照区間ノイズ帯
（`(reference_max - reference_min) / reference_min`）計算に必要な値列を
TSV だけから復元できるようにする（前バージョンは件数・min・median のみで
生値列を保持しておらず、事後の再判定・ノイズ帯計算ができなかった。
codex-review 指摘・Issue #530）。
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

# `benchmark-judgement-policy.md` §3 の N≥5 交互ペア規約。
MIN_COMPLETE_PAIRS = 5


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


def metric_values(summary: dict[str, Any]) -> dict[tuple[str, str], float]:
    """1 件の `summary.json` から `(workload, metric_key) -> min 値` を集める
    （`BENCH_CHIP_ROUNDS=1` 固定のため 1 ペア = 1 run。各 run の代表値として
    その run 内の `min` を使う）。"""
    out: dict[tuple[str, str], float] = {}
    results = summary.get("results", {})
    for workload, wdata in results.items():
        metrics = wdata.get("metrics", {})
        for key, series in metrics.items():
            v = series.get("min")
            if v is None:
                continue
            out[(workload, key)] = float(v)
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


def fmt_values(pair_values: dict[int, float]) -> str:
    """`pair<N>=<値>` をペア番号昇順・カンマ区切りで直列化する。"""
    return ",".join(f"pair{n}={v:.6g}" for n, v in sorted(pair_values.items()))


def median_of(values: list[float]) -> float:
    vs = sorted(values)
    mid = len(vs) // 2
    if len(vs) % 2 == 1:
        return vs[mid]
    return (vs[mid - 1] + vs[mid]) / 2.0


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

    # ペア番号ごとに before/after 双方を読み込む。片側でも欠損・読み込み
    # 失敗ならそのペア番号は「完全なペア」の集合から除外する（メトリクス側で
    # 個別に判定するため、ここでは読み込めた値だけを保持しておく）。
    before_by_pair: dict[int, dict[tuple[str, str], float]] = {}
    after_by_pair: dict[int, dict[tuple[str, str], float]] = {}
    before_env: dict[int, dict[str, Any]] = {}
    after_env: dict[int, dict[str, Any]] = {}
    for n in sorted(pairs):
        sides = pairs[n]
        if "before" in sides:
            try:
                s = load_summary(sides["before"])
                before_by_pair[n] = metric_values(s)
                before_env[n] = env_row(s)
            except (OSError, ValueError, json.JSONDecodeError) as e:
                print(f"WARN: skip pair{n}-before: {e}", file=sys.stderr)
        if "after" in sides:
            try:
                s = load_summary(sides["after"])
                after_by_pair[n] = metric_values(s)
                after_env[n] = env_row(s)
            except (OSError, ValueError, json.JSONDecodeError) as e:
                print(f"WARN: skip pair{n}-after: {e}", file=sys.stderr)

    if not before_by_pair or not after_by_pair:
        print("ERROR: need at least one before and one after summary.json", file=sys.stderr)
        return 1

    # 環境ブロック（読み込めた最初のペアの before/after 各 1 行）。
    print("# env")
    print("side\tdetected_isa\tlogical_cpus\tmodel_name\truntime_features")
    b_env = before_env[sorted(before_env)[0]]
    a_env = after_env[sorted(after_env)[0]]
    print(
        "before\t"
        + "\t".join(
            str(x)
            for x in (
                b_env["detected_isa"],
                b_env["logical_cpus"],
                b_env["model_name"],
                json.dumps(b_env["runtime_features"], sort_keys=True),
            )
        )
    )
    print(
        "after\t"
        + "\t".join(
            str(x)
            for x in (
                a_env["detected_isa"],
                a_env["logical_cpus"],
                a_env["model_name"],
                json.dumps(a_env["runtime_features"], sort_keys=True),
            )
        )
    )

    # メトリクスキーの全集合（before 側・after 側いずれかに 1 回でも現れたもの）。
    all_keys: set[tuple[str, str]] = set()
    for vals in before_by_pair.values():
        all_keys.update(vals.keys())
    for vals in after_by_pair.values():
        all_keys.update(vals.keys())

    print()
    print("# comparison")
    print(
        "workload\tmetric\tn_pairs\tbefore_min\tbefore_median\t"
        "after_min\tafter_median\tratio_min\tratio_median\tclass_min\tclass_median\t"
        "before_values\tafter_values"
    )

    insufficient: list[tuple[str, str, int, str, str]] = []

    for workload, key in sorted(all_keys):
        # 同一ペア番号の before/after 双方にこのメトリクスが存在するペアのみを
        # 「完全なペア」とする（benchmark-judgement-policy.md §3 の N≥5 交互
        # ペア契約。片側にしか無いペア番号は不完全ペアとして除外する）。
        complete_before: dict[int, float] = {}
        complete_after: dict[int, float] = {}
        for n in sorted(set(before_by_pair) & set(after_by_pair)):
            bv = before_by_pair[n].get((workload, key))
            av = after_by_pair[n].get((workload, key))
            if bv is None or av is None:
                continue
            complete_before[n] = bv
            complete_after[n] = av

        n_pairs = len(complete_before)
        if n_pairs < MIN_COMPLETE_PAIRS:
            insufficient.append(
                (
                    workload,
                    key,
                    n_pairs,
                    fmt_values(complete_before),
                    fmt_values(complete_after),
                )
            )
            continue

        bv_list = list(complete_before.values())
        av_list = list(complete_after.values())
        b_min, a_min = min(bv_list), min(av_list)
        b_med, a_med = median_of(bv_list), median_of(av_list)
        ratio_min = a_min / b_min if b_min != 0 else float("nan")
        ratio_med = a_med / b_med if b_med != 0 else float("nan")
        print(
            f"{workload}\t{key}\t{n_pairs}\t{b_min:.6g}\t{b_med:.6g}\t"
            f"{a_min:.6g}\t{a_med:.6g}\t{ratio_min:.4f}\t{ratio_med:.4f}\t"
            f"{classify(ratio_min)}\t{classify(ratio_med)}\t"
            f"{fmt_values(complete_before)}\t{fmt_values(complete_after)}"
        )

    print()
    print(
        f"# insufficient_pairs (fewer than {MIN_COMPLETE_PAIRS} complete "
        "before/after pairs at the same pair number; judgement withheld "
        "per benchmark-judgement-policy.md §3)"
    )
    print("workload\tmetric\tn_pairs\tbefore_values\tafter_values")
    for workload, key, n_pairs, bvals, avals in insufficient:
        print(f"{workload}\t{key}\t{n_pairs}\t{bvals}\t{avals}")

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
