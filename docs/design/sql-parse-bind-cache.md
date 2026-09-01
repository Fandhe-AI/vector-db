# SQL パース・束縛結果のセッション内キャッシュ検討

- ステータス: Accepted（実測に基づき見送り。Phase 1（実測ベンチ・回帰テスト）のみ
  実装、Phase 2（キャッシュ本体）は導入しない）
- 対応: Issue #360「perf(engine): SQL パース・束縛結果のセッション内キャッシュ検討
  （実測裏付け前提）」
- 前提: 対象ビヘイビア ID なし（本 Issue は特定ビヘイビアの実装ではなく、性能
  最適化の導入可否を実測で判断する検討タスク）

## 背景

同一 SQL 文字列を同一セッションで再実行しても、毎回「字句解析 → 許可リスト
構文解析 → スキーマ照合束縛」をやり直す。PostgreSQL の plan cache / SQLite の
prepared statement に相当する「セッション単位の SQL 文字列 → 束縛結果 LRU
キャッシュ」の導入価値を検討する Issue である。Issue 本文は「パース費用は ms
オーダーの p50 に対して小さいと推定されるため、先に実測し、効果が薄ければ見送り
判断を記録して close してよい」と明記しており、実測ファーストで判断する。

## 事前登録した判断基準

`crates/engine/benches/sql_parse_bind_bench.rs`（`make bench-parse-bind`）で
以下の 3 系列を実測し、「(b) の中央値 / (c) の中央値」を比率とする。

- (a) `validate_sql`（字句解析＋許可リスト構文解析＋ FROM テーブル存在確認）単体
- (b) (a) ＋ スキーマ取得（`Storage::get_table_schema`）＋
  `sql::parser::bind_in_session`（＝キャッシュヒットで省略可能になる総コストの近似）
- (c) `EngineCore::execute_sql`（SQL 表層フル実行。走査・実行を含む）

判定基準（Issue #360 の事前登録基準）:

- 現実的構成（10,000 行・768 次元・k=20）で比率 ≥ 0.05 → 導入検討（Phase 2a）
- 上記未満 → 見送り（Phase 2b）
- 小規模構成のみ境界的に高い場合 → 絶対値を併記し安全側（見送り）に倒す

## 実測結果

開発環境（Linux／x86_64／12 論理コア／AVX2+FMA 検出／`redb` バックエンド）で
`make bench-parse-bind` を 3 run 実行した（`MeasurementConfig` は warmup 20 回・
計測 50 回、`sql_c1_bench.rs` と同一のクエリ形状〔SQL-1 純粋 Top-k・768 次元
ベクトルリテラル・LIMIT 20〕）。

| 構成 | run | validate_sql 中央値 | (a)+bind 中央値 | フル実行中央値 | 比率 |
| ---- | --- | -------------------- | ---------------- | -------------- | ---- |
| small（1,000 行） | 1 | 12.42µs | 28.09µs | 781.51µs | 0.0359 |
| small（1,000 行） | 2 | 12.40µs | 27.86µs | 780.19µs | 0.0357 |
| small（1,000 行） | 3 | 12.45µs | 28.18µs | 755.32µs | 0.0373 |
| realistic（10,000 行） | 1 | 12.48µs | 28.17µs | 20.887ms | 0.0013 |
| realistic（10,000 行） | 2 | 12.35µs | 27.80µs | 20.726ms | 0.0013 |
| realistic（10,000 行） | 3 | 12.57µs | 27.62µs | 20.755ms | 0.0013 |

run 間のばらつきは両構成とも数 % 以内で安定している。パース・束縛コスト自体
（(a)+bind ≈ 28µs）は構成（行数）に依らずほぼ一定であり、これはベクトルリテラル
パース（768 次元 ≒ 数 KiB）が支配的コストであってテーブル行数に依存しないという
実装構造と整合する。一方でフル実行コストは行数にほぼ比例して増加するため、比率は
行数が増えるほど急速に小さくなる。

## 判断

**見送り（Phase 2b）。** 事前登録基準の閾値（0.05）に対し:

- 現実的構成（10,000 行）: 比率 ≈ 0.0013（閾値の 1/38 程度）で明確に未達
- 対照の小規模構成（1,000 行。パース比率が最大化する最悪ケース）でも
  比率 ≈ 0.036〜0.037 で閾値 0.05 未満

小規模構成が閾値に漸近する「境界ケース」にすら該当せず、両構成とも明確に閾値を
下回った。Issue 本文が推定していた「パース費用は ms オーダーの p50 に対して
小さい」という見立てが実測で裏付けられた形であり、セッション内キャッシュ
（`SessionState` への LRU 追加・世代失効・`SET search_mode`/`CREATE FUNCTION`
時のクリア等の実装・検証コスト）を導入する根拠に乏しい。

production コード（`crates/engine/src/`）は本 Issue を通じて無変更。実装したのは
実測ベンチ（`benches/sql_parse_bind_bench.rs`・`benches/harness/parse_bind.rs`）と
その回帰テスト（`tests/parse_bind_bench_accept.rs`）のみ。

## 再検討条件

以下のいずれかが生じた場合、本判断を再検討する価値がある（Issue 起票はオーナー
承認事項として申し送る。[out-of-scope-tracking](../../.claude/rules/out-of-scope-tracking.md)）。

- パラメータ化クエリ（`$n`）対応が入り、prepared statement 相当の設計が必要になる
  （現状 `$n` は字句解析段で拒否されキャッシュ以前に到達しない。TASK-nn なし・
  `crates/engine/src/sql/lexer.rs` の `rejects_dollar_parameter_placeholder` 参照）
- SQL 表層の行数が小規模（1,000 行未満）に偏る高頻度短クエリ用途が主要ワーク
  ロードになる（本実測の対照構成がその領域に最も近く、閾値の 3/4 程度まで
  近づいている）
- ベクトルリテラルパースそのものの実装コストが変化する（現状の支配的コストの
  内訳が変わればパース・束縛コストの絶対値・比率双方が動く）

## 実装（Phase 1・実測入口）

| パス | 内容 |
| ---- | ---- |
| `crates/engine/benches/sql_parse_bind_bench.rs` | 3 系列の実測本体（`fn main`・`harness = false`・`test = false`） |
| `crates/engine/benches/harness/parse_bind.rs` | 比率計算・判断ゲート（0.05）・レポート整形（時間非依存） |
| `crates/engine/benches/harness/mod.rs` | `parse_bind` モジュール追加 |
| `crates/engine/tests/parse_bind_bench_accept.rs` | `harness::parse_bind` の回帰テスト（`make ci` 対象） |
| `crates/engine/Cargo.toml` | `[[bench]]` エントリ追加 |
| `Makefile` | `bench-parse-bind` ターゲット追加 |

`sql_parse_bind_bench.rs` は spec 由来の pass/fail 閾値を持たない情報提供専用
ベンチのため `.github/workflows/*` へは配線しない（`bench-hybrid`・`bench-c1` と
同一方針。手動実行専用）。実測値の記録はオーナー判断（2026-08-29・
[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)「許可される
参照」）により公開可能な範囲。
