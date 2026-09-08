#!/usr/bin/env python3
"""`scripts/bench_scalar_index_crossdb_ab.sh --summarize <dir> [session_ts]` の実体（Issue #633）。

`<dir>` に集まった per-run 生データ（crossdb self の `self_exact.json`・
`hybrid_after_where.py` の JSON 出力）を読み、対象区間ごとに arm
（before/after/ref）別の min-of-N・median・`ratio = after/before` を求めて
TSV へ出力する。判定クラスの算出は
`docs/design/benchmark-judgement-policy.md` §4 の 2 種ノイズ帯
（固定 ±5% と参照区間実測帯）に従う——本スクリプトは数値を機械的に
並べるだけで、pass/fail の最終判断は doc（人間）が行う。

計測セッションの分離（Issue #633 codex-review P2 指摘）: `<dir>` に複数回の
実行（別の `<ts>`）の生データが混在すると、before/after の min-of-N が
別セッションの run から選ばれてしまい比較が成立しなくなる。そのため本
スクリプトは既定で `<dir>` 内に存在する ts を検出し、ちょうど 1 つに
定まらない場合は（省略可能な）第 2 引数 `<session_ts>` での明示指定を
要求して拒否する。
"""

from __future__ import annotations

import json
import os
import re
import statistics
import sys

# crossdb self フェーズ側で前後比較の対象にする区間（Issue #633 本文の 4 フェーズ）。
CROSSDB_PHASES = ["hybrid_rrf", "bulk_hybrid_k200", "vector_knn_where", "where_compound_count"]
# 参照区間（WHERE 前に実行される・状態非依存であるはずの区間）。同一計測
# セッション内でのこの区間の run-to-run 幅を「実測ノイズ帯」として、固定
# ±5% 帯とあわせた 2 種判定に使う（`docs/design/benchmark-judgement-policy.md`
# §4）。
CROSSDB_REFERENCE_PHASES = ["vector_knn", "mode_recall"]
# 実測ノイズ帯（reference_band）の算出に使う代表区間。Issue #633
# codex-review P1 指摘の実例（`vector_knn.p50` が after run 間で約 16.48%
# 変動）に合わせ、`vector_knn.p50` を各 arm・セッション共通の実測ノイズ帯
# として使う（`hybrid_loop` 区間は `hybrid_after_where.py` 側に対応する
# 参照区間を持たないため、同一セッションの crossdb self 側の値を代用する）。
REFERENCE_BAND_PHASE = "vector_knn"

# `docs/design/benchmark-judgement-policy.md` §3「N ≥ 5 ペア」の下限
# （codex-review P2 指摘: 集計前に必ずこの件数を満たすことを検証する）。
MIN_PAIRS = 5

HYBRID_MODES = ["hybrid", "warm_where_then_hybrid", "body_predicate"]

RUN_DIR_RE = re.compile(r"^(?P<ts>\d{8}T\d{6}Z)-crossdb-(?P<arm>before|after|ref)-run(?P<pair>\d+)$")
HYBRID_FILE_RE = re.compile(
    r"^(?P<ts>\d{8}T\d{6}Z)-hybrid-(?P<mode>hybrid|warm_where_then_hybrid|body_predicate)-"
    r"(?P<arm>before|after|ref)-run(?P<pair>\d+)\.json$"
)


def discover_sessions(dir_path: str) -> list[str]:
    """`<dir>` に存在する計測セッション（ts 接頭辞）を列挙する（昇順・重複なし）。"""
    sessions: set[str] = set()
    for entry in os.listdir(dir_path):
        m = RUN_DIR_RE.match(entry)
        if m:
            sessions.add(m.group("ts"))
            continue
        m = HYBRID_FILE_RE.match(entry)
        if m:
            sessions.add(m.group("ts"))
    return sorted(sessions)


def discover_crossdb_pairs(dir_path: str, session_ts: str) -> dict[str, set[int]]:
    """crossdb self の run ディレクトリから arm → 実在する run 番号（pair）集合を求める。

    `collect_crossdb` と同じ `self_exact.json` の存在を要件とする（欠損した
    run は「ペアが揃っている」とみなさない）。`validate_pairs` の入力。
    """
    out: dict[str, set[int]] = {}
    for entry in sorted(os.listdir(dir_path)):
        m = RUN_DIR_RE.match(entry)
        if not m or m.group("ts") != session_ts:
            continue
        json_path = os.path.join(dir_path, entry, "self_exact.json")
        if not os.path.isfile(json_path):
            continue
        out.setdefault(m.group("arm"), set()).add(int(m.group("pair")))
    return out


def discover_hybrid_pairs(dir_path: str, session_ts: str) -> dict[str, dict[str, set[int]]]:
    """hybrid_after_where.py の出力ファイルから mode → arm → 実在する run 番号集合を求める。"""
    out: dict[str, dict[str, set[int]]] = {}
    for fname in sorted(os.listdir(dir_path)):
        m = HYBRID_FILE_RE.match(fname)
        if not m or m.group("ts") != session_ts:
            continue
        out.setdefault(m.group("mode"), {}).setdefault(m.group("arm"), set()).add(int(m.group("pair")))
    return out


def validate_pairs(
    arms: list[str],
    crossdb_pairs: dict[str, set[int]],
    hybrid_pairs: dict[str, dict[str, set[int]]],
) -> list[str]:
    """集計前に N ≥ `MIN_PAIRS` ペア・before/対象 arm 間の run 番号対応を検証する
    （codex-review P2 指摘: 検証なしでは before 3 回/after 2 回のような不完全
    データでも通常どおり判定を出力してしまう）。

    「対応」は「同じ run 番号（pair）を both arm が持つこと」で判定する
    （交互実行〔`benchmark-judgement-policy.md` §3〕は同一ペア番号を
    before/after で対にする前提のため、番号が食い違う run 同士を突き合わせて
    min-of-N を取ると異なる測定条件を比較することになる）。

    違反を表す人間可読なメッセージのリストを返す（空リストなら検証を通過）。
    crossdb・hybrid いずれもデータ自体が存在しない区間は対象外とする
    （`emit_row` 側が空データを別途「該当なし」として扱うため、ここでの
    責務は「存在するデータが不完全なまま判定に使われること」の防止に限る）。
    """
    errors: list[str] = []

    before_crossdb = crossdb_pairs.get("before", set())
    for arm in arms:
        if arm == "before" or arm not in crossdb_pairs:
            continue
        matched = before_crossdb & crossdb_pairs[arm]
        if len(matched) < MIN_PAIRS:
            errors.append(
                f"crossdb before/{arm}: matched run pairs={len(matched)} (< {MIN_PAIRS} required); "
                f"before runs={sorted(before_crossdb)} {arm} runs={sorted(crossdb_pairs[arm])}"
            )

    for mode, per_arm in hybrid_pairs.items():
        before_hybrid = per_arm.get("before", set())
        for arm in arms:
            if arm == "before" or arm not in per_arm:
                continue
            matched = before_hybrid & per_arm[arm]
            if len(matched) < MIN_PAIRS:
                errors.append(
                    f"hybrid[{mode}] before/{arm}: matched run pairs={len(matched)} "
                    f"(< {MIN_PAIRS} required); before runs={sorted(before_hybrid)} "
                    f"{arm} runs={sorted(per_arm[arm])}"
                )

    return errors


def collect_crossdb(dir_path: str, session_ts: str) -> dict:
    """`<dir>/<session_ts>-crossdb-<arm>-run<N>/self_exact.json` を arm → phase → [p50_us,...] へ集約する。"""
    out: dict = {}
    for entry in sorted(os.listdir(dir_path)):
        m = RUN_DIR_RE.match(entry)
        if not m or m.group("ts") != session_ts:
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


def collect_hybrid(dir_path: str, session_ts: str) -> dict:
    """`<dir>/<session_ts>-hybrid-<mode>-<arm>-run<N>.json` を arm → mode → {p50/p95/rss} へ集約する。"""
    out: dict = {}
    for fname in sorted(os.listdir(dir_path)):
        m = HYBRID_FILE_RE.match(fname)
        if not m or m.group("ts") != session_ts:
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


def reference_band(values: list[float]) -> float | None:
    """同一計測セッションで得た参照区間の run-to-run 全幅（相対値）。

    `docs/design/benchmark-judgement-policy.md` §4:
    `reference_band = (reference_max - reference_min) / reference_min`。
    2 点未満（比較不能）または `reference_min` が 0 以下の場合は算出不能
    として `None` を返す（呼び出し側は固定帯のみで判定し `regressed`/
    `improved` を断定しない旨を記録する）。

    呼び出し側（`main`）は `before` arm の値列と比較対象 arm の値列を
    連結してから渡す（同一計測セッション全体の run-to-run 幅を実測帯と
    する。`before` 側の測定揺らぎだけを除外すると、比較対象 arm の値列
    だけでは検出できない変動を見落とし、実際にはノイズ帯内の差を
    `regressed`/`improved` に誤分類しうる——codex-review P1 指摘）。
    """
    if len(values) < 2:
        return None
    lo, hi = min(values), max(values)
    if lo <= 0:
        return None
    return (hi - lo) / lo


def classify(ratio: float, fixed_band: float = 0.05, ref_band: float | None = None) -> str:
    """固定 ±5% 帯・参照区間実測帯（`ref_band`）の両方を超えた場合のみ
    `regressed`/`improved` とする（`benchmark-judgement-policy.md` §4
    「判定に効かせる差分は、次の 2 種のノイズ帯を両方超えていることを
    要件とする。片方のみを超える場合は『ノイズ帯内』として記録し、
    採否の根拠にしない」）。`ref_band` が算出不能（`None`）の場合は
    固定帯のみで `within_band` へ倒す（fail-closed。実測帯なしで
    regressed/improved を断定しない）。最終判定は本 doc 側（人間）が行う。
    """
    exceeds_fixed = abs(ratio - 1.0) > fixed_band
    if ref_band is None:
        return "within_band(no_ref_band)"
    exceeds_ref = abs(ratio - 1.0) > ref_band
    if not (exceeds_fixed and exceeds_ref):
        return "within_band"
    return "regressed" if ratio > 1.0 else "improved"


def emit_row(
    rows: list, section: str, name: str, unit: str, data: dict, arms: list[str], ref_bands: dict
) -> None:
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
            row[f"{arm}_ref_band"] = ""
            row[f"{arm}_class"] = ""
            continue
        arm_min, arm_median = min_median(vals)
        ratio = arm_min / before_min if before_min else float("nan")
        rb = ref_bands.get(arm)
        row[f"{arm}_min"] = f"{arm_min:.2f}"
        row[f"{arm}_median"] = f"{arm_median:.2f}"
        row[f"{arm}_ratio_min"] = f"{ratio:.4f}"
        row[f"{arm}_ref_band"] = f"{rb * 100:.2f}%" if rb is not None else "n/a"
        row[f"{arm}_class"] = classify(ratio, ref_band=rb)
    rows.append(row)


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
                f"ERROR: session_ts {session_ts!r} not found under {dir_path}; "
                f"available sessions: {', '.join(sessions)}",
                file=sys.stderr,
            )
            return 2
    else:
        if len(sessions) > 1:
            print(
                "ERROR: multiple measurement sessions found under "
                f"{dir_path}: {', '.join(sessions)}. "
                f"Pass one explicitly: {sys.argv[0]} <dir> <session_ts> "
                "(mixing sessions makes before/after min-of-N incomparable — "
                "Issue #633 codex-review P2 指摘)",
                file=sys.stderr,
            )
            return 2
        session_ts = sessions[0]

    crossdb = collect_crossdb(dir_path, session_ts)
    hybrid = collect_hybrid(dir_path, session_ts)
    arms_present = [a for a in ("before", "after", "ref") if a in crossdb or a in hybrid]
    if "before" not in arms_present:
        print("ERROR: no before-arm data found", file=sys.stderr)
        return 2
    arms = ["before"] + [a for a in arms_present if a != "before"]

    # N ≥ 5 ペア・before/対象 arm 間の run 番号対応を検証する（codex-review
    # P2 指摘）。不完全なデータのまま判定へ進まず拒否する（fail-closed）。
    crossdb_pairs = discover_crossdb_pairs(dir_path, session_ts)
    hybrid_pairs = discover_hybrid_pairs(dir_path, session_ts)
    pair_errors = validate_pairs(arms, crossdb_pairs, hybrid_pairs)
    if pair_errors:
        print(
            "ERROR: insufficient or mismatched run pairs "
            f"(N >= {MIN_PAIRS} matched pairs required per "
            "docs/design/benchmark-judgement-policy.md §3 — codex-review P2 指摘):",
            file=sys.stderr,
        )
        for err in pair_errors:
            print(f"  - {err}", file=sys.stderr)
        return 2

    # 実測ノイズ帯（reference_band）: `before` arm と比較対象 arm の
    # `REFERENCE_BAND_PHASE`（`vector_knn.p50`）run-to-run 値列を連結した
    # うえで算出する（同一計測セッション全体の変動幅を実測帯とする。
    # `before` 側の測定揺らぎだけを除外すると、たとえば `before` 側の
    # 変動が比較対象 arm 側より大きいケースでノイズ帯を過小評価し、
    # 実際にはノイズ帯内の差を `regressed`/`improved` に誤分類しうる
    # ——`docs/design/benchmark-judgement-policy.md` §4「同一計測セッション
    # で得た参照区間の run-to-run 幅」・codex-review P1 指摘）。
    before_ref_vals = crossdb.get("before", {}).get(REFERENCE_BAND_PHASE, {}).get("p50", [])
    ref_bands: dict[str, float | None] = {}
    for arm in arms:
        if arm == "before":
            continue
        arm_vals = crossdb.get(arm, {}).get(REFERENCE_BAND_PHASE, {}).get("p50", [])
        ref_bands[arm] = reference_band(before_ref_vals + arm_vals)

    print(f"# session_ts={session_ts}", file=sys.stderr)
    if before_ref_vals:
        print(
            f"# reference_band[before] {REFERENCE_BAND_PHASE}.p50: "
            f"min={min(before_ref_vals):.2f} max={max(before_ref_vals):.2f}",
            file=sys.stderr,
        )
    else:
        print(f"# reference_band[before] {REFERENCE_BAND_PHASE}.p50: no data", file=sys.stderr)
    for arm in arms:
        if arm == "before":
            continue
        arm_vals = crossdb.get(arm, {}).get(REFERENCE_BAND_PHASE, {}).get("p50", [])
        combined = before_ref_vals + arm_vals
        rb = ref_bands.get(arm)
        if combined:
            print(
                f"# reference_band[before+{arm}] {REFERENCE_BAND_PHASE}.p50: "
                f"min={min(combined):.2f} max={max(combined):.2f} "
                f"band={'n/a' if rb is None else f'{rb * 100:.2f}%'}",
                file=sys.stderr,
            )
        else:
            print(f"# reference_band[before+{arm}] {REFERENCE_BAND_PHASE}.p50: no data", file=sys.stderr)

    rows: list[dict] = []
    for phase in CROSSDB_PHASES + CROSSDB_REFERENCE_PHASES:
        section = "reference" if phase in CROSSDB_REFERENCE_PHASES else "crossdb"
        data_p50 = {arm: crossdb.get(arm, {}).get(phase, {}).get("p50", []) for arm in arms}
        emit_row(rows, section, f"{phase}.p50", "us", data_p50, arms, ref_bands)
        data_p95 = {arm: crossdb.get(arm, {}).get(phase, {}).get("p95", []) for arm in arms}
        emit_row(rows, section, f"{phase}.p95", "us", data_p95, arms, ref_bands)

    for mode in HYBRID_MODES:
        data_p50 = {arm: hybrid.get(arm, {}).get(mode, {}).get("p50", []) for arm in arms}
        emit_row(rows, "hybrid_loop", f"{mode}.p50", "us", data_p50, arms, ref_bands)
        data_p95 = {arm: hybrid.get(arm, {}).get(mode, {}).get("p95", []) for arm in arms}
        emit_row(rows, "hybrid_loop", f"{mode}.p95", "us", data_p95, arms, ref_bands)
        for rss_key in ("rss_start", "rss_after_warm", "rss_end"):
            data_rss = {arm: hybrid.get(arm, {}).get(mode, {}).get(rss_key, []) for arm in arms}
            emit_row(rows, "hybrid_loop_rss", f"{mode}.{rss_key}", "MiB", data_rss, arms, ref_bands)

    header = ["section", "name", "unit", "before_min", "before_median"]
    for arm in arms:
        if arm == "before":
            continue
        header += [f"{arm}_min", f"{arm}_median", f"{arm}_ratio_min", f"{arm}_ref_band", f"{arm}_class"]

    print("\t".join(header))
    for row in rows:
        print("\t".join(str(row.get(h, "")) for h in header))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
