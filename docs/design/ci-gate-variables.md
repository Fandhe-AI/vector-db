# 回帰ベンチ・Recall ゲートの repo variables 目標値確定: 設計判断記録

- ステータス: Proposed（本コミットは variables ↔ spec ポインタの対応整理・設定手順の
  確立のみ。実際の `gh variable set`・`workflow_dispatch` による疎通確認はマージ後の
  リポジトリ管理者作業。実施後、別コミットで Accepted に更新する）
- 対応: Issue #286（`ci: 回帰ベンチ・Recall ゲートの repo variables 目標値を確定`）
- 前提: TASK-127・TASK-130・TASK-83（`.github/workflows/bench.yml`）、
  TASK-104・TASK-108・TASK-112・TASK-113（`.github/workflows/recall.yml`）。既存 ADR
  （`hybrid-recall-regression.md`・`rerank-recall-regression.md`・
  `query-planning-recall-regression.md`・`core5-contrast-engine.md`）はいずれも
  「Actions variables の実値設定はマージ後のリポジトリ管理者作業」と明記しており、
  本 ADR はその作業の実行手順・対応表を一箇所に集約するもの

## 目的

`bench.yml`・`recall.yml` は spec 由来の数値基準を GitHub Actions variables
（repo レベル／Environment `recall-gate`）から注入する設計で、未設定時は
fail-closed で red になる。調査時点（2026-08-28）でこれらの variables は
いずれも 1 件も設定されておらず、週次 schedule は「一度も評価していない run」の
まま red が続いている。

本 ADR は variable 名と spec ポインタ・受理形式（本リポの Rust 実装が検証する
範囲）の対応を一箇所にまとめ、値そのもの（spec 由来の数値）を一切含まない形で
設定手順を確立する。**実際の値の設定・`workflow_dispatch` による疎通確認は
本コミットのスコープ外**（下記「申し送り」参照。GitHub 設定変更は他の並列作業と
共有される repository-wide な状態変更であり、レビュー未了の変更として実行しない）。

## variable ↔ spec ポインタ対応（値は書かない）

値そのもの（spec 由来の数値基準）は `.claude/rules/spec-confidentiality.md`
（本リポ public・spec は private）により本 ADR を含むいかなる public 資産にも
記載しない。設定時は `docs/spec` の該当ビヘイビア ID の記述を直接参照すること。

### repo variables（`.github/workflows/bench.yml`）

| variable | spec ポインタ | 受理形式（実装の検証規則） |
| -------- | -------------- | -------------------------- |
| `BENCH_MAX_P95_MS` | `docs/spec/04-behavior/core-engine.md` CORE-3・`search.md` SEARCH-4（TASK-127） | 正の整数（ms） |
| `BENCH_MIN_RECALL` | `core-engine.md` CORE-4（TASK-127） | `(0.0, 1.0]` |
| `BENCH_MAX_CONTRAST_RATIO` | `core-engine.md` CORE-5（TASK-127。`harness/accept.rs::parse_contrast_ratio_limit`） | 有限・正の浮動小数点 |
| `BENCH_BATCH_MAX_DEGRADATION_PCT` | `core-engine.md` CORE-7 改訂＋`05-tasks.md` TASK-130（動的窓の単発 p95 劣化上限） | 0 以上の有限浮動小数点（%） |
| `BENCH_SQL_C1_MAX_P95_MS` | `sql-surface.md` SQL-1（TASK-83） | 正の整数（ms） |
| `BENCH_SQL_C1_MIN_RECALL` | `sql-surface.md` SQL-1（TASK-83。参照実装との Top-20 一致率） | `(0.0, 1.0]` |
| `BENCH_CORE6_MIN_IMPROVEMENT_PCT`（任意） | `core-engine.md` CORE-6（TASK-130） | 正の浮動小数点（%）。`BENCH_CORE6` opt-in 時のみ読まれる |
| `BENCH_CORE16_MIN_IMPROVEMENT_PCT`（任意） | `core-engine.md` CORE-16（TASK-130） | 正の浮動小数点（%）。`BENCH_CORE16` opt-in 時のみ読まれる |

前 6 変数は `bench.yml` の既定ゲート（`bench-simd`・`bench-contrast`・
`bench-batch`・`bench-c1` の各 job）が必須とするため、揃わない限り run は
fail-closed で red のままとなる。

### Environment `recall-gate` variables（`.github/workflows/recall.yml`）

| variable | spec ポインタ | 受理形式 |
| -------- | -------------- | -------- |
| `HYBRID_RECALL_MIN_R20_SMALL` | `search.md` SEARCH-1（TASK-104） | `(0.0, 1.0]` |
| `HYBRID_RECALL_MIN_R20_LARGE` | `search.md` SEARCH-2（TASK-104） | `(0.0, 1.0]` |
| `HYBRID_RECALL_MIN_R100_LARGE` | `search.md` SEARCH-2（TASK-104） | `(0.0, 1.0]` |
| `RERANK_RECALL_MIN_R20_LARGE` | `search.md` SEARCH-7（TASK-108。絶対下限） | `(0.0, 1.0]` |
| `RERANK_RECALL_MIN_R20_IMPROVEMENT` | `search.md` SEARCH-7（TASK-108。改善幅＝after − baseline。spec の pt 表記は Recall の差＝小数へ換算して設定する） | `[0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT` | `query-planning.md` PLAN-1（TASK-112。改善幅。pt → 小数換算） | `[0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_R20_DIRECT` | `query-planning.md` PLAN-2（TASK-112） | `(0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE` | `query-planning.md` PLAN-3 → `search.md` SEARCH-2 のスケール条件付き基準（TASK-113） | `(0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED` | spec に対応値なし（本リポ独自の劣化展開シナリオ用回帰基準。`docs/design/query-planning-recall-regression.md` 参照） | `[0.0, 1.0]`。導出規則は下記 |

前 8 変数（`DEGRADED` を除く）が揃わない限り `recall-regression` job は
strict モード（`*_REQUIRE_THRESHOLDS=1`）により fail-closed で red のままとなる。

#### `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED` の導出規則（暫定・オーナー確認待ち）

spec に対応する確定値が無いため、以下の規則による暫定値をオーナー確認待ちとして
提案する（値そのものは非公開のまま、規則のみを記録する）:

- 規則: `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT`（PLAN-1 の改善幅下限）×
  0.5。`NoisyLlmClient`（決定的スタブ。言い換え語彙の半数のみを正しく写像）が
  非劣化スタブの半分の改善しか生まないことに対応させた比例規則。
- 採用条件: ローカルで `make query-planning-regression`（strict フラグなし・
  4 変数のみ注入）を実行し、この暫定値のもとで `pass_degraded=true` が確認できる
  場合に限り採用する。
- 不採用時の扱い: 確認できない場合はこの変数のみ未設定のまま残し
  （strict モードにより fail-closed で red）、他 3 変数（`INTENT`・`DIRECT`・
  `DIRECT_LARGE`）には影響させない。閾値を実測に合わせて緩めることはしない。

## 意図的に据え置く事項

- **`PRECISION_EVAL_MIN_TOP1_ACC` / `PRECISION_EVAL_MIN_MRR10` /
  `PRECISION_EVAL_MAX_FALSE_RETURN`（TASK-163・SEARCH-10）は設定しない**。
  spec（SEARCH-10）が「目標値確定まで必達基準に含めない」「確定はユーザー確認」と
  定めており、`docs/design/precision-eval-regression.md`・README も同じ申し送りを
  持つ。仮置き値を週次 strict ゲートへ入れることは spec の申し送りと矛盾するため、
  安全側（設定しない・`recall.yml` へ未接続のまま）に倒す。
- **TASK-116（ティア別レイテンシ・PLAN-4/6/7）は対象外**。常駐 Ollama が必要で
  GitHub ホステッド runner に CI 経路が無く、repo variables でもない
  （`BENCH_TIER_*` は `make bench-tier` の実行時 env）。`docs/design/
  tier-latency-acceptance.md` の目標値確定はオーナーが承認済み計測環境で実施する。
- **`BENCH_CORE6` / `BENCH_CORE16` の opt-in フラグは有効化しない**。GitHub
  ホステッド runner に GPU が無く、有効化すると必ず `pass=false` で red になる。
  下限値の 2 変数（`BENCH_CORE6_MIN_IMPROVEMENT_PCT` / `BENCH_CORE16_MIN_IMPROVEMENT_PCT`）
  は opt-in しない限り読まれないため、先行設定するかは管理者判断に委ねる。
- **`bench-c1` の Conditional Go 条件7 判定（`BENCH_DEDICATED_ENV=1`）は有効化
  しない**。GitHub ホステッド runner は専有環境の自己申告に該当せず、
  `docs/design/c1-p95-dedicated-env-reverification.md` の既定方針どおり運用者が
  専有環境で直接実行する。

## 設定手順（マージ後・リポジトリ管理者作業）

値をコマンドライン引数・シェル履歴・ファイル（リポジトリ配下）に残さないため、
必ず stdin 経由で `gh variable set` に渡す。

```bash
# repo variables（bench.yml）
printf '%s' "<spec 値>" | gh variable set BENCH_MAX_P95_MS
printf '%s' "<spec 値>" | gh variable set BENCH_MIN_RECALL
printf '%s' "<spec 値>" | gh variable set BENCH_MAX_CONTRAST_RATIO
printf '%s' "<spec 値>" | gh variable set BENCH_BATCH_MAX_DEGRADATION_PCT
printf '%s' "<spec 値>" | gh variable set BENCH_SQL_C1_MAX_P95_MS
printf '%s' "<spec 値>" | gh variable set BENCH_SQL_C1_MIN_RECALL

# Environment recall-gate variables（recall.yml）
printf '%s' "<spec 値>" | gh variable set HYBRID_RECALL_MIN_R20_SMALL --env recall-gate
printf '%s' "<spec 値>" | gh variable set HYBRID_RECALL_MIN_R20_LARGE --env recall-gate
printf '%s' "<spec 値>" | gh variable set HYBRID_RECALL_MIN_R100_LARGE --env recall-gate
printf '%s' "<spec 値>" | gh variable set RERANK_RECALL_MIN_R20_LARGE --env recall-gate
printf '%s' "<spec 値>" | gh variable set RERANK_RECALL_MIN_R20_IMPROVEMENT --env recall-gate
printf '%s' "<spec 値>" | gh variable set QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT --env recall-gate
printf '%s' "<spec 値>" | gh variable set QUERY_PLANNING_RECALL_MIN_R20_DIRECT --env recall-gate
printf '%s' "<spec 値>" | gh variable set QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE --env recall-gate
# DEGRADED は上記「採用条件」を満たした場合のみ設定する
printf '%s' "<暫定値>" | gh variable set QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED --env recall-gate

gh variable list
gh variable list --env recall-gate
```

設定後は必ず以下を確認する:

1. `gh api repos/Fandhe-AI/vector-db/environments/recall-gate/deployment-branch-policies`
   で branch policy が `main` のみに制限されたままであること
   （`hybrid-recall-regression.md` の実行境界設計を参照）
2. `gh workflow run recall.yml --ref main` / `gh workflow run bench.yml --ref main`
   を実行し、`gh run view <id>` で各 job・step が **skip ではなく実行**され、
   strict モード（`*_REQUIRE_THRESHOLDS=1`）で pass したことを `pass=`/`pass_*=`
   行（非数値の判定結果のみ）で確認する。閾値未達（red）の場合は spec 値を変更
   せず、pass/fail の状態のみを記録する（fail-closed を維持する）
3. ログ全文（`gh run view --log`）は保存・転記しない。閾値・実測値を public
   資産（PR・commit・docs・Issue）へ書かない

## 申し送り（本コミットのスコープ外）

- repo variables（6 件必須＋2 件任意）の実値設定
- Environment `recall-gate` variables（8 件必須＋`DEGRADED` 暫定値）の実値設定
- `workflow_dispatch` による strict モード疎通確認と run の記録
- `PRECISION_EVAL_*`（TASK-163・SEARCH-10）目標値確定と `recall.yml` への接続
- TASK-116（PLAN-4/6/7）の `make bench-tier` 実測と ADR の Accepted 化
- `BENCH_CORE6` / `BENCH_CORE16` の GPU 搭載ホストでの opt-in 有効化
- `bench-c1`（TASK-83 条件7）の専有環境での最終判定
