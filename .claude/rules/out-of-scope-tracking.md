# 実装対象外の追跡規約

## 目的

実装・レビュー中にスコープ外と判断した事項を放置しない。
後で埋もれるのを防ぐため、発見したタイミングで Issue に記録して追跡可能にする。

## フロー

1. **検出**: 実装・レビュー中にスコープ外と判断した事項をメモする
2. **既存 Issue の確認**:

   ```bash
   gh issue list --search "${KEYWORD}" --state open
   ```

   キーワードは `"${KEYWORD}"` でクォートして渡す。
3. **ユーザーに承認を得る**: 既存 Issue への追記か新規起票かをユーザーと確認する
4. **記録**:
   - 既存 Issue がある場合:

     ```bash
     gh issue comment "${ISSUE_NUMBER}" --body "$(cat <<'EOF'
     （本文をここに記述）
     EOF
     )"
     ```

   - 存在しない場合: `create-issue-tree` または `create-issue` で適切な親 Issue 配下に起票
5. **元 PR・レビューに切り出し先を記録**: コメントまたは PR 本文に切り出し先の Issue 番号を記載する

## 本リポ固有の注意

- Issue は public リポに作られる。`docs/spec`（private）の本文を Issue 本文へ転記せず、TASK-nn・ビヘイビア ID のポインタ表記に留める（[spec-confidentiality](./spec-confidentiality.md)）
- spec 側（タスク定義・ロードマップ）の変更が必要と判断した事項は、本リポの Issue ではなく spec リポ側の課題としてユーザーに報告する

## 注意

- ユーザーの承認なしに勝手に Issue を起票しない
- スコープ外の修正を現在の PR に混入させない（別 Issue・別 PR で対処する）
