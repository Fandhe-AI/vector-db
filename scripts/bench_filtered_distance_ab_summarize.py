#!/usr/bin/env python3
"""`scripts/bench_filtered_distance_ab.sh --summarize <dir> [session_ts]` の実体
（Issue #655）。

`<dir>` に集まった `scan_stage_profile_bench` の per-run 生ログ
（`<ts>-scan-profile-<before|after>-sel1of5-run<N>.log`・
`<ts>-scan-profile-after-sel1of3-run<N>.log`）を読み、対象区間
（`e2e(vector_knn_where/W0-hot)`・`e2e(vector_knn_where/W0-cold)`）と
参照区間（`e2e(vector_knn/W0-nowhere, cache fast path)`・
`R_dot_kernel_distance_only`）の run 横断 min-of-N・median-of-N・
`ratio = after_min / before_min`・`reference_band` を求めて TSV へ出力する。
判定式は `scripts/bench_scalar_index_crossdb_ab_summarize.py::classify`／
`reference_band` と同一（`docs/design/benchmark-judgement-policy.md` §4）。

before（#654 未適用）バイナリは `(min-of-R=...)` 接尾辞を持たない
（`e2e(vector_knn_where/W0-hot): median={:.3}ms` のみ）ため、抽出正規表現は
接尾辞の有無いずれも受理する。before バイナリは選択率 opt-in
（`BENCH_SCAN_PROFILE_SELECTIVITY`）を持たないため、ペア run
（`sel1of5`）は before/after 共通の既定選択率（1/5）でのみ存在する
——after-only 段（既定 `sel1of3`。`bench_filtered_distance_ab.sh` の
`AB_AFTER_ONLY_SELECTIVITY` の分母をそのままファイル名へ反映するため
分母は可変）は after-only 段としてのみ記録され、この summarizer では
before との比較を行わず min/median のみを出力する
（`bench_filtered_distance_ab.sh` のコメント「1/3 ペア比較の構造的不能」参照）。

before/after 各指標の集計は、run 番号（pair）が両側に揃っている run
だけを対象にする（`scripts/bench_scalar_index_crossdb_ab_summarize.py`
の対応済みペア限定と同型。PR #669 codex-review 指摘）。対応しない
余剰 run が min-of-N・median を歪めるのを防ぐため、指標ごとに
before/after の共通 pair 集合を求め、その集合が `MIN_PAIRS` 未満なら
拒否する。
"""

from __future__ import annotations

import os
import re
import statistics
import sys

MIN_PAIRS = 5

# 対象区間: vector_knn_where の W0-hot/W0-cold。
TARGET_METRICS = [
    ("target", "vector_knn_where.W0-hot", r"e2e\(vector_knn_where/W0-hot\): median=([\d.]+)ms"),
    ("target", "vector_knn_where.W0-cold", r"e2e\(vector_knn_where/W0-cold\): median=([\d.]+)ms"),
]
# 参照区間: 変更を含まない cache fast path・距離カーネル単体。
REFERENCE_METRICS = [
    (
        "reference",
        "vector_knn.W0-nowhere",
        r"e2e\(vector_knn/W0-nowhere, cache fast path\): median=([\d.]+)ms",
    ),
    (
        "reference",
        "R_dot_kernel_distance_only",
        r"R_dot_kernel_distance_only \(reference band\): median=([\d.]+)ms",
    ),
    ("reference", "agg_count.A0a", r"e2e\(agg_count/A0a, ctx=tenant-a\): median=([\d.]+)ms"),
    ("reference", "rls_isolation.A0b", r"e2e\(rls_isolation/A0b, ctx=tenant-b\): median=([\d.]+)ms"),
]
ALL_METRICS = TARGET_METRICS + REFERENCE_METRICS

# after-only（1/3）段でのみ非 vacuous 性・I 系列を確認する行。
AFTER_ONLY_METRICS = [
    ("after_only", "index,k=10", r"e2e\(index,k=10\): median=([\d.]+)ms"),
    ("after_only", "plain,k=10", r"e2e\(plain,k=10\): median=([\d.]+)ms"),
    (
        "after_only",
        "I1_index_candidate_resolve",
        r"bucket_share\(I1_index_candidate_resolve\): us=([\d.]+)",
    ),
    ("after_only", "I2a_candidate_predicate", r"bucket_share\(I2a_candidate_predicate\): us=([\d.]+)"),
    ("after_only", "I2b_candidate_mask_build", r"bucket_share\(I2b_candidate_mask_build\): us=([\d.]+)"),
    ("after_only", "I3_provider_search", r"bucket_share\(I3_provider_search\): us=([\d.]+)"),
]

# denom は `bench_filtered_distance_ab.sh` の AB_AFTER_ONLY_SELECTIVITY
# の分母をそのまま埋め込むため可変（2〜100。PR #669 codex-review 指摘。
# 旧来の `5|3` 固定では既定 1/3 以外の分母を指定したログを拾えなかった）。
# paired 系列は常に分母 5 固定（同スクリプトのハードコード）で書き出される
# ため、denom=="5" は paired・それ以外は after-only 段として扱う。
RUN_RE = re.compile(
    r"^(?P<ts>\d{8}T\d{6}Z)-scan-profile-(?P<side>before|after)-sel1of(?P<denom>\d{1,3})-run(?P<pair>\d+)\.log$"
)


def discover_sessions(dir_path: str) -> list[str]:
    sessions: set[str] = set()
    for entry in os.listdir(dir_path):
        m = RUN_RE.match(entry)
        if m:
            sessions.add(m.group("ts"))
    return sorted(sessions)


def extract(text: str, pattern: str) -> float | None:
    m = re.search(pattern, text)
    if m is None:
        return None
    return float(m.group(1))


def collect(dir_path: str, session_ts: str) -> tuple[dict, dict, str | None]:
    """(sel1of5 系列, after-only 系列, after-only 段の分母文字列) を
    side/denom → metric_name → {pair: value} へ集約する。after-only 段の
    分母は実際に見つかったログ名から拾う（`AB_AFTER_ONLY_SELECTIVITY` の
    分母をそのまま反映。PR #669 codex-review 指摘）。"""
    sel1of5: dict = {"before": {}, "after": {}}
    after_only: dict = {}
    after_only_denom: str | None = None
    for entry in sorted(os.listdir(dir_path)):
        m = RUN_RE.match(entry)
        if not m or m.group("ts") != session_ts:
            continue
        side = m.group("side")
        denom = m.group("denom")
        pair = int(m.group("pair"))
        with open(os.path.join(dir_path, entry), "r", encoding="utf-8") as f:
            text = f.read()
        if "consistency checks passed" not in text:
            print(
                f"ERROR: {entry}: missing 'consistency checks passed' line "
                "(non-vacuous verification failed or run did not complete)",
                file=sys.stderr,
            )
            sys.exit(2)
        if denom == "5":
            bucket = sel1of5[side]
            for _section, name, pattern in ALL_METRICS:
                v = extract(text, pattern)
                if v is not None:
                    bucket.setdefault(name, {})[pair] = v
        else:
            if side != "after":
                print(f"ERROR: unexpected before-side after-only run: {entry}", file=sys.stderr)
                sys.exit(2)
            if after_only_denom is None:
                after_only_denom = denom
            elif after_only_denom != denom:
                print(
                    f"ERROR: mixed after-only selectivity denominators in session "
                    f"{session_ts}: {after_only_denom} vs {denom} ({entry})",
                    file=sys.stderr,
                )
                sys.exit(2)
            if "index_mask_scans_delta" not in text:
                print(
                    f"ERROR: {entry}: missing index_mask_scans_delta counter "
                    "(candidate-id mask path did not fire; #654 non-vacuous check failed)",
                    file=sys.stderr,
                )
                sys.exit(2)
            m2 = re.search(r"index_mask_scans_delta=(\d+)", text)
            if m2 is None or int(m2.group(1)) <= 0:
                print(
                    f"ERROR: {entry}: index_mask_scans_delta is zero or absent (vacuous #654 path)",
                    file=sys.stderr,
                )
                sys.exit(2)
            for _section, name, pattern in AFTER_ONLY_METRICS:
                v = extract(text, pattern)
                if v is not None:
                    after_only.setdefault(name, {})[pair] = v
    return sel1of5, after_only, after_only_denom


def min_median(values: list[float]) -> tuple[float, float]:
    return min(values), statistics.median(values)


def reference_band(values: list[float]) -> float | None:
    if len(values) < 2:
        return None
    lo, hi = min(values), max(values)
    if lo <= 0:
        return None
    return (hi - lo) / lo


def classify(ratio: float, fixed_band: float = 0.05, ref_band: float | None = None) -> str:
    exceeds_fixed = abs(ratio - 1.0) > fixed_band
    if ref_band is None:
        return "within_band(no_ref_band)"
    exceeds_ref = abs(ratio - 1.0) > ref_band
    if not (exceeds_fixed and exceeds_ref):
        return "within_band"
    return "regressed" if ratio > 1.0 else "improved"


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print(f"usage: {sys.argv[0]} <dir> [session_ts]", file=sys.stderr)
        return 2
    dir_path = sys.argv[1]
    if not os.path.isdir(dir_path):
        print(f"ERROR: not a directory: {dir_path}", file=sys.stderr)
        return 2

    sessions = discover_sessions(dir_path)
    if not sessions:
        print(f"ERROR: no run data found under {dir_path}", file=sys.stderr)
        return 2
    if len(sys.argv) == 3:
        session_ts = sys.argv[2]
        if session_ts not in sessions:
            print(
                f"ERROR: session_ts {session_ts!r} not found; available: {', '.join(sessions)}",
                file=sys.stderr,
            )
            return 2
    else:
        if len(sessions) > 1:
            print(
                f"ERROR: multiple measurement sessions found under {dir_path}: "
                f"{', '.join(sessions)}. Pass one explicitly: {sys.argv[0]} <dir> <session_ts>",
                file=sys.stderr,
            )
            return 2
        session_ts = sessions[0]

    sel1of5, after_only, after_only_denom = collect(dir_path, session_ts)

    # 指標ごとに before/after 共通の run 番号（pair）だけを集計対象にする
    # （`scripts/bench_scalar_index_crossdb_ab_summarize.py` の対応済み
    # ペア限定と同型。PR #669 codex-review 指摘）。対応しない余剰 run が
    # min-of-N・median を歪めるのを防ぐため、共通集合が MIN_PAIRS 未満
    # なら拒否する。
    matched_pairs_by_name: dict[str, set[int]] = {}
    pair_errors: list[str] = []
    for _section, name, _pattern in ALL_METRICS:
        before_pairs = set(sel1of5["before"].get(name, {}).keys())
        after_pairs = set(sel1of5["after"].get(name, {}).keys())
        if not before_pairs and not after_pairs:
            continue
        matched = before_pairs & after_pairs
        matched_pairs_by_name[name] = matched
        if len(matched) < MIN_PAIRS:
            pair_errors.append(
                f"sel1of5[{name}]: matched run pairs={len(matched)} (< {MIN_PAIRS} required); "
                f"before runs={sorted(before_pairs)} after runs={sorted(after_pairs)}"
            )
    if pair_errors:
        print(
            "ERROR: insufficient or mismatched run pairs "
            f"(N >= {MIN_PAIRS} matched pairs required per "
            "docs/design/benchmark-judgement-policy.md §3):",
            file=sys.stderr,
        )
        for e in pair_errors:
            print(f"  - {e}", file=sys.stderr)
        return 2

    print(f"# session_ts={session_ts}", file=sys.stderr)

    ref_band_values: list[float] = []
    for side in ("before", "after"):
        vals = list(sel1of5[side].get("vector_knn.W0-nowhere", {}).values())
        ref_band_values.extend(vals)
    ref_dot_values: list[float] = []
    for side in ("before", "after"):
        vals = list(sel1of5[side].get("R_dot_kernel_distance_only", {}).values())
        ref_dot_values.extend(vals)
    rb_nowhere = reference_band(ref_band_values)
    rb_dot = reference_band(ref_dot_values)
    # 判定には保守的に大きい方の参照帯を使う（`docs/design/
    # benchmark-judgement-policy.md` §4 の 2 種ノイズ帯思想を踏まえ、
    # 複数参照区間があるときは緩い方＝より広い帯で誤判定を避ける）。
    candidates_rb = [rb for rb in (rb_nowhere, rb_dot) if rb is not None]
    ref_band = max(candidates_rb) if candidates_rb else None
    print(
        f"# reference_band: W0-nowhere={'n/a' if rb_nowhere is None else f'{rb_nowhere * 100:.2f}%'} "
        f"R_dot={'n/a' if rb_dot is None else f'{rb_dot * 100:.2f}%'} "
        f"(using max={'n/a' if ref_band is None else f'{ref_band * 100:.2f}%'} for classification)",
        file=sys.stderr,
    )

    header = [
        "section",
        "name",
        "unit",
        "before_min",
        "before_median",
        "after_min",
        "after_median",
        "ratio_min",
        "ref_band",
        "class",
    ]
    print("\t".join(header))
    for section, name, _pattern in ALL_METRICS:
        matched = matched_pairs_by_name.get(name)
        if not matched:
            continue
        before_by_pair = sel1of5["before"].get(name, {})
        after_by_pair = sel1of5["after"].get(name, {})
        before_vals = [before_by_pair[p] for p in matched]
        after_vals = [after_by_pair[p] for p in matched]
        b_min, b_median = min_median(before_vals)
        a_min, a_median = min_median(after_vals)
        ratio = a_min / b_min if b_min else float("nan")
        cls = classify(ratio, ref_band=ref_band)
        row = [
            section,
            name,
            "ms",
            f"{b_min:.4f}",
            f"{b_median:.4f}",
            f"{a_min:.4f}",
            f"{a_median:.4f}",
            f"{ratio:.4f}",
            "n/a" if ref_band is None else f"{ref_band * 100:.2f}%",
            cls,
        ]
        print("\t".join(row))

    # after-only（既定 1/3・実際の分母は after_only_denom）段:
    # min/median のみ（before 対照なし）。分母はログ名から拾った実際の
    # 値をそのまま表示する（PR #669 codex-review 指摘。既定 1/3 以外の
    # 分母を指定しても常に "1/3" と表示されていた不整合を解消）。
    if after_only:
        denom_label = f"1/{after_only_denom}" if after_only_denom is not None else "unknown"
        print(
            f"# after-only (selectivity={denom_label}) section: min/median only, no before comparison",
            file=sys.stderr,
        )
        for _section, name, _pattern in AFTER_ONLY_METRICS:
            pairs = after_only.get(name, {})
            if not pairs:
                continue
            vals = list(pairs.values())
            if len(vals) < MIN_PAIRS:
                print(
                    f"# NOTE: after_only[{name}]: only {len(vals)} runs (< {MIN_PAIRS})",
                    file=sys.stderr,
                )
            v_min, v_median = min_median(vals)
            unit = "ms" if name in ("index,k=10", "plain,k=10") else "us"
            row = ["after_only", name, unit, "", "", f"{v_min:.4f}", f"{v_median:.4f}", "", "", ""]
            print("\t".join(row))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
