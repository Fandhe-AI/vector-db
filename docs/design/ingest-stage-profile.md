# ingest 経路の段別内訳プロファイル

- ステータス: Accepted（本コミットで計測ベンチを追加。段別内訳は本ベンチの
  実測値であり、production コード〔`crates/engine/src/`〕は無変更）
- 対応: Issue #396（`test(engine): ingest 段別プロファイルベンチ（encode／
  content_hash／redb insert／commit）を追加`）
- 関連: Issue #356（hybrid 段別内訳。`docs/design/hybrid-rrf-latency-breakdown.md`）・
  Issue #362（KNN 段別内訳。`docs/design/knn-stage-profile.md`）と同型の
  手動専用・情報提供専用ベンチ

## 背景

書き込み経路（`crates/engine/src/tenant.rs::insert_rows` → `insert_rows_unchecked`）
には専用ベンチが無く、`crates/engine/examples/feature_bench.rs` の `ingest`
フェーズ（`insert_rows` を 1,000 行バッチ × dim 128 で連続投入し p50/p95 を出す）
が唯一の計測手段だった。`insert_rows_unchecked` の内部は「所有権検査 → バッチ内
id 重複検出 → `begin_write` → スキーマ取得 → content_hash（内部で全行を
`encode_row` した上で SHA-256）→ 台帳記録（`op_ledger` get+insert・`last_op`
insert）→ 行ループ（次元検証 → `encode_row` → redb insert）→ 世代更新 → commit」
の直列構成で、どの段が支配的かの実測が存在しなかった。本ベンチはその内訳を実測し、
後続の書き込み最適化検討の優先判断材料とする。

## 測定設計

`insert_rows_unchecked` の内部段（`storage::encode_row`・`recovery::content_hash::
for_insert_batch`・`recovery::ledger::record_in_txn` の台帳エントリ符号化）は
いずれも `pub(crate)` で、独立コンパイル単位であるベンチ（`crates/engine/
benches/`）から直接呼べない。KNN 段別内訳（Issue #362）と同じ制約のため、同じ
3 手法の組み合わせで分解する:

1. **pub API の e2e 計測（E0）**: `engine::tenant::insert_rows`（`feature_bench`
   と同じ入口）を丸ごと計測する。
2. **生 `redb::Database`（既存 dev-dependency `=4.2.0`）による計装レプリカ
   （I1〜I8）**: `insert_rows_unchecked` の各段を、同名テーブル
   （`user_rows/docs`・`op_ledger`・`last_op`・`table_generation`）に対して
   同順序で再現し、段ごとに `Instant` で区切って計測する。
3. **ベンチ内再実装（ドリフト検出テスト付き）**: 行フォーマット v2 の encode・
   content_hash（自作 SHA-256 ＋ 台帳値レイアウト）を `benches/harness/
   ingest_profile.rs` に再実装する。

### 段の定義

| 段 | 内容 | 実現手段 |
| --- | --- | --- |
| I1 | 所有権検査（`PolicyContext::is_owner` 全件）＋ バッチ内 id 重複検出 | pub API（`is_owner`）＋ std（`HashSet`） |
| I2 | `begin_write` | 生 redb |
| I3 | content_hash（全行の encode 再実装 ＋ SHA-256 再実装） | 再実装 |
| I4 | 台帳記録（`op_ledger` get+insert v2 値・`last_op` insert） | 生 redb |
| I5 | encode（行ループ内。実経路は I3 と I5 で **encode が 2 回**走る） | 再実装 |
| I6 | redb insert（`insert` の戻り値が `None` であることを検査。`insert_unique_row` 相当） | 生 redb |
| I7 | 世代更新（`table_generation` get→checked_add→insert） | 生 redb |
| I8 | `commit`（生 `WriteTransaction::commit`。`recovery::commit_boundary` の panic ガード・abort 機構は含まない） | 生 redb |
| 残差 | E0 − Σ(I1..I8)。スキーマ取得（カタログデコード）・`commit_boundary` ガード・抽象化コストに相当する | 算出値 |

E0・レプリカのいずれも warmup 20 バッチ ＋ 計測 20 バッチ ＝ 40 バッチ
（既定 1,000 行/バッチ ＝ 40,000 行）を同一テーブルへ連続投入する成長テーブル
条件で計測する（`harness::protocol::MeasurementConfig` の下限検証を利用）。
E0・レプリカは別々の一時 DB へ書き込み、入力データ（id 範囲・可視性・埋め込み・
metadata）はバッチ番号から `DeterministicRng` で毎回再生成し両者で内容を一致させる
（メモリには保持しない）。可視性は `feature_bench` と同様、10 件に 1 件を
`Private` にする。スキーマは `embedding VECTOR(dim)` ＋ `body TEXT` の 2 列。

### 複製近似の限界

- レプリカはカタログ層（`Storage::create_table`・スキーマ取得・
  `commit_boundary` の panic ガード）を再現しない。E0 の `table_generation` は
  `create_table` 自体の 1 回分を含むため、レプリカ（40）より 1 大きい値
  （41）になる（下記「実測結果」参照）。
- I8（commit）は生 `WriteTransaction::commit()` のみを計測し、
  `recovery::commit_boundary::commit` が付加する
  `PostCommitPanicGuard`・`COMMIT_PENDING_RESPONSE` 記録のコストは含まない
  （残差側に含まれる）。

### 整合性検証（fail-closed）

1. 計測後、E0 DB・レプリカ DB の `user_rows/docs` 全エントリがバイト単位で一致し、
   件数が投入バッチ数 × 行数と一致すること（encode 再実装のドリフト検出）。
2. E0 DB の `op_ledger` エントリ（計測フェーズの各バッチ分）を復号し、
   content_hash 再実装の結果と一致すること（content_hash 再実装のドリフト検出）。
3. E0 DB・レプリカ DB それぞれの `table_generation["docs"]` が期待値
   （E0 は投入バッチ数 + 1〔`create_table` 分〕、レプリカは投入バッチ数）と
   一致すること。
4. I6 段の `insert` 戻り値が常に `None`（新規行）であること。

いずれかが不一致ならベンチはエラー終了し測定値を出力しない。

### env による可変化

- `BENCH_INGEST_PROFILE_ROWS`: 1 バッチの行数。既定 1,000・許容 1..=10,000。
- `BENCH_INGEST_PROFILE_DIM`: 次元。既定 128・許容 1..=4,096。

未設定は既定値、空文字・非数値・範囲外は fail-closed に拒否する
（`harness::ingest_profile::parse_bounded_env`）。

### 測定入口

`crates/engine/benches/ingest_profile_bench.rs`（時間依存の実測本体）・
`crates/engine/benches/harness/ingest_profile.rs`（段別ロジック・encode/
content_hash/台帳値再実装などの時間非依存ロジック）・`crates/engine/tests/
ingest_profile_accept.rs`（`make ci` からの回帰検証。encode 再実装のバイト単位
一致・content_hash 再実装の `op_ledger` 一致・SHA-256 の FIPS 180-4 テスト
ベクタ・env 解析の境界値をカバーする）。`make bench-ingest-profile` から実行する。
spec 由来の pass/fail 閾値を持たない情報提供専用のため `.github/workflows/*` へは
配線しない（`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する）。

## 実測結果

開発環境（Linux・x86_64・12 論理コア・AVX2FMA。専有環境ではなく通常の開発機。
`loadavg` は実行時のもの）で `make bench-ingest-profile`（既定 rows=1,000・
dim=128）を 3 回実行した中央値:

| 段 | median (ms/1,000 行) | ns/行 |
| --- | --- | --- |
| I1 (precheck) | 0.015 | 15.0 |
| I2 (begin_write) | 0.001 | 0.8 |
| I3 (content_hash) | 1.70〜1.73 | 約 1,700 |
| I4 (ledger) | 0.008〜0.009 | 約 8〜9 |
| I5 (encode) | 0.09〜0.11 | 約 90〜115 |
| I6 (redb insert) | 0.81〜0.83 | 約 810〜830 |
| I7 (generation bump) | 0.002 | 約 2 |
| I8 (commit) | 0.32〜0.38 | 約 320〜380 |
| Σ(I1..I8) | 2.96〜3.07 | 約 2,960〜3,070 |
| E0 (`insert_rows` e2e) | 3.58〜3.77 | 約 3,580〜3,770 |
| 残差 (E0 − Σ) | 0.63〜0.71 | 約 630〜710 |

整合性検証はすべて green（`user_rows/docs` バイト単位一致・件数 40,000、
`table_generation` = 41 (E0) / 40 (レプリカ)、`op_ledger` content_hash 一致）。

`BENCH_INGEST_PROFILE_ROWS=200 BENCH_INGEST_PROFILE_DIM=64` での 1 点確認、
`BENCH_INGEST_PROFILE_ROWS=0`・`=abc` での fail-closed 拒否、
`GITHUB_ACTIONS=true` での起動直後拒否をいずれも確認済み。

### feature_bench `ingest` フェーズとの整合（A1）

同一開発環境で `cargo run --release -p engine --example feature_bench` を実行し、
`ingest` フェーズ（25 バッチ・1,000 行/バッチ・dim128・テナント A/B 混在）の
実測値と本ベンチの E0（40 バッチ・1,000 行/バッチ・dim128・単一テナント）を比較する:

| 指標 | feature_bench `ingest.p50_us` | 本ベンチ E0 median |
| --- | --- | --- |
| 実測値 | 4,477 µs/1,000 行 | 3,583〜3,773 µs/1,000 行 |
| 比率（本ベンチ/feature_bench） | — | 約 0.80〜0.84 |

両者は入力規模・テナント構成・バッチ数が異なるため厳密な同一条件ではないが、
比率が「同オーダー」の目安（0.5〜2 倍）に十分収まっており、A1（段別内訳と合計が
`feature_bench` ingest p50 と同オーダーで整合）を満たす。

## 考察

- **content_hash（I3）と redb insert（I6）が支配的**（合わせて Σ の約 80%）。
  content_hash は全行を再度 encode したうえで SHA-256 を計算するコストで、
  encode 自体（I5）の約 15〜20 倍かかっている。SHA-256 の計算コストが支配的
  であることを示唆する。
- **encode が 2 回走る**（I3 の content_hash 内部・I5 の行ループ内）という
  実装構造がそのまま I3 の重さに寄与している。content_hash 側の encode を
  I5 側の結果と共有できれば、I3 の計算コストのうち encode 分（I5 と同オーダー）
  を削減できる余地があるが、これは production コード変更を伴うため本 Issue の
  スコープ外（下記「後続への示唆」参照）。
- **commit（I8）は無視できない比重**（Σ の約 11〜12%）。生 `WriteTransaction::
  commit()` のみでこの値であり、`recovery::commit_boundary` の panic ガード分は
  残差側に含まれる。
- **残差（約 20〜23%）** はスキーマ取得（カタログデコード）・
  `commit_boundary` ガード・抽象化コストに相当する。スキーマがシンプル
  （2 列）な本ベンチの条件では相対的に大きい比率だが、行数・列数が増えるほど
  相対比率は下がると予想される（列数依存のスキーマデコードコストは行数に
  比例しないため）。

## 後続への示唆

- content_hash 側の encode（I3 内部）と行ループ側の encode（I5）を共有し
  encode を 1 回にできれば、Σ 全体を数 % 〜 1 割程度削減できる可能性がある
  （実測比率からの見積り。production コード変更を伴うため別 Issue でオーナー
  判断）。
- SHA-256 自体をハードウェアアクセラレーション命令（SHA-NI 等）で高速化する
  余地があるかは本ベンチの範囲外（`.claude/rules/dependency-policy.md` により
  外部クレート採用はユーザー承認が必要）。

## 再現手順

```sh
make bench-ingest-profile
# 規模を変えて 1 点確認する場合:
BENCH_INGEST_PROFILE_ROWS=200 BENCH_INGEST_PROFILE_DIM=64 make bench-ingest-profile
# CI 経路での拒否を確認する場合:
GITHUB_ACTIONS=true make bench-ingest-profile
# feature_bench との比較用:
cargo run --release -p engine --example feature_bench
```

## スコープ外

- `crates/engine/src/` への変更（段の直接計測を可能にする公開フック追加・
  content_hash と encode の共有化）は本 Issue では行わない。
- `insert_typed_row`（SQL `INSERT` 経路）・`replace_typed_rows_by_text_key`
  （ファイル形 `INSERT` 経路）の段別内訳。
- 専有環境での確定測定・複数規模点（rows/dim の掃引）の系統的記録
  （env による 1 点確認のみ）。
