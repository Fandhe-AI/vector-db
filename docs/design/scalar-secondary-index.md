# ADR: スカラー列二次索引の設計検討（Issue #359）

- ステータス: Proposed
- 対応: Issue #359（親 #348「中期構造（二次索引・パースキャッシュ）検討」の子。
  ルート #344「参照 DB 比較に基づく性能改善トラッキング」配下）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-12）・
  `docs/spec/04-behavior/rls.md`・`docs/spec/05-tasks.md`（TASK-75・TASK-89/133 系）。
  spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/storage.rs`（`ROWS_TABLE`）・
  `crates/engine/src/catalog.rs`（`TABLE_GENERATION_TABLE`）・
  `crates/engine/src/arena.rs`（`build_filtered_with_rows`）・
  `crates/engine/src/declarative_filter.rs`・`crates/engine/src/sql/plan.rs`
  （`ExecutionPlan`）・`crates/engine/src/policy.rs`（`PolicyContext::is_visible`）
- 関連 Issue: #360（パースキャッシュ検討・兄弟）・#363（VectorArena 世代整合キャッシュ）・
  #367（ANN 索引採否検討）
- 本 ADR は**設計検討のみ**であり、実装コード（`crates/`）は含まない。実装タスクの
  起票は本 ADR の承認後、別途ユーザー承認を経て行う
  （[out-of-scope-tracking](../../.claude/rules/out-of-scope-tracking.md)）

## 背景

現状、SQL 表層の `WHERE` スカラー条件（等価・前方一致）は、
`sql/exec.rs::execute_statement` から `arena.rs::VectorArena::build_filtered_with_rows`
（および txn 内版 `_in_txn`）を経由し、RLS 段（`predicate`）→ SCALAR 段
（`on_visible_row`）という固定順序の**全行走査（O(N)）＋インライン比較**で評価される。
等価・前方一致条件は `declarative_filter.rs`（`DeclarativeFilter::equals` /
`starts_with` → `MetadataFilter`）としてバインドされ、`sql/plan.rs::ExecutionPlan`
の `scalar_prefilter`（SCALAR 段を DISTANCE 段より先に評価するか）によって適用順序が
決まる。

Phase 1（#345 配下）で行ったデコード段の最適化（`storage::decode_row_embedding_and_metadata_into`
のスクラッチバッファ再利用等、Issue #314）は行あたりコストの定数倍削減であり、**走査
する行数そのものを減らす技法ではない**。走査行数を減らすには二次索引が必要になるが、
索引は書き込み経路・RLS 境界・キャッシュ世代機構（`catalog.rs::TABLE_GENERATION_TABLE`・
`core.rs::PrefilterCache`）に新しい不変条件を持ち込むため、実装に先行して設計を整理
する。

参照実装として PostgreSQL の B-tree 索引選択（`access/nbtree/`）・SQLite のプランナ
（`where.c` / `wherecode.c`）、および Issue #359 のコメント（オーナー member）で補足
された Qdrant のスカラー列索引（`lib/segment/src/index/field_index/`）とカーディナリ
ティ推定（`query_estimator.rs` 等）を検討対象とする。これらはいずれも公開 OSS
（PostgreSQL: PostgreSQL License、SQLite: Public Domain、Qdrant: Apache-2.0）であり、
本 ADR では設計概念（選択度推定に基づく索引経路 / 全走査経路の切替）の要約のみを用い、
コードの転記は行わない。

## 現状の物理構造（事実整理）

- 行ストアは `storage.rs::ROWS_TABLE`（複合キー `(tenant_id, id)` の `redb::TableDefinition`）
  で、値は `tenant_id` / `visibility` / embedding / metadata を同居させた v2 行エンコー
  ディング。テーブルごとの行テーブル名は `catalog.rs::user_rows_table_name`
  経由で解決される
- `WHERE` 評価は `arena.rs::build_filtered_with_rows` が担い、RLS 述語 `predicate`
  （`tenant_id`・`visibility` のみで判定）→ SCALAR フック `on_visible_row`（等価・前方
  一致）の順で固定。不可視行は `on_visible_row` に到達しない
- 実行計画順序は `sql/plan.rs::ExecutionPlan`（`RLS`・`SCALAR`・`DISTANCE` の順列を型で
  検証）が決め、`scalar_prefilter` が `true` の場合は DISTANCE 段より前に SCALAR 段を
  適用し、`false` の場合は DISTANCE 段後の事後フィルタとして適用する
- テーブル単位の世代カウンタ `catalog.rs::TABLE_GENERATION_TABLE` は `tenant.rs` /
  `catalog.rs` の書き込み系関数群すべてでバンプされ、呼び忘れはソース走査テスト
  `crates/engine/tests/table_generation_bump_coverage.rs` が構造的に検出する

本リポには行数依存の走査コストを定量測定する常設ベンチが現時点で存在しない
（計画時点で参照された `feature_bench.rs` は本リポに未実装。既存の測定資産は
`crates/engine/examples/multi_dim_bench.rs` / `high_dim_bench.rs` /
`concurrent_write_bench.rs`、および `crates/engine/benches/` 配下のベンチ群）。損益分岐
点の実測は本 ADR の対象外とし、「コスト見積り・索引選択」節ではパラメトリックな見積りに留める。

## 索引データモデル（redb 上の設計比較）

| 案 | 構造 | 特徴 |
| -- | ---- | ---- |
| A 案 | 列ごとの `MultimapTable<(tenant_id, value_key), id>` | 等価条件のヒット列挙に直接対応。前方一致には別途レンジスキャン可能な構造が要る |
| B 案 | 複合キー `TableDefinition<(tenant_id, value_key, id), ()>` | redb のタプルキーは辞書式全順序を持つため、`(tenant_id, prefix..)` によるレンジスキャンで等価・前方一致の双方に対応できる |

B 案は前方一致（`declarative_filter.rs::starts_with` / `parse_prefix_pattern`）への
拡張性で A 案に優位なため、**推奨は B 案**とする。ただし最終選定は実装フェーズの
プロトタイプ計測（「実装フェーズのタスク分解案」節のタスク (1)）で確定する。

**索引は単一テーブルではなく二層構成とする**（後述の RLS 境界レビューで判明した
不整合の是正。詳細は「RLS 境界」節参照）:

| 層 | キー | 収録対象 | 用途 |
| -- | ---- | -------- | ---- |
| 自テナント索引（Index-T） | `(tenant_id, value_key, id)` | そのテナントが所有する全行（`Public`・`Private` 問わず） | `is_owner` と同じテナント境界でレンジスキャンを閉じる。既存 B 案どおり |
| Public 横断索引（Index-P） | `(value_key, tenant_id, id)` | `visibility = Public` の行のみ（全テナント） | `policy.rs::PolicyContext::is_visible` が定義するとおり `Public` 行は元々全テナントから可視のため、この層への収録・横断スキャンは新たな漏えいを生まない |

`WHERE` 条件の索引経路は Index-T（自テナント prefix scan）と Index-P（value_key
prefix scan）の**和集合**を候補として返す。行ストア（`storage.rs::ROWS_TABLE`）
の物理キーが `(tenant_id, id)` の複合キーであるとおり `id` はテナント内でのみ
一意な識別子であるため、和集合の重複排除は `id` 単独ではなく `(tenant_id, id)`
の組で行う（`id` 単独で dedup すると異なるテナントの別行を同一視しうる）。両層
とも索引ヒット後の可視性再判定（`is_visible` の再適用）は必須で変わらない
（後述）。`Private` 行のエントリは Index-T にのみ存在し、Index-P には一切現れ
ない——他テナントの `Private` 行の存在・分布は索引のどの層からも観測できない
設計とする。

設計上の留意点:

- 値エンコーディング: `Text` 値は memcmp 順序を保つ正規化（バイト列比較で辞書式順序が
  意味的順序と一致する形）が必要。キー長には上限を設け、untrusted 由来の値をそのまま
  無制限にキーへ連結しない（`coding-rust.md` の untrusted 入力規約——長さ上限検証後に
  アロケーションする）
- NULL 値は索引エントリを作らない（索引は「値が存在する行」のみを列挙し、NULL 述語は
  従来どおり全走査にフォールバックする）
- `visibility` が `Private` → `Public` へ更新される場合（更新経路が存在する場合）は
  Index-P への新規挿入を、`Public` → `Private` の場合は Index-P からの削除を、行の
  値更新と同一 write txn 内で行う。Index-T のエントリは可視性変更の影響を受けない
  （テナント所有権は不変のため）
- 索引対象列の宣言方式は、全 `Text` 列を自動索引化する案と、`CREATE INDEX` 相当の宣言
  的構文を SQL 表層へ追加する案がある。本リポの SQL 表層は許可リスト検証方式
  （TASK-74）を採用しているため、将来構文を追加する場合も**許可リストへの追加**として
  設計し、未検証入力を SQL 文字列へ連結する経路は作らない

## 一貫性（DML 反映・世代整合）

索引エントリの更新は、対応する行の `ROWS_TABLE` への書き込みと**同一の
`redb::WriteTransaction` 内**でコミットする。redb の write txn は単一トランザクション
内の複数テーブル書き込みをアトミックにコミットするため、この設計により「行は書けたが
索引は書けていない」という不整合状態を構造的に排除する。

具体的なハザードと対応方針:

1. **同一パス置換の索引残留**: `incremental.rs::index_file` / `index_file_batch`
   （416 行目 / 502 行目）はファイル形 `INSERT` を同一パスで置換書き込みする。旧行を
   置換する際、旧索引エントリを**先に削除してから**新索引エントリを挿入しないと、
   古いキーで索引を引いた際に既に置換済みの行を指す stale-positive エントリが残る。
   これは Index-T・Index-P（旧行が `Public` だった場合）の両方に適用される。実装
   タスクでは「同一パス置換」経路を索引更新の必須テストケースに含める
2. **世代バンプの網羅漏れ**: `catalog.rs::bump_table_generation_in_txn`（703 行目）を
   呼ぶべき書き込み系関数の一覧は `crates/engine/tests/table_generation_bump_coverage.rs`
   がソース走査で網羅性を検証している。索引専用の新しい書き込みパス（索引の再構築・
   個別エントリ更新）を追加する場合、このカバレッジ検査対象へ含める
3. **既存データからの索引ビルドと途中失敗**: テーブルに既存データがある状態で索引を
   後付けする再構築処理は、`crates/engine/tests/index_failure_injection.rs` の方針
   （commit 前失敗・再構築処理そのものの途中失敗——`arena.rs` の
   `build_filtered_with_limits_failure_mid_rebuild_*` 系）に倣い、途中失敗時にコミット
   済み行が壊れない fail-closed 契約を注入試験で固定する
4. **索引の鮮度判定と既存キャッシュ機構**: `core.rs::PrefilterCache` は世代競合を
   `DictionaryCache` と同じ fail-closed 契約（世代不一致時は `None` で拒否・Issue #280）
   に統一済み。索引の鮮度判定もこの既存機構へ相乗りし、索引専用の別系統世代カウンタを
   新設しない方針とする（テーブル単位世代カウンタの拒否粒度は Issue #285 で現状維持が
   確定済み・`docs/design/table-generation-rejection-granularity.md`）
5. **`operation_id` 台帳との独立性**: 索引更新は `operation_id` 再送契約（TASK-101・
   RECOVER-10 系）の対象である行データの内容とは独立した副次構造であり、索引の存在・
   不在は `operation_id` の重複判定（`23505` / `22023`）に影響を与えない設計とする
6. **DDL（`DROP TABLE`）時の索引破棄**: 現行 `catalog.rs::Storage::drop_table`
   （776 行目〜）は、`CATALOG_TABLE` からのスキーマ削除・行テーブル
   （`user_rows_table_name` が返す名前の `delete_table`）・`recovery::ledger` の
   テーブル分エントリ削除・`bump_table_generation_in_txn` を同一
   `write_txn`・単一 `commit_boundary::commit` で行っている（索引はまだ実装されて
   いないため、現行コードにこれ以上の破棄対象は無い）。索引導入後は、この同一
   `write_txn` 内に **Index-T・Index-P の両テーブルおよびカーディナリティ統計
   （テナント横断で保持する Index-P 由来統計を含む）の破棄**を追加することを不変
   条件とする。索引・統計の破棄が行テーブル削除より後続の別 txn・別 commit に
   分離されると、`drop_table` 後に同名テーブルが再作成された場合、旧
   `(tenant_id, id)` を指す stale-positive エントリが新テーブルの索引経路へ紛れ込み、
   誤った検索結果に加え旧テナントデータの存在情報漏えい（RLS 境界の実質的な迂回）
   につながる。「行テーブル・Index-T・Index-P・カーディナリティ統計を同一 write txn
   で破棄する」契約は、行データの `write_txn` 統一契約（本節冒頭）と同格の不変条件
   として扱う

## RLS 境界

索引ヒット後も可視性判定を必ず再適用し、索引を RLS のバイパス経路にしない
（fail-closed。`security.md` P0）。

### 索引経路と全走査経路の結果一致（Index-T / Index-P の二層構成）

`policy.rs::PolicyContext::is_visible`（142 行目）は、まず構築時に決まる
`allowed_visibilities`（許可可視性集合。既定は `Public` のみ、`Private` を見せる
には構築時の明示付与が要る）で行の可視性ラベルを絞り込み、そのうえで「同一テナ
ントの行」または「`Public` 行（テナント問わず）」を可視と判定する二段の判定
である。索引キーの先頭に `tenant_id` を置いて単一テーブルでレンジスキャンを
テナント内に閉じる設計（旧案）では、後段の判定のうち他テナントの `Public` 行に
対応する索引エントリが構造的に存在せず、索引経路が全走査経路
（`build_filtered_with_rows` の `predicate` は `is_visible` を行単位でそのまま
評価する）と異なる結果セットを返しうる。そのため索引データモデルを次の二層に
分離する（「索引データモデル」節参照）:

- **Index-T**（自テナント索引・`(tenant_id, value_key, id)`）: レンジスキャンを
  物理的にテナント内へ閉じる。従来案どおり他テナントの索引エントリへは到達しない
- **Index-P**（Public 横断索引・`(value_key, tenant_id, id)`）: `visibility =
  Public` の行のみを収録し、`value_key` を先頭キーとして**全テナントを横断**して
  レンジスキャンする

索引経路は両層の和集合を候補として返すことで、全走査経路と同じ「自テナント全体 +
他テナント `Public`」の範囲を候補として網羅する。ただし `allowed_visibilities` に
よる絞り込みは索引側では行わないため、和集合は**可視集合そのものではなく上位
集合（候補）**であり、候補に対しては**索引では代替せず必ず** `is_visible` を
再適用する（索引ヒットは「候補」であって「可視である保証」ではない、という二段
構えを設計上の不変条件とする）。エラー・空結果経由で他テナントの存在情報を漏ら
さない契約（`wire_code` 設計）は索引導入後も維持する。

### カーディナリティ統計・タイミングのクロステナント漏えい防止

Index-P への横断到達は `Public` 行に限られるが、`Public` 行の存在自体はテナント
横断で既に可視な情報であり（`is_visible` の定義そのもの）、これを索引が横断的に
扱っても新たな情報漏えいにはならない。一方 `Private` 行のカーディナリティ・分布は
Index-T にのみ反映され、Index-P・その統計のいずれからも観測できない——**選択度
推定の統計もこの二層分離をそのまま踏襲し、Index-T 由来の統計は自テナント内でのみ
参照し、Index-P 由来の統計（`Public` 行限定）のみをテナント横断で保持・参照する**。
`Private` 行の分布を横断集計する統計・「非権威的ヒント」であっても `Private` 行に
由来するテナント横断統計を経路選択（索引経路 vs 全走査経路）へ用いる設計は採用しな
い。索引経路・全走査経路のいずれを選んでも「その選択が他テナントの `Private` 行の
分布に依存する」観測可能な差（処理時間・レイテンシを含む）が生じないことを、実装
フェーズの RLS 不変テスト（「実装フェーズのタスク分解案」節のタスク (5)）で選択度推定込みで検証する。

## コスト見積り・索引選択

- 全走査経路: O(N) × (デコード + 比較) の逐次アクセス
- 索引経路: O(log N + K) × (ランダム行フェッチ + 可視性再判定 + デコード)。ここで
  K は索引がヒットする行数（選択度 s に対し K ≈ s × N）

索引経路はシーケンシャルアクセスをランダムアクセスへ置き換えるため、単純な演算量
比較だけでなく I/O パターンの違いを含めた損益分岐選択度 s\* が存在し、s\* は 1 よりも
十分小さい値になる（低選択性条件——ヒット率が高い条件——では全走査の方が有利）。この
s\* の具体値は実装フェーズでの実測が必要であり、本 ADR では以下の設計方針のみを定める:

- **選択度推定による経路切替**（Issue コメントで補足された要件）: Qdrant の
  `query_estimator.rs` 等に見られる「カーディナリティ推定値に基づき索引経路と全走査
  経路を切り替える」という設計概念を採用する。Index-T・Index-P それぞれにごく軽量な
  カーディナリティカウンタ（列値ごとの概算出現数。Index-T はテナント単位、Index-P は
  `Public` 行限定でテナント横断）を保持し、推定選択度が閾値 s\* を上回る場合は索引を
  使わず全走査へフォールバックする（統計のテナント境界は「RLS 境界」節の分離方針に
  従う）
- この切替は `sql/plan.rs::ExecutionPlan` の枠組みに「SCALAR 事前フィルタの候補列挙
  手段」として統合する位置づけとし、`scalar_prefilter` の意味（DISTANCE 段より先に
  SCALAR 段を適用するか）自体は変更しない。`precision` モードの契約（空集合応答等・
  TASK-162）も変更しない
- 書き込み側のコストは索引列数・索引層数に比例する write amplification として見積
  もる。`Private` 行は列ごとに Index-T のみ（列数分）、`Public` 行は列ごとに
  Index-T・Index-P の両方（列数の 2 倍）の追加エントリ書き込みが発生する

数値基準（具体的な s\* や break-even 行数）は実装フェーズでのベンチ実測（「実装フェーズの
タスク分解案」節のタスク (6)）で確定する。行数が少ない環境では、索引の維持コスト（write amplification・
実装複雑度）がメリットを上回る可能性があり、優先度判断はその実測後に行う。

## 代替案の比較

| 代替案 | 概要 | 本 ADR の結論 |
| ------ | ---- | -------------- |
| 現状維持 | 索引を導入せず全走査のみ | 行数が増えた場合の性能天井を左右するため、設計整理自体は先行して行う価値がある（実装着手は別判断） |
| #363（VectorArena 世代整合キャッシュ）との統合 | `VectorArena` 構築結果自体をキャッシュする方向 | 目的が異なる（索引は走査行数の削減、#363 はアリーナ再構築の削減）。相互に排他ではなく併用可能。重複回避のため、索引導入時は #363 のキャッシュ無効化条件と整合させる（世代機構を共有する設計・「一貫性（DML 反映・世代整合）」節参照） |
| zone map / bloom filter 等の軽量代替 | 列値の範囲・存在有無のみを粗く記録し、行単位索引より軽量に走査対象を絞る | 等価条件の完全な絞り込みはできない（偽陽性を許容し全走査は残る）。実装コストは低いが本 Issue の受け入れ条件（高選択性等価条件での絞り込み）を完全には満たさないため、B 案の補助的な最適化候補として位置づけるに留める |

## 実装フェーズのタスク分解案

| # | タスク | 依存 | 見積り（目安） |
| - | ------ | ---- | -------------- |
| 1 | 索引テーブルの物理設計確定＋プロトタイプ計測（A 案 / B 案、Index-T / Index-P 二層構成比較） | 本 ADR 承認 | 中 |
| 2 | 索引テーブルの物理実装＋ DML（`INSERT` / 削除 / 同一パス置換 / 可視性変更）同期反映（Index-T・Index-P 両層）＋ `catalog.rs::Storage::drop_table` への Index-T・Index-P・カーディナリティ統計の同一 write txn 破棄追加 | (1) | 大 |
| 3 | 既存データからの索引ビルド＋世代整合（`PrefilterCache` 相乗り）＋途中失敗の fail-closed 注入試験 | (2) | 中 |
| 4 | `ExecutionPlan` への経路統合＋ Index-T/Index-P 和集合の候補列挙＋カーディナリティ推定による索引経路 / 全走査経路の選択度切替 | (2) | 大 |
| 5 | RLS 不変テスト（索引ヒット後の可視性再判定・テナント境界・索引経路と全走査経路の結果一致・`DROP TABLE` 後の同名テーブル再作成で旧索引エントリが候補に復活しないこと）＋統計・処理時間経由のクロステナント（`Private` 行）漏えい防止検証 | (3)(4) | 中 |
| 6 | ベンチによる損益分岐選択度 s\* の実測・行数スケーリング測定 | (4) | 中 |

実装タスクの Issue 起票は本 ADR の承認後、ユーザー承認を経て別途行う
（本 ADR・本 PR では起票しない）。

## スコープ外

- `crates/` 配下のコード変更・索引の実装そのもの
- 実装タスクの Issue 起票
- 索引対象列の宣言構文（`CREATE INDEX` 相当）の具体的な文法確定
- 損益分岐選択度 s\* の具体的な数値の確定（実装フェーズでの実測が必要）
- #360（パースキャッシュ）・#363（VectorArena 世代整合キャッシュ）・#367（ANN 索引
  採否検討）そのものの設計（関係の整理・重複回避の言及に留める）

## 参照

- `docs/spec/04-behavior/data-model.md`（TABLE-12）
- `docs/spec/04-behavior/rls.md`
- `docs/spec/05-tasks.md`（TASK-75・TASK-89/133 系）
- PostgreSQL `access/nbtree/`（PostgreSQL License）
- SQLite `where.c` / `wherecode.c`（Public Domain）
- Qdrant `lib/segment/src/index/field_index/`・`query_estimator.rs`（Apache-2.0）
- `docs/design/table-generation-rejection-granularity.md`（Issue #285）
- `docs/design/plan-rls-boost-interaction.md`（TASK-139）
