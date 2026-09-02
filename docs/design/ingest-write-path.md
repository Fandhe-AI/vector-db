# 書き込み経路改善（Phase 2）の総括: 前後比較と Durability／バッチ上限の判断記録

- ステータス: Accepted（記録のみ。production コード〔`crates/engine/src/`〕は
  本コミットで無変更）
- 対応: Issue #401
- 親: Issue #395（「ingest 書き込み経路の固定コスト削減」Phase 2）
- 前提 doc: `docs/design/ingest-stage-profile.md`（Issue #396・#397 追記・#398 追記）・
  `docs/design/redb-insert-reserve-zero-copy.md`（Issue #400）
- 関連ポインタ（判定内容は spec 側が SSOT）: RECOVER-5／RECOVER-6／RECOVER-10・
  TASK-93／TASK-96／TASK-97／TASK-101／TASK-122・INDEX-4・TABLE-12

## 1. 背景

親 Issue #395 は、`feature_bench`（`crates/engine/examples/feature_bench.rs`）の
`ingest` フェーズ（1,000 行バッチ・dim 128）p50 が読み取り側の一連の改善
（Issue #350・#353・#357・#363 等）後も改善されず相対的に劣位だったことを起点に、
`tenant::insert_rows_unchecked` の行ごと固定コストを削減する取り組みだった。
個別 Issue の結果は次のとおり。

| Issue | 内容 | 結果 |
| --- | --- | --- |
| #396 | ingest 段別プロファイルベンチ（`make bench-ingest-profile`）の追加 | 実施（`docs/design/ingest-stage-profile.md`） |
| #397 | `encode_row` の二重実行排除（content_hash 用と書き込み用でそれぞれ encode していたのを 1 回に統合） | 採用（同 doc「Issue #397 追記」） |
| #398 | 行エンコードのバッファ再利用（`encode_row_into` の append 方式・1 バッチ 1 arena 確保）・embedding のバルク LE 変換 | 採用（同 doc「Issue #398 追記」。前後比較実測は当時の高負荷共有環境のため未記録） |
| #400 | redb `insert_reserve` によるゼロコピー行書き込みの試作 | **不採用**（`docs/design/redb-insert-reserve-zero-copy.md`。I6 段が中央値 約 +49.8% 悪化） |
| #399 | 自作 SHA-256 の最適化 | **open（未マージ）**。本 doc の前後比較には含まれない |

本 doc は Phase 2 を通した **前後比較**（before: `61fc943`／after: `origin/main`
時点の `badd9d9`）と、調査段階で検討し棄却した選択肢の記録（4 節。詳細は
RECOVER-5／RECOVER-6／RECOVER-8・TABLE-12 のポインタ表記に留める）、
および PostgreSQL `COPY` のバッファフラッシュ基準と本リポの一括投入上限
（`batch_limits.rs`）の対比を記録する。production コードは無変更（docs 専任）。

## 2. 計測設計

### 2.1 対象コミット

- **before**: `61fc943`（Issue #396 のベンチ追加時点。production コードは
  Phase 2 着手前と同一）
- **after**: `origin/main`（`badd9d9`。#397・#398 の production 変更を含む。
  #400 はベンチ側 A/B モードの追加のみで production 無変更）

### 2.2 判断ルール（計測前に固定）

`docs/design/knn-two-stage-topk.md` の教訓（per-run 生データを保持しないと
事後判定できない）を踏まえ、計測前に次を固定した。

1. before/after を**交互**に実行する（逐次ではなく A/B ペア）。
   `feature_bench` は 3 ペア、`bench-ingest-profile` は 3 ペア実行した
   （共有計測環境の負荷 `loadavg` は各 run で記録）。
2. 集計は各指標の**中央値**（median-of-N）とする。
3. **非退行判定**（受け入れ条件「13 フェーズ非退行」）: `ingest` 以外の
   12 フェーズは本 Issue の production 変更（`tenant.rs` の書き込み経路・
   `storage::encode_row_into`）を読み取り経路として経由しないため
   「変更を含まない参照区間」として扱い、after median p50 ≤ before median
   p50 × 1.05 かつ p95 ≤ × 1.10 を pass とする。超過するフェーズがあれば
   before 側 3 run の p50/p95 の変動幅（run-to-run 変動）と比較し、
   変動幅内なら「ノイズ帯内・非退行」、変動幅を超えるなら「退行の疑い」として
   本 doc に明記する（安全側）。
4. `ingest` フェーズは構造的改善（encode 1 回化・確保回数削減）が実装から
   直接導かれる事実であるため、時間面の改善が統計的に有意でなくても
   「非退行」であれば Phase 2 の結論としては記録可とする（#397 追記節と
   同じ扱い）。

### 2.3 計測環境

Linux・x86_64・12 論理コア・AVX2FMA。`docs/design/ingest-stage-profile.md` と
同一の**共有開発環境**（専有環境ではない）。計測時の `loadavg` は 1.1〜3.4
（12 コア中）で推移した。ホスト上には他プロセス（コンテナ・エディタバックエンド
等）が常時稼働しており、この環境固有のノイズ源として後述 3.3 で言及する。

## 3. 前後比較

### 3.1 `feature_bench` `ingest` フェーズ

25 バッチ・1,000 行/バッチ・dim128・テナント A/B 混在（`cargo run --release
-p engine --example feature_bench`）を交互 3 ペア実行した中央値:

| 指標 | before (median) | after (median) | 比 (after/before) |
| --- | --- | --- | --- |
| p50 | 4,575 µs/1,000 行 | 4,543 µs/1,000 行 | 0.993 |
| p95 | 4,712 µs/1,000 行 | 4,740 µs/1,000 行 | 1.006 |
| rows/sec | 218,776 | 219,722 | 1.004 |

各 run の p50 生データ: before `[4575, 4583, 4573]`／after `[4555, 4543, 4528]`
（単位 µs）。p50・p95・rows/sec のいずれも判定基準（p50 ≤ ×1.05・p95 ≤ ×1.10）
を満たし **pass**。ただし比が 1.0 前後に収まっており、共有計測環境のノイズ帯
（before 自身の run-to-run 幅が p50 で最大 10 µs 程度）と比べて統計的に有意な
改善とまでは主張しない。#397・#398 の構造的な確保回数削減は実装から直接導かれる
事実として記録するにとどめる（2.2 の判定ルール 4）。

### 3.2 `feature_bench` 全 13 フェーズの非退行判定

| フェーズ | before p50 (median) | after p50 (median) | 比 | before p95 (median) | after p95 (median) | 比 | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| ingest | 4,575 | 4,543 | 0.993 | 4,712 | 4,740 | 1.006 | pass |
| point_where | 2,804 | 2,733 | 0.975 | 3,237 | 2,864 | 0.885 | pass |
| where_compound | 3,260 | 3,343 | 1.025 | 3,439 | 3,611 | 1.050 | pass |
| agg_count | 2,970 | 2,931 | 0.987 | 3,461 | 3,029 | 0.875 | pass |
| agg_multi | 3,172 | 3,173 | 1.000 | 3,343 | 3,261 | 0.975 | pass |
| group_by_having | 3,549 | 3,526 | 0.994 | 3,689 | 3,778 | 1.024 | pass |
| vector_knn | 8,618 | 8,517 | 0.988 | 10,681 | 9,250 | 0.866 | pass |
| vector_knn_where | 2,784 | 2,757 | 0.990 | 3,117 | 2,850 | 0.914 | pass |
| hybrid_rrf | 99,296 | 98,766 | 0.995 | 106,117 | 105,601 | 0.995 | pass |
| mode_recall | 8,817 | 8,870 | 1.006 | 10,553 | 9,882 | 0.936 | pass |
| mode_precision | 8,701 | 8,825 | 1.014 | 10,249 | 10,000 | 0.976 | pass |
| rls_isolation | 2,818 | 2,796 | 0.992 | 3,096 | 2,909 | 0.940 | pass |
| udf_call | 690 | 695 | 1.007 | 738 | 1,040 | 1.409 | 要注記（下記） |

単位はすべて µs/クエリ。12 フェーズ（`ingest` を除く。`ingest` は 3.1 参照）のうち
11 フェーズは判定基準内で pass。残る `udf_call` は p95 が判定基準（×1.10）を
超過しており、下記のとおり退行の疑いとして注記する。

**udf_call の p95 について**: 比 1.409 は判定基準（×1.10）を超過する。
before 側 3 run の p95 幅は `[756, 734, 738]`（範囲 22 µs）と極めて小さいのに対し、
after 側は `[1008, 1040, 1117]` で明確に高い。2.2 のルール 3 に従い「退行の疑い」
として記録する。

ただし次の理由からこれを実際の性能退行とは判断していない:

- `udf_call` フェーズは宣言的 UDF 呼び出し（TASK-79）経路を計測するもので、
  本 Issue の production 変更（`tenant.rs` の書き込み経路・`storage::
  encode_row_into`）を一切経由しない。
- p50 は before/after でほぼ同一（690 → 695 µs）であり、p95 のみが乖離している。
  1 クエリあたりの絶対時間が最も小さいフェーズ（他フェーズの 1/4〜1/140）で
  あるため、外れ値 1 件の混入が比率に大きく効きやすい。
- 計測ホスト上に無関係な高 CPU 占有プロセスが常時存在することを `ps aux` で
  確認した（共有開発環境。3.3 参照）。このような環境ノイズが短時間フェーズの
  tail latency（p95）に偏って現れることと整合する。

原因調査（本 Issue のスコープ外・専有環境での再測定）は 7 節の申し送りに記載する。
production コードは `udf_call` の実行経路（`udf_call.rs`）に触れていないため、
本 Issue の変更が原因である可能性は構造的に排除できる。

### 3.3 `bench-ingest-profile` 段別内訳

`make bench-ingest-profile`（既定 rows=1,000・dim=128・`BENCH_INGEST_PROFILE_
INSERT_MODE` は既定 `insert`）を交互 3 ペア実行した中央値（ms/1,000 行）:

| 段 | before (median) | after (median) | 比 |
| --- | --- | --- | --- |
| I1 (precheck) | 0.008 | 0.008 | 1.00 |
| I2 (begin_write) | 0.001 | 0.001 | 1.00 |
| I3 (content_hash) | 1.680 | 1.975 | 1.18 |
| I4 (ledger) | 0.008 | 0.008 | 1.00 |
| I5 (encode) | 0.095 | 0.108 | 1.14 |
| I6 (redb insert) | 0.809 | 1.058 | 1.31 |
| I7 (generation bump) | 0.002 | 0.002 | 1.00 |
| I8 (commit) | 0.353 | 0.348 | 0.99 |
| Σ(I1..I8) | 2.960 | 3.509 | 1.19 |
| E0 (`insert_rows` e2e) | 3.609 | 3.434 | 0.95 |

各 run の I3/I6 生データ（ms）: before I3 `[1.680, 1.679]`・I6 `[0.806, 0.809]`
／after I3 `[1.967, 1.982]`・I6 `[1.059, 1.057]`。**3 run 目の生データは記録が
残っておらず、上記は 3 ペア中 2 ペア分のみである**。第三者が中央値を再計算できる
形で 3 ペア全件を記載できていないため、本節の中央値（I3/I6 の median 列）自体の
確からしさは未確定として扱う。

以下の所見は、記録が残っている 2 ペア分の生データと E0（pub API 実測）を根拠と
した**暫定所見（判定保留）**であり、3 run 目を含む生データの再測定・再記録が
完了するまでは「退行ではない」の確定判定とはしない:

1. **E0（`insert_rows` の pub API 実測）は after のほうが小さい**（3.609 →
   3.434 ms、0.95 倍）。I3・I6 は `insert_rows_unchecked` の内部段を独立再現
   したベンチ内レプリカの計測であり、production の実経路（E0）は退行していない。
   Σ（レプリカの内部段合計）が E0（実経路の直接計測）を上回る逆転が生じており
   （ベンチ自身が `residual(E0-SUM): n/a` として自己検出・報告する既存の
   fail-safe ログ出力どおり）、レプリカ側の計測区間にのみ偏ったノイズが乗った
   ことを示唆する。
2. **段の実装は #397 で「I5 が I3 より前・I3 は SHA-256 のみ」という構造へ
   変更されており、この構造変更自体は既に `docs/design/ingest-stage-profile.md`
   「Issue #397 追記」節で前後比較済みで、その時点の実測（別の計測タイミング）
   では I3 が 1.70〜1.73 → 1.58〜1.59 ms へ**改善**していた。今回の after 実測
   （I3 = 1.97〜1.98 ms）はその実測とも整合しない。同一の production・ベンチ
   コードに対する 2 時点の計測差であり、コード変更ではなく環境要因（計測時点の
   違い）に起因すると考えられる。
3. 計測ホスト（`ps aux`）に、無関係なシェルスナップショット由来のビジーループ
   プロセスが 1 コアを継続的に占有していることを確認した。加えて Docker・
   Erlang（logflare/realtime）等の常駐プロセスが同居する共有開発環境である。
   `loadavg` は計測中 1.1〜3.4（12 コア中）で推移しており、単独のコア占有が
   ベンチのスレッドスケジューリングに影響しうる。
4. I6（redb insert）は #397/#398 のいずれの変更も insert 呼び出し自体の
   バイト列生成方式を変えただけで（事前 encode 済みバイト列をそのまま
   `Table::insert` に渡す点は変更前後で同一）、insert 自体の redb 内部処理は
   不変であるため、31% もの増加を production 変更で説明できる要因はない。

以上の状況証拠（1〜4）は、I3・I6 の増加が本 Issue の production 変更に起因する
退行ではなく共有計測環境のノイズである可能性を支持するが、**生データ不足のため
本 doc 単独では確定判定としない（計測不成立／判定保留）**。3 run 全件の生データを
伴う再測定を 7 節へ申し送り、その結果をもって確定判定とする。

## 4. 棄却判断の記録（ポインタ表記）

Phase 2 の検討過程で採否を判断し、production コードへ反映しなかった選択肢が
ある。検討した選択肢・redb の実装事実・棄却理由の詳細は private spec 側の
判断事項であり、本 doc では転記しない
（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）。
判断は RECOVER-5／RECOVER-6・TASK-96／TASK-97、RECOVER-8・TASK-99、
TABLE-12・#397 の既存契約の範囲内で行っている。

## 5. 参考: PostgreSQL `COPY` のバッファ二重基準と本リポのバッチ上限

PostgreSQL の `COPY` 実装（`src/backend/commands/copyfrom.c`）は、
`MAX_BUFFERED_TUPLES`（1000）・`MAX_BUFFERED_BYTES`（65535）という**件数・
バイトの二重基準**でバッファをフラッシュする。

本リポの一括投入上限の現状は次のとおり:

| 経路 | 上限の有無 | 実装 |
| --- | --- | --- |
| ファイル形バッチ（`EngineCore::execute_insert_sql_batch`） | あり（4 上限＋解析前 SQL 長ガード） | `batch_limits.rs`（TASK-122・INDEX-4。ファイル数／ファイル本文バイト／バッチ合計バイト／生成チャンク数。数値は env 注入・fail-closed フォールバック） |
| 行形バッチ（`tenant::insert_rows`） | **バイト・行数上限は無し** | 行単位の `MAX_METADATA_LEN`・`MAX_EMBEDDING_DIM`・`MAX_TENANT_ID_LEN` と、id 集合・arena・range 表・hash 入力に対する `try_reserve`／`try_reserve_exact` の確保失敗を `Err` に倒す fail-closed のみで頭打ちになる |

行形バッチ（`insert_rows`）にはバイト単位・行数単位の明示的な予算が無く、
`try_reserve` による確保失敗時の fail-closed のみが歯止めになっている。
Issue #398 で導入した 1 バッチ 1 arena 確保（`encoded_row_len` の `checked_add`
総和 → `try_reserve_exact`）は行数×行長に比例する単一確保であるため、上限が無い点は
無制限リソース確保（DoS）の観点で申し送りに値する。既存の走査側上限（例:
`MAX_BATCH_LOG_ROWS` 等）は書き込み側には効かない。

**申し送り**: 行形バッチへの行数×バイト予算（PostgreSQL `COPY` の二重基準相当）
の導入は、本 Issue のスコープ外としてユーザー承認後に別 Issue で判断する
（自動運転では起票しない。`out-of-scope-tracking.md` 準拠）。

## 6. 再現手順

```sh
# before（Issue #396 時点）
git worktree add /path/to/wt-before 61fc943 --detach
cd /path/to/wt-before
cargo run --release -p engine --example feature_bench
make bench-ingest-profile

# after（origin/main 最新）
cd /path/to/repo
cargo run --release -p engine --example feature_bench
make bench-ingest-profile
```

各コマンドを交互（before → after → before → …）に複数回実行し、フェーズ／段
ごとの中央値を比較する。専有環境での再実測が望ましい（3.3 参照）。

## 7. スコープ外・申し送り

- #399（自作 SHA-256 最適化）マージ後の再計測・専有環境での確定測定。
- `bench-ingest-profile` の段別内訳（3.3 の I3・I6）の専有環境での再実測
  （本 doc の共有環境計測はノイズの影響を排除できていない。3 run 全件の生データ
  記録を含め、判定保留を解消するための再測定が必要）。
- `udf_call` フェーズ p95 の専有環境での切り分け（3.2）。
- 行形バッチ `insert_rows` への行数×バイト予算の導入（5 節。別 Issue・
  ユーザー承認後）。
- `insert_typed_row`／`replace_typed_rows_by_text_key`（ファイル形 `INSERT`）
  経路の段別内訳・前後比較。
- `bench-ingest-profile` の複数規模点（rows／dim 掃引）の系統的記録。
- CLAUDE.md「ステータス」段落が現状 2 回重複している点の解消（本 Issue の
  変更では両方に同一の追記を行うことで整合を保っているが、重複自体の解消は
  別途の申し送りとする）。
