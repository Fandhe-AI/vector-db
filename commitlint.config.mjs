// commitlint 設定。
//
// CI では Fandhe-AI/actions の lint-docs reusable workflow（commitlint）が
// `--extends @commitlint/config-conventional` 付きで PR の commit 範囲を検証し、
// 本ファイルのルールが extends 側を上書きする（agent-cli-skills の同名設定と同方針）。
export default {
  rules: {
    // 日本語 subject は「Claude Code スキル体系を導入」のように英大文字始まりの
    // 固有名詞・識別子で始まることが多く、config-conventional の subject-case
    // （sentence-case 等の禁止）と構造的に衝突するため大文字小文字の検査は無効化する
    'subject-case': [0],
  },
  // `git merge --no-edit`（origin/main 取り込み）が生成する既定のマージコミット
  // メッセージ（「Merge branch '...' into ...」等）は commitlint の
  // `defaultIgnores` により既に対象外だが、過去に本リポで使われていた
  // `merge: origin/main を取り込み` 形式（type-enum に無い `merge` を type として
  // 使う）はこのデフォルトパターンに一致せず type-enum で fail していた
  // （PR #249 CI 指摘）。履歴の書き換え（reword）は共有済みブランチの force-push を
  // 要し安全側でないため行わず、代わりにこの既存パターンの merge コミットのみを
  // commitlint の検証対象から明示的に除外する。以降の base 取り込みコミットは
  // 通常の Conventional Commits 形式（例: `chore(engine): base ブランチの変更を
  // 取り込む`）を使うため、本 ignore は過去コミットの後方互換のためだけに残す。
  ignores: [(commit) => /^merge:\s/i.test(commit)],
};
