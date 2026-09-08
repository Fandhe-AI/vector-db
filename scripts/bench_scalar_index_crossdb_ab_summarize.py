#!/usr/bin/env python3
"""`scripts/bench_scalar_index_crossdb_ab.sh --summarize <dir> [session_ts]` の実体（Issue #633）。

`<dir>` に集まった per-run 生データ（crossdb self の `self_exact.json`・
`hybrid_after_where.py` の JSON 出力）を読み、対象区間ごとに候補
（after/ref）別の min-of-N・median・`ratio = candidate/baseline` を求めて
TSV へ出力する。判定クラスの算出は
`docs/design/benchmark-judgement-policy.md` §4 の 2 種ノイズ帯
（固定 ±5% と参照区間実測帯）に従う——本スクリプトは数値を機械的に
並べるだけで、pass/fail の最終判断は doc（人間）が行う。

計測セッションの分離（Issue #633 codex-review P2 指摘）: `<dir>` に複数回の
実行（別の `<ts>`）の生データが混在すると、baseline/candidate の
min-of-N が別セッションの run から選ばれてしまい比較が成立しなくなる。
そのため本スクリプトは既定で `<dir>` 内に存在する ts を検出し、ちょうど
1 つに定まらない場合は（省略可能な）第 2 引数 `<session_ts>` での明示
指定を要求して拒否する。

候補ごとの専用 baseline（Issue #633 codex-review P1 指摘）:
`scripts/bench_scalar_index_crossdb_ab.sh` は 3 arm（before/after/ref）
比較時、`benchmark-judgement-policy.md` §3「baseline/cand1/baseline/
cand2/… の輪番」に従い、候補（after・ref）ごとに専用の直近 baseline
計測を用意する（after は `before`、ref は `before_ref`——いずれも同一
バイナリだが、ref との比較が after 計測ぶん時間的に隔たった before を
参照しないよう別ラベルで記録する）。本スクリプトも比較を候補単位で行い、
baseline・candidate の列を候補ごとに独立させる（TSV の列名は
`<candidate>_baseline_min` 等）。

対応済み run ペアへの限定（Issue #633 codex-review P2 指摘・2 巡目）:
`validate_pairs` は「一致するペア数が N ≥ `MIN_PAIRS`」だけを検証していたが、
実際の集計（`collect_crossdb`/`collect_hybrid`）はセッション内に存在する
run を arm ごとに無条件に全件集約していたため、baseline 6 回・候補 5 回の
ような対応しない余剰 run が min-of-N・median・`reference_band` を歪め
得た（対応しない run は他 arm 側の同時刻条件を欠くため、交互実行が保証
する時間方向の対称性が崩れる）。そのため本スクリプトは crossdb self・
hybrid（モードごと）いずれも、候補ごとの baseline/candidate 2 arm 間の
run 番号（pair）の共通集合だけを集計対象とし、共通集合が `MIN_PAIRS`
未満なら拒否する（fail-closed）。
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
# codex-review P1 指摘の実例（`vector_knn.p50` が run 間で約 16.48%
# 変動）に合わせ、`vector_knn.p50` を候補ごとの実測ノイズ帯として使う
# （`hybrid_loop` 区間は `hybrid_after_where.py` 側に対応する参照区間を
# 持たないため、同一セッションの crossdb self 側の値を代用する）。
REFERENCE_BAND_PHASE = "vector_knn"

# `docs/design/benchmark-judgement-policy.md` §3「N ≥ 5 ペア」の下限
# （codex-review P2 指摘: 集計前に必ずこの件数を満たすことを検証する）。
MIN_PAIRS = 5

HYBRID_MODES = ["hybrid", "warm_where_then_hybrid", "body_predicate"]

# 候補（比較対象 arm）ごとの専用 baseline arm 名。
# `scripts/bench_scalar_index_crossdb_ab.sh::baseline_for_candidate` と
# 同一の対応（codex-review P1 指摘・Issue #633）。
BASELINE_OF = {"after": "before", "ref": "before_ref"}
CANDIDATE_ORDER = ["after", "ref"]


def effective_baseline_map(collected: dict) -> dict[str, str]:
    """候補ごとの実効 baseline arm 名を決める。

    `BASELINE_OF` の専用 baseline（`ref` なら `before_ref`）にデータが
    存在すればそれを使う。存在しない場合は、本スクリプトの候補別
    baseline 導入（P1 修正）より前に取得された生データ（`before`/
    `after`/`ref` の 3 arm のみで `before_ref` を持たない）を再集計
    できるよう `before` へフォールバックする（stderr にその旨を記録し、
    サイレントな挙動変化にはしない）。
    """
    out: dict[str, str] = {}
    for candidate, preferred in BASELINE_OF.items():
        if preferred in collected:
            out[candidate] = preferred
        elif "before" in collected:
            out[candidate] = "before"
            print(
                f"# NOTE: '{preferred}' data absent for candidate '{candidate}'; falling back to "
                "'before' as its baseline (legacy 3-arm data predating the per-candidate baseline "
                "rotation — codex-review P1 指摘・Issue #633)",
                file=sys.stderr,
            )
        else:
            out[candidate] = preferred
    return out

RUN_DIR_RE = re.compile(r"^(?P<ts>\d{8}T\d{6}Z)-crossdb-(?P<arm>before|before_ref|after|ref)-run(?P<pair>\d+)$")
HYBRID_FILE_RE = re.compile(
    r"^(?P<ts>\d{8}T\d{6}Z)-hybrid-(?P<mode>hybrid|warm_where_then_hybrid|body_predicate)-"
    r"(?P<arm>before|before_ref|after|ref)-run(?P<pair>\d+)\.json$"
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


def collect_crossdb(dir_path: str, session_ts: str) -> dict:
    """`<dir>/<session_ts>-crossdb-<arm>-run<N>/self_exact.json` を
    arm → phase → {"p50": {pair: val}, "p95": {pair: val}} へ集約する（未フィルタ）。

    値を `pair`（run 番号）をキーとした dict で保持し、候補ごとの
    baseline/candidate 対応ペア（`common_pairs`）で後段が絞り込めるようにする
    （codex-review P2 指摘・2 巡目: 対応しない余剰 run を含めない）。
    """
    out: dict = {}
    for entry in sorted(os.listdir(dir_path)):
        m = RUN_DIR_RE.match(entry)
        if not m or m.group("ts") != session_ts:
            continue
        arm = m.group("arm")
        pair = int(m.group("pair"))
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
            bucket = out.setdefault(arm, {}).setdefault(phase_name, {"p50": {}, "p95": {}})
            bucket["p50"][pair] = p50
            if p95 is not None:
                bucket["p95"][pair] = p95
    return out


def collect_hybrid(dir_path: str, session_ts: str) -> dict:
    """`<dir>/<session_ts>-hybrid-<mode>-<arm>-run<N>.json` を
    arm → mode → {"p50": {pair: val}, ...} へ集約する（未フィルタ）。"""
    out: dict = {}
    for fname in sorted(os.listdir(dir_path)):
        m = HYBRID_FILE_RE.match(fname)
        if not m or m.group("ts") != session_ts:
            continue
        arm = m.group("arm")
        mode = m.group("mode")
        pair = int(m.group("pair"))
        with open(os.path.join(dir_path, fname), "r", encoding="utf-8") as f:
            payload = json.load(f)
        stats = payload.get("stats", {})
        rss = payload.get("rss", {})
        bucket = out.setdefault(arm, {}).setdefault(
            mode, {"p50": {}, "p95": {}, "rss_start": {}, "rss_after_warm": {}, "rss_end": {}}
        )
        if stats.get("p50_us") is not None:
            bucket["p50"][pair] = stats["p50_us"]
        if stats.get("p95_us") is not None:
            bucket["p95"][pair] = stats["p95_us"]
        for key, out_key in (("start", "rss_start"), ("after_warm", "rss_after_warm"), ("end", "rss_end")):
            v = rss.get(key, {}).get("vm_rss_kib")
            if v is not None:
                bucket[out_key][pair] = v / 1024.0
    return out


def arm_pairs(collected: dict, arm: str, key: str, subkey: str) -> set[int]:
    """`collected`（`collect_crossdb`/`collect_hybrid` の出力）から
    `arm`/`key`（phase または mode）/`subkey`（p50 等）に存在する run 番号集合を返す。"""
    return set(collected.get(arm, {}).get(key, {}).get(subkey, {}).keys())


def common_pairs_for(collected: dict, baseline: str, candidate: str, key: str, subkey: str = "p50") -> set[int]:
    """候補専用 baseline と candidate、両方に存在する run 番号（pair）の積集合。

    `benchmark-judgement-policy.md` §3 の交互実行は同一ペア番号を baseline/
    candidate で対にする前提のため、いずれか一方にしか存在しない run 番号は
    対応しない余剰 run として除外する（codex-review P2 指摘・2 巡目）。
    """
    return arm_pairs(collected, baseline, key, subkey) & arm_pairs(collected, candidate, key, subkey)


def filtered_values(collected: dict, arm: str, key: str, subkey: str, allowed_pairs: set[int]) -> list[float]:
    """`allowed_pairs`（対応済み run 番号）に限定した値列を pair 昇順で返す。"""
    per_pair = collected.get(arm, {}).get(key, {}).get(subkey, {})
    return [per_pair[p] for p in sorted(allowed_pairs) if p in per_pair]


def min_median(values: list[float]) -> tuple[float, float]:
    return min(values), statistics.median(values)


def reference_band(values: list[float]) -> float | None:
    """同一計測セッションで得た参照区間の run-to-run 全幅（相対値）。

    `docs/design/benchmark-judgement-policy.md` §4:
    `reference_band = (reference_max - reference_min) / reference_min`。
    2 点未満（比較不能）または `reference_min` が 0 以下の場合は算出不能
    として `None` を返す（呼び出し側は固定帯のみで判定し `regressed`/
    `improved` を断定しない旨を記録する）。

    呼び出し側（`main`）は候補専用 baseline の値列と candidate の値列を
    連結してから渡す（同一計測セッション全体の run-to-run 幅を実測帯と
    する。baseline 側の測定揺らぎだけを除外すると、candidate 側だけでは
    検出できない変動を見落とし、実際にはノイズ帯内の差を
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
    rows: list,
    section: str,
    name: str,
    unit: str,
    collected: dict,
    key: str,
    subkey: str,
    candidates: list[str],
    baseline_of: dict[str, str],
    allowed_pairs_by_candidate: dict[str, set[int]],
    ref_bands: dict[str, float | None],
) -> None:
    """1 行ぶんの baseline/candidate 比較を、候補（after/ref）ごとに算出して積む。

    候補ごとに専用 baseline を持つ（`baseline_of`。既定は `BASELINE_OF` だが
    レガシーデータではフォールバックしうる——`effective_baseline_map`
    参照）ため、baseline_min 等の列も候補ごとに独立させる（codex-review
    P1 指摘: 「出力・集約も比較対象別に分けること」）。
    """
    row: dict = {"section": section, "name": name, "unit": unit}
    any_data = False
    for candidate in candidates:
        baseline = baseline_of[candidate]
        allowed = allowed_pairs_by_candidate.get(candidate, set())
        baseline_vals = filtered_values(collected, baseline, key, subkey, allowed)
        candidate_vals = filtered_values(collected, candidate, key, subkey, allowed)
        if not baseline_vals or not candidate_vals:
            row[f"{candidate}_baseline_min"] = ""
            row[f"{candidate}_baseline_median"] = ""
            row[f"{candidate}_min"] = ""
            row[f"{candidate}_median"] = ""
            row[f"{candidate}_ratio_min"] = ""
            row[f"{candidate}_ref_band"] = ""
            row[f"{candidate}_class"] = ""
            continue
        any_data = True
        baseline_min, baseline_median = min_median(baseline_vals)
        cand_min, cand_median = min_median(candidate_vals)
        ratio = cand_min / baseline_min if baseline_min else float("nan")
        rb = ref_bands.get(candidate)
        row[f"{candidate}_baseline_min"] = f"{baseline_min:.2f}"
        row[f"{candidate}_baseline_median"] = f"{baseline_median:.2f}"
        row[f"{candidate}_min"] = f"{cand_min:.2f}"
        row[f"{candidate}_median"] = f"{cand_median:.2f}"
        row[f"{candidate}_ratio_min"] = f"{ratio:.4f}"
        row[f"{candidate}_ref_band"] = f"{rb * 100:.2f}%" if rb is not None else "n/a"
        row[f"{candidate}_class"] = classify(ratio, ref_band=rb)
    if any_data:
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
                "(mixing sessions makes baseline/candidate min-of-N incomparable — "
                "Issue #633 codex-review P2 指摘)",
                file=sys.stderr,
            )
            return 2
        session_ts = sessions[0]

    crossdb = collect_crossdb(dir_path, session_ts)
    hybrid = collect_hybrid(dir_path, session_ts)

    # collect_hybrid の構造は arm→mode→{...} のため、候補の有無は
    # arm 自体の存在で判定する（crossdb 判定と対称にするため hybrid も
    # 同様に arm キーの有無で見る）。
    candidates = [c for c in CANDIDATE_ORDER if c in crossdb or c in hybrid]
    if not candidates:
        print("ERROR: no after/ref-arm data found", file=sys.stderr)
        return 2

    # 候補ごとの実効 baseline arm 名（レガシーデータへのフォールバックを
    # 含む。`effective_baseline_map` 参照）。crossdb・hybrid は別データ
    # セットのため個別に決定する。
    eff_crossdb_baseline = effective_baseline_map(crossdb)
    eff_hybrid_baseline = effective_baseline_map(hybrid)

    # 候補ごとに専用 baseline との対応済み run ペア（積集合）を求める
    # （codex-review P1 指摘: 候補ごとに専用 baseline。P2 指摘・2 巡目:
    # 対応しない余剰 run は集計対象から除外する）。crossdb self は
    # 「全 phase に共通の pair 集合」ではなく phase ごとに個別で良いが、
    # 実運用では 1 run が全 phase を含むため、代表として
    # `REFERENCE_BAND_PHASE` の pair 集合を候補の対応判定に用いる
    # （crossdb・hybrid 各モードは `emit_row` 呼び出し時に該当 key で
    # 再度 `common_pairs_for` を計算するため、phase ごとの部分欠損にも
    # 対応する）。
    crossdb_common: dict[str, set[int]] = {}
    for candidate in candidates:
        baseline = eff_crossdb_baseline[candidate]
        crossdb_common[candidate] = common_pairs_for(crossdb, baseline, candidate, REFERENCE_BAND_PHASE)

    hybrid_common: dict[str, dict[str, set[int]]] = {}
    for mode in HYBRID_MODES:
        hybrid_common[mode] = {}
        for candidate in candidates:
            baseline = eff_hybrid_baseline[candidate]
            hybrid_common[mode][candidate] = common_pairs_for(hybrid, baseline, candidate, mode, "p50")

    # N ≥ 5 ペア・候補ごとの baseline/candidate 対応を検証する（codex-review
    # P2 指摘）。不完全なデータのまま判定へ進まず拒否する（fail-closed）。
    pair_errors: list[str] = []
    for candidate in candidates:
        if candidate not in crossdb:
            continue
        matched = crossdb_common[candidate]
        if len(matched) < MIN_PAIRS:
            baseline = eff_crossdb_baseline[candidate]
            pair_errors.append(
                f"crossdb {baseline}/{candidate}: matched run pairs={len(matched)} (< {MIN_PAIRS} required); "
                f"{baseline} runs={sorted(arm_pairs(crossdb, baseline, REFERENCE_BAND_PHASE, 'p50'))} "
                f"{candidate} runs={sorted(arm_pairs(crossdb, candidate, REFERENCE_BAND_PHASE, 'p50'))}"
            )
    for mode in HYBRID_MODES:
        for candidate in candidates:
            if candidate not in hybrid or mode not in hybrid.get(candidate, {}):
                continue
            matched = hybrid_common[mode][candidate]
            if len(matched) < MIN_PAIRS:
                baseline = eff_hybrid_baseline[candidate]
                pair_errors.append(
                    f"hybrid[{mode}] {baseline}/{candidate}: matched run pairs={len(matched)} "
                    f"(< {MIN_PAIRS} required); "
                    f"{baseline} runs={sorted(arm_pairs(hybrid, baseline, mode, 'p50'))} "
                    f"{candidate} runs={sorted(arm_pairs(hybrid, candidate, mode, 'p50'))}"
                )
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

    # 実測ノイズ帯（reference_band）: 候補専用 baseline と candidate の
    # `REFERENCE_BAND_PHASE`（`vector_knn.p50`）run-to-run 値列を、対応済み
    # run ペアに限定したうえで連結して算出する（`docs/design/
    # benchmark-judgement-policy.md` §4「同一計測セッションで得た参照区間の
    # run-to-run 幅」・codex-review P1 指摘）。
    ref_bands: dict[str, float | None] = {}
    for candidate in candidates:
        baseline = eff_crossdb_baseline[candidate]
        allowed = crossdb_common.get(candidate, set())
        baseline_vals = filtered_values(crossdb, baseline, REFERENCE_BAND_PHASE, "p50", allowed)
        candidate_vals = filtered_values(crossdb, candidate, REFERENCE_BAND_PHASE, "p50", allowed)
        ref_bands[candidate] = reference_band(baseline_vals + candidate_vals)

    print(f"# session_ts={session_ts}", file=sys.stderr)
    for candidate in candidates:
        baseline = eff_crossdb_baseline[candidate]
        allowed = crossdb_common.get(candidate, set())
        baseline_vals = filtered_values(crossdb, baseline, REFERENCE_BAND_PHASE, "p50", allowed)
        candidate_vals = filtered_values(crossdb, candidate, REFERENCE_BAND_PHASE, "p50", allowed)
        combined = baseline_vals + candidate_vals
        rb = ref_bands.get(candidate)
        if combined:
            print(
                f"# reference_band[{baseline}+{candidate}] {REFERENCE_BAND_PHASE}.p50: "
                f"min={min(combined):.2f} max={max(combined):.2f} "
                f"band={'n/a' if rb is None else f'{rb * 100:.2f}%'}",
                file=sys.stderr,
            )
        else:
            print(f"# reference_band[{baseline}+{candidate}] {REFERENCE_BAND_PHASE}.p50: no data", file=sys.stderr)

    rows: list[dict] = []
    for phase in CROSSDB_PHASES + CROSSDB_REFERENCE_PHASES:
        section = "reference" if phase in CROSSDB_REFERENCE_PHASES else "crossdb"
        allowed_by_candidate = {
            c: common_pairs_for(crossdb, eff_crossdb_baseline[c], c, phase, "p50") for c in candidates
        }
        emit_row(
            rows,
            section,
            f"{phase}.p50",
            "us",
            crossdb,
            phase,
            "p50",
            candidates,
            eff_crossdb_baseline,
            allowed_by_candidate,
            ref_bands,
        )
        allowed_by_candidate_p95 = {
            c: common_pairs_for(crossdb, eff_crossdb_baseline[c], c, phase, "p95") for c in candidates
        }
        emit_row(
            rows,
            section,
            f"{phase}.p95",
            "us",
            crossdb,
            phase,
            "p95",
            candidates,
            eff_crossdb_baseline,
            allowed_by_candidate_p95,
            ref_bands,
        )

    for mode in HYBRID_MODES:
        allowed_p50 = {c: common_pairs_for(hybrid, eff_hybrid_baseline[c], c, mode, "p50") for c in candidates}
        emit_row(
            rows, "hybrid_loop", f"{mode}.p50", "us", hybrid, mode, "p50", candidates, eff_hybrid_baseline, allowed_p50, ref_bands
        )
        allowed_p95 = {c: common_pairs_for(hybrid, eff_hybrid_baseline[c], c, mode, "p95") for c in candidates}
        emit_row(
            rows, "hybrid_loop", f"{mode}.p95", "us", hybrid, mode, "p95", candidates, eff_hybrid_baseline, allowed_p95, ref_bands
        )
        for rss_key in ("rss_start", "rss_after_warm", "rss_end"):
            allowed_rss = {
                c: common_pairs_for(hybrid, eff_hybrid_baseline[c], c, mode, rss_key) for c in candidates
            }
            emit_row(
                rows,
                "hybrid_loop_rss",
                f"{mode}.{rss_key}",
                "MiB",
                hybrid,
                mode,
                rss_key,
                candidates,
                eff_hybrid_baseline,
                allowed_rss,
                ref_bands,
            )

    header = ["section", "name", "unit"]
    for candidate in candidates:
        header += [
            f"{candidate}_baseline_min",
            f"{candidate}_baseline_median",
            f"{candidate}_min",
            f"{candidate}_median",
            f"{candidate}_ratio_min",
            f"{candidate}_ref_band",
            f"{candidate}_class",
        ]

    print("\t".join(header))
    for row in rows:
        print("\t".join(str(row.get(h, "")) for h in header))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
