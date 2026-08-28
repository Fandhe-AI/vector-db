# ADR: マージ済みコミットメッセージに残る spec 規則の説明的表現の扱い（PR #263）

- ステータス: Accepted（本 PR のマージをもって確定）
- 対応: Issue #284（親: #275 / #273）・TASK-139（ポインタ: `docs/spec/05-tasks.md`）
- 関連規約: `.claude/rules/spec-confidentiality.md`

## 経緯

PR #263（TASK-139・Issue #79）のレビュー対応コミット（`57381fe`・`28e85c0`）の
コミット本文に、private spec のビヘイビア定義（TABLE-12）に触れる説明的表現が
残った。除去には main の history rewrite が必要と見られ、対応未了のまま
Issue #284 として申し送りされていた。本 ADR は調査結果と「rewrite しない」
判断の根拠を記録する。問題となったコミット本文自体はここに再掲しない。

## 調査結果

| # | 事実 |
| - | ---- |
| 1 | main 上の squash コミット `aa6cb93`（`chore: TASK-139 基盤・工程管理 (#263)`）の本文は PR 本文（Summary・対象外・Cursor Bugbot 要約）由来で、TABLE-12 への言及は含まれない |
| 2 | 問題の表現は PR #263 のブランチ中間コミット `57381fe`・`28e85c0` に存在する。squash merge のため main 履歴には含まれないが、GitHub の `refs/pull/263/head`（`4e3717e`）から到達可能で、PR の Commits タブ・commit URL から今も参照できる |
| 3 | ブランチ元リモート（`chore/79-task-139-plan-rls-verification`）は削除済み |
| 4 | 当該表現は「spec にビヘイビア定義（TABLE-12）が存在し、その具体説明を削除した」というメタ記述に留まり、規則の中身自体は書かれていない。また main 履歴には既に `(tenant_id, id)` キー整合（TABLE-12）を件名に持つマージ済みコミットが複数あり、README「実装方針（要点）」にも `(tenant_id, id)` キー順の記述がオーナー公開済みである |
| 5 | branch ruleset `main-protection`（active）が非高速転送・削除禁止を含み、マージ方式は squash のみ。main への force push は不可で、rewrite には組織管理者による ruleset の一時無効化が必要 |
| 6 | `refs/pull/*` は GitHub 管理であり利用者は書き換え・削除できない。main を rewrite しても問題の中間コミットは残るため、rewrite は目的を達成しない |
| 7 | fork 数 0・public リポ。ただし clone・worktree（並列実装中のものを含む）が存在し、rewrite は全 clone の再同期を強いる |

## 判断

**history rewrite は行わない。**

根拠:

- rewrite 対象となる main 上の問題コミットが存在しない（#1）
- 問題表現の所在は GitHub 管理の PR refs にあり、rewrite の効果が及ばない（#2・#6）
- ruleset により技術的に不可であり、実施には管理者作業と全 clone・worktree の
  再同期という大きな影響が伴う（#5・#7）
- 当該表現は既に公開済みの範囲内に収まるメタ記述に留まる（#4）

history rewrite の可否そのものはオーナー判断事項だが、自動運転での実装では
承認を得られないため、上記根拠を本 ADR と PR 本文に明記し「rewrite しない」を
判断として提示する。オーナーが rewrite を選ぶ場合は本 PR をマージせず、
下記「rewrite を選ぶ場合の手順」を参照して別途実施する。

## rewrite を選ぶ場合の手順と影響範囲（参考・未実施）

1. branch ruleset `main-protection` の非高速転送制限を組織管理者が一時解除する
2. 該当コミットのメッセージを書き換える（history rewrite）
3. force push（管理者権限）
4. ruleset を復元する
5. 全 clone・worktree・open な PR ブランチを rebase し、CI を再実行する
6. `refs/pull/263/*` に残る中間コミットの除去は GitHub サポートへの別途依頼が
   必要（#6）。手順 1〜5 だけでは解決しない

fork は 0 だが、並列実装中の worktree・他の open ブランチが影響を受けるため、
実施には全 clone の再同期が前提になる。

## 再発防止

- コミットメッセージ・PR 本文も public 資産であり、`.claude/rules/spec-confidentiality.md`
  の禁止事項・ポインタ表記が同様に適用される旨を明記する（本 ADR に伴う規約追記）
- squash merge 後もブランチ上の中間コミットは PR refs で永続するため、
  history rewrite ではなく **マージ前の検査** で防ぐことを原則とする
- レビュー指摘（spec 転記）への対応コミットでは、削除内容の説明に spec 規則の
  内容・対象を再転記せず、ID ポインタと削除の事実のみを書く
- squash merge の件名・本文は PR 本文から生成されるため、PR 本文作成時の
  検査（create-pr スキルの OWASP 観点）に spec 転記の確認を含める

## 参照

- `docs/spec/05-tasks.md`（TASK-139）
- `.claude/rules/spec-confidentiality.md`
