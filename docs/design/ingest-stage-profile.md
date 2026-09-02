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

## Issue #397 追記: `encode_row` の二重実行排除

- ステータス: Accepted（`crates/engine/src/` へ production 変更あり）
- 対応: Issue #397（`perf(engine): encode_row の二重実行を排除（エンコード済み
  バイト列を content_hash と書き込みで共有）`）

上記「後続への示唆」節で見積もった「content_hash 側の encode（I3 内部）と行ループ側
の encode（I5）の共有化」を実装した。`tenant.rs::insert_rows_unchecked`（バッチ
INSERT）・`insert_row_unchecked`（単一 INSERT）・`update_row_unchecked`（UPDATE）の
3 経路で、行を要求記載順に **1 回だけ** `storage::encode_row` し、その結果を
`recovery::content_hash::for_insert_batch_encoded`／`for_insert_encoded`／
`for_update_encoded`（いずれも `encoded_row: &[u8]` を受け取る新形）と redb 書き込み
の双方で共有する。ハッシュ入力バイト列のレイアウト（`DOMAIN_TAG`・`OpTag`・長さ
プレフィクス・連結順）は変更していない（`RowInput` から内部で `encode_row` する旧形
`for_insert`／`for_insert_batch`／`for_update` を `#[cfg(test)]` の参照実装として
残し、新形との等価性を単体テストで機械検証。既存台帳エントリとの互換は
`tests/ingest_profile_accept.rs::content_hash_insert_batch_reimpl_matches_stored_op_ledger_hash`
〔独立再実装との照合。production 側は無関係のため本 Issue の変更前後で無変更のまま
green〕でも確認済み）。

エラー優先順位も変更前と同一に保っている: バッチ経路は事前エンコードを台帳記録より
前・要求記載順で行うため、encode 失敗が最初に発生する行・エラー種別は変更前と同じ
（`crates/engine/tests/recovery_content_hash.rs::
insert_rows_encode_error_takes_priority_over_operation_id_content_mismatch`）。
UPDATE 経路は `validate_embedding_dim` の位置を変えていない（変更前から
`content_hash::for_update` 内部の `encode_row` より前で無条件に走っていたため、
その `encode_row` を共有用に外出ししても位置関係は変わらない。
`update_row_embedding_dim_mismatch_takes_priority_over_operation_id_content_mismatch`）。

### 前後比較実測

計測環境は本ファイル冒頭の実測環境と同一（共有計測環境。`rows=1,000・dim=128`・
既定 `make bench-ingest-profile`）。

| 段 | 変更前（本ファイル上記実測。encode+SHA を含む I3 ＋ 別途 I5） | 変更後（I5 を先に 1 回・I3 は SHA のみ） |
| --- | --- | --- |
| I3（content_hash） | 1.70〜1.73 ms/1,000 行 | 1.58〜1.59 ms/1,000 行 |
| I5（encode） | 0.09〜0.11 ms/1,000 行（次元検証込み） | 0.08〜0.11 ms/1,000 行（encode のみ） |
| I3＋I5 合計 | 約 1.80〜1.83 ms/1,000 行 | 約 1.67〜1.70 ms/1,000 行 |
| Σ（I1..I8） | — | 2.85〜2.86 ms/1,000 行 |
| E0（e2e） | 3.58〜3.77 ms/1,000 行 | 3.53〜3.57 ms/1,000 行（3 回実測） |

encode の実行回数は段モデル上で 2 回（I3 内部の再 encode ＋ I5）→ 1 回（I5 のみ。
I3 は共有結果への SHA-256 適用のみ）へ構造的に半減しており、これは実装（本ファイル
「測定設計」節の段の再実装・`tenant.rs` の事前エンコード化）から直接導かれる事実
である。時間面では I3＋I5 合計が約 0.1〜0.15 ms/1,000 行（1 回分の encode コストと
同オーダー）減っており構造的半減と整合するが、E0・Σ を含む全体としては共有計測
環境の run-to-run 変動（Issue #366・#365 と同じ扱い）の範囲内にとどまり、統計的に
有意な e2e 改善とまでは主張しない。支配的段は I3（SHA-256 計算コスト）・I6（redb
insert）のままで変わらず、本変更単独でのボトルネック解消は意図していない
（見積もり通り「Σ 全体を数 % 程度削減」の範囲）。

### スコープ外（申し送り）

- エンコードバッファの再利用・ストリーミング SHA-256（`push_bytes` の内部コピーを
  含む複数回コピーの削減）は本 Issue のスコープ外（Issue 起票は自動運転では行わず
  ユーザー承認を経てから判断）。
- `insert_typed_row_unchecked`／`replace_typed_rows_by_text_key` は `encode_row` を
  経由しない別経路（`content_hash.rs` モジュールドキュメント参照）のため対象外。
- 専有環境での確定測定は引き続き未実施。

## Issue #399 追記: 自作 SHA-256 のストリーミング化

### 設計

`recovery/content_hash.rs` の自作 SHA-256（依存最小方針。TASK-101・RECOVER-10
ポインタ）を、バッチ全体（1,000 行 × 約 0.5KB ≈ 500KB）を `Vec<u8>` へ一度
連結し、さらにパディングのため丸ごと複製していた一括処理版から、ブロック単位
ストリーミング更新型の `Sha256`（`update`/`finalize`）へ再構成した。
`HashInputBuilder` の内部を `Vec<u8>` から `Sha256` へ置換し、各 `push_*` が
`Sha256::update` を直接呼ぶことで、バッチ全体を保持する中間バッファを排除した。
圧縮関数のメッセージスケジュールも 64 語配列から 16 語のローリング配列
（`w[t & 15]`）へ変更し、固定サイズの境界チェックで済むようにした。

ハッシュ入力バイト列のレイアウト（ドメインタグ・操作種別タグ・長さプレフィクス
付きフィールド連結の順序）は 1 ビットも変えていない。旧実装は `sha256_reference`
として `#[cfg(test)]` に残し、ストリーミング版との等価性をテストで機械検証する
（FIPS 180-4／NIST 公開テストベクタ 5 本〔`abc`・空・2 ブロック・4 ブロック・
100 万 `'a'`〕、境界長 0..=200・4096・65537 バイトの網羅比較、1/3/63/64/65/100
バイト刻みの分割 `update` 等価性）。既存の `for_*_encoded` 系ハッシュ関数の
単体・結合テスト（`recovery_content_hash.rs`・`recovery_ledger.rs`・
`sql_operation_id.rs`・`recovery_two_path.rs`・`tenant_write_error_exhaustive.rs`）
も無変更のまま green であり、独立再実装との照合テスト
（`ingest_profile_accept.rs::content_hash_insert_batch_reimpl_matches_stored_op_ledger_hash`）
も含めて既存 content_hash 値が不変であることを確認済み。

### 実測（受け入れ 2）

`crates/engine/benches/ingest_profile_bench.rs` の I3 は harness 側の独立再実装
（`sha256_reimpl`）で計測しており、production の `content_hash.rs` を変更しても
自動では反映されない構造（本ファイル冒頭の「測定設計」節参照）のため、本 Issue
では `content_hash.rs` 内の手動専用テスト
（`recovery::content_hash::tests::sha256_streaming_vs_reference_manual_timing`。
`#[ignore]`・CI 非配線。`cargo test --release -p engine --lib
recovery::content_hash::tests::sha256_streaming_vs_reference_manual_timing --
--ignored --nocapture` で実行）により、台帳ハッシュ対象と同オーダー（500KB・
200 回反復）の入力でストリーミング版（本番経路）と参照実装（旧・一括処理版）を
同一プロセス内で直接比較した。共有計測環境での 4 回実測（開発環境。専有環境での
再測定は未実施）:

| 実装 | 実測（4 回） | ns/byte（4 回） |
| --- | --- | --- |
| `sha256_reference`（旧・一括処理版） | 265.3 / 280.1 / 277.0 / 309.4 ms | 2.65 / 2.80 / 2.77 / 3.09 |
| `sha256`（新・ストリーミング版） | 263.8 / 265.7 / 266.1 / 266.1 ms | 2.64 / 2.66 / 2.66 / 2.66 |

ストリーミング版は 4 回すべてで参照実装以下（改善幅 0.6〜14.0%）であり、
参照実装側は `Vec` の確保・再確保に由来すると見られる run-to-run 変動
（265〜309 ms）が大きい一方、ストリーミング版は 264〜266 ms とばらつきが小さい。
バッチ全体の 2 回の全量コピー（約 1MB）を排除した構造的な効果は確認できたが、
改善幅自体は小さく（中央値で約 5%）、`docs/design/dot-kernel-multi-accumulator.md`
等で採用してきた「複数回実測でノイズ帯を超える悪化がないこと」を判定基準とすると、
本変更は悪化を示していない（4 回すべてで同等以上）ため**採用**と判断した
（Issue #365・#366 と異なり、本件は改善方向で一貫しており撤回の理由がない）。

### スコープ外（申し送り）

- `bench-ingest-profile` の I3 を production と同じストリーミング構成の
  再実装（harness 側 `Sha256Reimpl` 等）で計測し直す本格的な A/B 配線
  （段別プロファイルベンチへの正式な結線）は本 Issue では実施せず、上記の
  モジュール内手動テストによる直接比較に留めた。
- ループ展開（8 ラウンド単位のマクロ展開等）の追加検証は実施していない
  （16 語ローリングスケジュールへの変更のみを採用）。
- SHA-NI 等のハードウェア命令（`unsafe` 前提）は対象外。
- 専有環境での確定再測定は引き続き未実施。
