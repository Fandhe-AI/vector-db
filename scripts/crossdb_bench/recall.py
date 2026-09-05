"""Recall@10 の正解集合（ground truth）を作る配線検算ユーティリティ。

`docs25k.jsonl` から RLS 相当で可視な行の embedding を numpy に乗せ、内積
（自作 DB の `<=>` と同じ指標。`crates/engine/src/kernel.rs` の「スコアは
内積」に合わせる）で 200 クエリそれぞれの正解を計算する。

可視性モデル（実機確認済みの現行契約）: 自作 DB はどのテナントの wire
セッションからも `visibility = 'public'` の行のみが可視であり、private 行は
所有テナント自身のセッションからも不可視（`docs25k.jsonl`: tenant-a 行 =
public 23,000・tenant-b 行 = private 2,000。tenant-a・tenant-b いずれの
セッションで `COUNT(*)` しても 23,000 になることを実機で確認済み）。正解集合は
tenant を問わず `visibility == "public"` の行のみから作る。`visibility`
フィールドを持たない旧フィクスチャとの互換のため、ファイル全体に一つも
`visibility` キーが無い場合のみ従来どおり tenant-a 一致で判定する。

同点許容の Recall@10: 実測データには同一スコアの文書が多数存在し（合成
embedding の構造上、内積が完全一致するグループができやすい）、「厳密な
上位 10 件」の境界がどの 10 件を含むかは同点内の順序規約に依存してしまう
（exact 構成でも DB 間で一致しない）。そのため正解集合を「真の内積スコアが
10 位のスコア以上の全文書」（同点を全て含む）へ広げ、返却 10 件のうち
その集合に含まれる件数 / 10 を recall とする（`recall_at_k_tie_tolerant`）。
従来の厳密一致（同点は決定的だが恣意的な順序で 10 件に切る）は
`recall_at_k`／`recall_at_10_strict` として引き続き提供する。

同点判定には小さな許容誤差（`TIE_EPSILON`）を設ける。数学的には同一の
内積値でも、DB ごとに float32 の総和順序が異なると計算結果が最終桁で
ずれる（実機確認: pgvector と numpy で同一の意味的な同点グループのうち
1 件だけ `0.4850712716579437` と `0.48507124185562134` のように 1e-7 台で
食い違い、許容誤差なしの厳密な `>=` 比較では同点集合から漏れる）。
`TIE_EPSILON = 1e-4` は float32 の総和誤差（次元 128 程度で概ね 1e-6〜1e-7）
より十分大きく、かつこのフィクスチャで観測された「真に異なるスコア間の
最小差」（1e-2 オーダー）より十分小さい値として選んだ。
"""

from __future__ import annotations

import numpy as np

from common import TENANT_VISIBLE, load_jsonl

# float32 の総和順序差に起因する同点境界の見落としを防ぐための許容誤差。
# モジュール docstring の「同点判定には小さな許容誤差」を参照。
TIE_EPSILON = 1e-4


def _visible_mask(docs: list[dict]) -> list[bool]:
    has_visibility_field = any("visibility" in d for d in docs)
    if has_visibility_field:
        return [d.get("visibility") == "public" for d in docs]
    # 旧フィクスチャ互換（visibility 未導入時のみ）: 従来どおり tenant-a 一致。
    return [d.get("tenant") == TENANT_VISIBLE for d in docs]


def build_ground_truth(
    docs_path: str, queries: list[dict], k: int = 10
) -> tuple[list[list[int]], list[set[int]]]:
    """`visibility == "public"` な行に対する内積正解を各クエリについて返す。

    戻り値は (strict_top_k, tie_inclusive_sets) のタプル。
    - strict_top_k: 各クエリの厳密な上位 k 件 id リスト（同点は決定的な順序
      ——`np.argsort` の安定ソートにより可視行の元の並び順、`docs25k.jsonl`
      内の出現順——で切る。従来の `recall_at_10_strict` 用）。
    - tie_inclusive_sets: 各クエリの「真のスコアが k 位のスコア以上」の
      全文書 id 集合（同点を全て含む。`recall_at_k_tie_tolerant` 用）。
    """
    docs = load_jsonl(docs_path)
    mask = _visible_mask(docs)
    visible = [d for d, m in zip(docs, mask) if m]
    strict_top_k: list[list[int]] = []
    tie_sets: list[set[int]] = []
    if not visible:
        # 可視行が無い場合は行列積へ進まない（`np.array([])` は形状 (0,) になり
        # `mat @ qv` が次元不一致で失敗する）。各クエリの正解集合は空。
        return [[] for _ in queries], [set() for _ in queries]
    ids = np.array([d["id"] for d in visible], dtype=np.int64)
    mat = np.array([d["embedding"] for d in visible], dtype=np.float32)  # (N, dim)

    for q in queries:
        qv = np.array(q["embedding"], dtype=np.float32)
        scores = mat @ qv  # 内積
        # 安定ソート（同点は元の並び順を保つ）で降順（大きいほど上位）にする。
        order = np.argsort(-scores, kind="stable")
        top_idx = order[: min(k, len(order))]
        strict_top_k.append([int(ids[i]) for i in top_idx])

        if len(scores) == 0:
            tie_sets.append(set())
            continue
        kth_pos = min(k, len(scores)) - 1
        kth_score = scores[order[kth_pos]]
        tie_mask = scores >= (kth_score - TIE_EPSILON)
        tie_sets.append({int(x) for x in ids[tie_mask]})
    return strict_top_k, tie_sets


def recall_at_k(returned_ids_per_query: list[list[int]], ground_truth: list[list[int]]) -> float:
    """厳密一致版 Recall@k（従来どおり）。returned と ground_truth（固定長リスト）
    を突き合わせ平均 Recall@k を返す。"""
    if not ground_truth:
        return float("nan")
    scores = []
    for returned, truth in zip(returned_ids_per_query, ground_truth):
        if not truth:
            continue
        hit = len(set(returned) & set(truth))
        scores.append(hit / len(truth))
    if not scores:
        return float("nan")
    return sum(scores) / len(scores)


def recall_at_k_tie_tolerant(
    returned_ids_per_query: list[list[int]], tie_sets: list[set[int]], k: int = 10
) -> float:
    """同点許容版 Recall@k。返却 id 列のうち「k 位のスコア以上の全文書」集合に
    含まれる件数を、strict 版（`recall_at_k`）と同じ分母定義——
    `min(k, 可視文書数)`——で割った値の平均を返す（同点境界の順序差を許容する）。

    可視文書数が k 未満の場合、`tie_set` は可視文書全件（`build_ground_truth`
    が `kth_score` を可視件数-1 番目のスコアとして計算するため tie_mask が
    全件に一致する）と一致するため `len(tie_set)` がそのまま可視文書数になる。
    可視文書数が k 以上の場合は `tie_set` の要素数が k 以上になるため
    `min(k, len(tie_set))` は k に収束する。つまり `min(k, len(tie_set))` は
    常に strict 版の分母 `min(k, 可視文書数)` と一致する（両版の分母定義を
    同一にする補正。詳細はモジュール docstring の「同点許容の Recall@10」節）。
    """
    if not tie_sets:
        return float("nan")
    scores = []
    for returned, tie_set in zip(returned_ids_per_query, tie_sets):
        if not tie_set:
            continue
        denom = min(k, len(tie_set))
        if denom == 0:
            continue
        hit = len(set(returned) & tie_set)
        scores.append(min(hit, denom) / denom)
    if not scores:
        return float("nan")
    return sum(scores) / len(scores)


if __name__ == "__main__":
    # 自己テスト: 可視 9 行・k=10 のとき strict/tie-tolerant 両版の分母が
    # min(k, 可視件数) = 9 に揃い、可視 9 行を全件返せば両版とも 1.0 になる
    # ことを確認する（codex-review P2 指摘の分母不整合の再発防止）。
    visible_ids = list(range(9))
    ground_truth = [visible_ids]
    tie_sets = [set(visible_ids)]
    returned = [visible_ids]

    strict = recall_at_k(returned, ground_truth)
    tie_tolerant = recall_at_k_tie_tolerant(returned, tie_sets, k=10)
    assert strict == 1.0, f"strict recall expected 1.0, got {strict}"
    assert tie_tolerant == 1.0, f"tie-tolerant recall expected 1.0, got {tie_tolerant}"
    print("recall.py self-test OK: visible=9, k=10 -> strict=1.0, tie_tolerant=1.0")
