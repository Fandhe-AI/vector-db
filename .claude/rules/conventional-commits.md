# Conventional Commits 規約

## 形式

```
<type>(<scope>): <日本語の説明>

<本文（任意・日本語）>
```

commitlint（`commitlint.config.mjs`）が CI・フックで検証する。

## type

| type | 用途 |
| ---- | ---- |
| feat | 機能追加 |
| fix | バグ修正 |
| refactor | 挙動を変えないコード整理 |
| perf | 性能改善 |
| test | テストの追加・修正 |
| docs | ドキュメントのみの変更 |
| ci | CI 設定の変更 |
| build | ビルド・依存関係の変更 |
| chore | 上記以外の雑務 |

## scope

| scope | 対象 |
| ----- | ---- |
| engine | `crates/engine` |
| wire | `crates/wire-server` |
| spec | `docs/spec` submodule 参照の更新 |
| skills | `.claude/skills`・skills-lock.json |
| （スキル名等） | 対象が明確な場合は個別名も可 |

## breaking change

- 破壊的変更は `!` を付け（例: `feat(wire)!: ...`）、本文に `BREAKING CHANGE:` を記載する

## 禁止事項

- `git commit --no-verify` の使用（pre-commit / commit-msg フックを必ず通す）
- 複数の関心事を 1 コミットに混在させること（type が 2 つ以上必要なら分割する）
- スコープ外の変更の混入（[out-of-scope-tracking](./out-of-scope-tracking.md)）
