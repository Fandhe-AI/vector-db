# ADR: embedding チャンク連続格納レイアウト（Issue #364）

- ステータス: Proposed（オーナー承認待ち。承認後に別コミットで Accepted へ更新）
- 対応: Issue #364（親 Issue #361「ベクトル検索・ストレージレイアウト最適化」の子。
  ルート Issue #344）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-8・TABLE-12）・
  `docs/spec/04-behavior/persistence.md`（PERSIST-1〜4）・
  `docs/spec/04-behavior/indexing.md`（INDEX-1・INDEX-2・INDEX-4）・
  `docs/spec/04-behavior/recovery.md`（RECOVER-2・RECOVER-5・RECOVER-9）・
  `docs/spec/04-behavior/rls.md`（RLS-1〜4）・`docs/spec/04-behavior/core-engine.md`
  （CORE-15）・`docs/spec/05-tasks.md`（TASK-87・TASK-120・TASK-140/141・TASK-142・
  TASK-145・TASK-157・TASK-169）。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連 Issue: #362（KNN 段別内訳プロファイル。本 ADR の採否判断の前提計測）・
  #363（VectorArena 世代整合キャッシュ。効果の重なりを「#363 との関係」節で分析）・
  #350（embedding 非参照集計のデコードスキップ。行値縮小の副次効果）・#366・#367
- 検証コード: なし（本 ADR はドキュメント専任。`crates/engine/src/` は無変更）

## 概要

現行の行ストアは 1 行 = 1 redb エントリであり、embedding は行値の中に inline で
格納されている（`user_rows/{table}` テーブル。キー `(tenant_id, id)`。
`crates/engine/src/storage.rs::encode_row`）。全クエリ経路が行数分の per-entry
コスト（B-tree ページ探索・エントリ開き・ヘッダデコード）を払っている。

Lance のような列指向ストレージが採る「固定長ベクトル列の連続配置（行ごとの個別
デコードを持たない設計）」に倣い、embedding をチャンク単位で連続格納する別テーブル
へ分離するレイアウト変更を検討する。本 ADR はその設計案・コスト見積り・実装タスク
分解案をまとめ、採否判断の材料を提供する。**production コードの変更は行わない**
（実装タスクは本 ADR の承認後に分解する）。

## 現状の機構

| # | 事実 | コード参照 |
| - | ---- | ---------- |
| 1 | 行値の物理レイアウト（v2）: embedding は行値の中に inline 格納。`ROW_FORMAT_VERSION` 不一致は fail-closed 拒否・マイグレーション非提供 | `crates/engine/src/storage.rs::encode_row`／`decode_row_header`／`ROW_FORMAT_VERSION` |
| 2 | 上限値（確保前検証）: `MAX_EMBEDDING_DIM` = 65,536・`MAX_METADATA_LEN` = 4MiB・`MAX_TENANT_ID_LEN` = 256 | `crates/engine/src/storage.rs` 冒頭定数 |
| 3 | KNN（SQL 表層）はクエリ毎に対象テーブル全エントリを `table.iter()` で走査し、ヘッダのみデコード → RLS `predicate` 通過行だけ `decode_row_embedding_and_metadata_into` でスクラッチへ f32 デコード → `GrowableArenaBuffers::push_row` で平坦 `Vec<f32>` へコピー | `crates/engine/src/arena.rs::build_filtered_with_rows_and_limits_in_txn`・`crates/engine/src/sql/exec.rs`（`VectorArena::build_filtered_with_rows_in_txn` 呼び出し） |
| 4 | Rust API 直呼びは `PrefilterCache`（TASK-169）で世代整合キャッシュされるが、SQL 表層は未経由（#363 の対象） | `crates/engine/src/core.rs` モジュールドキュメント |
| 5 | ストリーミング走査（`SearchTimeFilter`）・バッチ検索（`batch_search.rs::run_batch_search`）・集計（`sql/aggregate.rs`・`sql/group_by.rs`）・TASK-120 置換（`tenant.rs::replace_typed_rows_by_text_key`。`range` 走査 + `MAX_SCANNED_ROWS`）も同じ per-entry 走査を行う | `user_rows_table_def(` 呼び出し箇所: `arena.rs`・`catalog.rs`・`tenant.rs`・`rls.rs`・`sql/aggregate.rs`・`sql/group_by.rs` |
| 6 | 旧 `ROWS_TABLE`（テーブル非スコープ）は `Storage::put`/`put_batch`/`scan`/`scan_page`・`txn.rs`（`WriteTxn`/`BatchWriteTxn`）・`examples/crash_tool*.rs` が使用。本 ADR の対象（`user_rows/{table}`）とは別系統 | `crates/engine/src/storage.rs`・`crates/engine/src/txn.rs` |

実測基準値（Issue #344／#361 記載の public な数値。25,000 行・dim=128・release・
`feature_bench`）: KNN 11.7ms（≈468ns/行）・`COUNT(*)` 5.64ms（≈225ns/行）。
`COUNT(*)` と共通する走査・ヘッダデコード分が KNN コストの約半分（≈225ns/行）を
占める。

### 書き込み・世代・回復契約（現状）

- 全書き込み経路は同一 `redb::WriteTransaction` 内で「台帳追記
  （`recovery/ledger.rs::record_in_txn`）→ 行書き込み/削除 →
  `catalog::bump_table_generation_in_txn` →
  `recovery::commit_boundary::commit`」の順（`tenant.rs::*_unchecked`）。
  `Storage::put` 系（旧 `ROWS_TABLE`）は台帳非経由
- 世代はストレージ全体世代（`GENERATION_TABLE`）とテーブル単位世代
  （`catalog.rs::TABLE_GENERATION_TABLE`）の 2 層。`PrefilterCache`／
  `DictionaryCache` は fail-closed 世代照合（Issue #280／#285・
  [table-generation-rejection-granularity.md](./table-generation-rejection-granularity.md)）
- `drop_table` はカタログ・`user_rows/{table}`・op_ledger を同一 txn で削除
  （`catalog.rs`）
- crash-test 3 種（`scripts/crash_test.sh`・`crash_test_cross_table.sh`・
  `crash_test_interrupt.sh`）と `tests/power_loss.rs`（`StorageBackend` 差し替え
  電源断モデル）・`tests/index_failure_injection.rs`（RECOVER-9）が現行の回復検証面

### アライメントに関する事実（結論の根拠）

- redb 4.2.0 の leaf ページは「key data 連続 → value data 連続」で詰めるレイアウト
  であり、値ごとのアライメントパディングを持たない（`redb-4.2.0/src/tree_store/
  btree_base.rs` の `RawLeafBuilder` レイアウトコメント）。`AccessGuard::value()`
  は `&page.memory()[offset..offset+len]` の借用であり、**値先頭の 4 バイト境界は
  保証されない**。本リポ側の値レイアウトにパディングを入れても、redb から借用した
  `&[u8]` を zero-copy で `&[f32]` に再解釈できる保証は得られない
- `unsafe` は `crates/engine/src/isa.rs` 以外で禁止であり、
  `crates/engine/tests/isa.rs::unsafe_is_confined_to_isa_module_with_safety_comments`
  がソース走査で構造的に強制している。`slice::align_to` 等の unsafe な再解釈は
  不可
- `bytemuck::try_cast_slice` は safe API だが、アライメント不一致時に実行時 `Err`
  となる。上記のとおり不一致が非決定的に起きうるため、依存追加しても zero-copy は
  成立しない
- 現行の `as_chunks::<4>()` + `f32::from_le_bytes` ループ（rustc 1.96 stable）は
  安全で自動ベクトル化される。チャンク化の利得は「zero-copy」ではなく「per-entry
  コストの N 行分への償却＋1 チャンク 1 回の連続コピー」に求める

## 提案レイアウト

### 物理設計案

- 新規動的テーブル `user_vecs/{table_name}`（`user_rows/{table}` と同じ命名規約・
  `drop_table` 同一 txn 削除の対象に追加）
  - キー: `(tenant_id: &str, chunk_no: u64)`
  - 値: `[version u8 | dim u32 | slot_count u16 | live_count u16 |
    public_live u16 | private_live u16 | slot_bits (2bit/slot: live, visibility) |
    row_ids (u64 × slot_count) | f32 LE × slot_count × dim]`
    （`row_ids[slot]` はそのスロットの行 `id`。チャンクはテナント単位キーのため
    `tenant_id` は補わずに済み、`(tenant_id, row_ids[slot])` で行を一意に特定
    できる。**スロットの有効性は `slot_bits` の live ビットのみを唯一の正とし、
    `row_ids[slot]` の値そのものに意味を持たせない**（既存の `insert_row` は
    `id: u64` の 0 を拒否しておらず、`0` を「未使用」の予約値として扱うと有効な
    `id=0` の行と衝突する破壊的変更になる。codex-review P1 指摘。よって `0` を
    予約しない・既存の公開 API 契約〔`id` の入力域〕を変更しない）。tombstone・
    未使用スロットの `row_ids[slot]` は不定値として扱い、デコーダは live ビットが
    立っていないスロットの `row_ids[slot]`／embedding を読み取り対象から常に除外
    する（値の内容——0 埋めか直前の生存値の残置かは問わない）。T1 のコーデック
    検証は「非 live スロットの `row_ids` を読まないこと」を対象とし、「`0` を
    無効値として拒否すること」は対象としない）
  - KNN・`SearchTimeFilter`・`batch_search` はチャンク走査で得たスロットの
    `row_ids[slot]` から直接 `(tenant_id, id)` を復元し、`user_rows` へは
    metadata 取得のためポイントルックアップする。`user_rows` の全件走査は発生
    しない（この逆引きが無いと KNN 結果からの行復元に全件走査が必要になり、本
    ADR が狙う per-entry コスト削減が成立しない。codex-review 指摘）。
    **点検索を行うタイミングは「RLS predicate 通過後・返却対象への絞り込み前」**
    であり、「最終的な返却行に絞ってから」ではない（既存の評価順序
    `predicate`（RLS 段）→ `on_visible_row`（`WHERE`・宣言的フィルタ API 等の
    SCALAR 段）は `arena.rs::build_filtered_with_rows` のドキュメントに明記された
    固定順序であり、SCALAR 段は「ランキング前の候補集合」に対して metadata を
    要求する。したがって RLS predicate を通過した行はすべて、SCALAR 段の評価
    前に `user_rows` へポイントルックアップして metadata を取得する必要がある。
    codex-review P1 指摘）。実行計画別の取得段階は次のとおりとし、既存のフィルタ
    順序・エラー契約（fail-closed のまま維持）を変えない:
    - KNN（`WHERE`・宣言的フィルタ API を伴う場合）: RLS predicate 通過直後・
      SCALAR 述語評価の直前に metadata をポイントルックアップし、SCALAR 述語も
      通過した行だけをアリーナへ積む（`arena.rs::build_filtered_with_rows` の
      `predicate` → `on_visible_row` の 2 段構成を維持しつつ、`on_visible_row`
      の評価に必要な metadata をこの時点で取得する形へ結線する）
    - KNN（`WHERE`・宣言的フィルタ API を伴わない場合）: 従来案どおり、ランキング
      後に返却対象へ絞ってから取得してよい（候補段階で metadata を必要としない
      ため）
    - `SearchTimeFilter`・`batch_search`: 適用する述語が RLS のみ（metadata 不要）
      であれば従来案のとおり返却対象に絞ってから取得する。metadata を用いる
      述語を渡す呼び出し経路がある場合は KNN の `WHERE` 付き経路と同じ「RLS
      直後」の段階で取得する
    - この結果、`WHERE`・宣言的フィルタ API を伴うクエリでは「RLS 可視行数」分の
      ポイントルックアップが発生し、「最終的な返却行数」分に絞られる従来案より
      点検索の母数が増える。per-entry 削減効果は主に embedding 分（f32 デコード・
      コピー）に残り、metadata 分の点検索コストは RLS 可視行数に比例したまま
      である点を「コスト見積り」節に反映する（T7 の効果実測でフィルタなし
      クエリとの差を計測する）
- 行値（`user_rows`）からは embedding を外し
  `[header | locator(chunk_no u64, slot u16) | metadata]` へ（行フォーマット v3。
  v2 は fail-closed 拒否・マイグレーション非提供の既存方針を維持するか、下記
  「行単位レイアウトとの互換・移行方針」に従う）。この行→チャンクの locator は
  「行 id からチャンク位置を引く」順方向専用であり、上記 `row_ids` 配列（チャンク
  →行の逆方向）と役割が異なる。再パック（下記「削除（tombstone）」節）でスロット
  が移動した場合はチャンク側 `row_ids` の更新に加え、移動した行の `user_rows`
  locator も同一 txn 内で更新する（`row_ids` と `locator` の不一致を残さない）
- チャンクは **テナント単位**（キー先頭が `tenant_id`）とし、1 チャンクに複数
  テナントの行を混在させない。理由:
  - RLS の単一照合パス維持（後述「チャンク単位可視性ビットマップ」）
  - 他テナント破損時の可用性干渉を遮断（Issue #137 と同種の懸念）
  - `(tenant_id, id)` 昇順の同点タイブレーク前提（README 記載の検索結果順序）の
    維持

### スロット割当の選択肢比較

| 案 | 内容 | 利点 | 欠点 |
| -- | ---- | ---- | ---- |
| A. `id` 派生 | `chunk_no = id / N`, `slot = id % N` | locator 不要・決定的 | 疎な `id` 分布でチャンクが空スカスカになり空間効率が `id` 分布依存 |
| B. 追記順割当＋行値 locator（推奨） | 挿入順にチャンクへ詰める。行値に `(chunk_no, slot)` を保持 | 密に詰まる | tombstone 後の再利用に空きリスト or 末尾詰めコンパクションが必要 |

推奨は B。ただし最終決定はオーナー判断とし、本 ADR では判断材料として提示する
にとどめる。

## 検討事項ごとの設計

### 1. チャンクサイズ N

チャンク値バイト量は `N × dim × 4 + N × 8`（f32 ベクトル本体 ＋ 逆引き用
`row_ids`。header 分は別途数十バイト）。`row_ids` の寄与は dim が小さいほど
相対的に大きくなる（dim=128 では f32 部 32KiB に対し `row_ids` は 512B ≈ 1.6%）。
見積り例（f32 部のみ。上記のとおり `row_ids` 分を追加で見込む）:

| dim | N=64 | N=256 | N=1024 |
| --- | ---- | ----- | ------ |
| 128 | 32KiB | 128KiB | 512KiB |
| 768 | 192KiB | 768KiB | 3MiB |
| 1536 | 384KiB | 1.5MiB | 6MiB |

redb は値を leaf ページへ inline 格納し、値の一部更新は値全体の再書き込み
（copy-on-write）になる。N を大きくするほど読み取りの償却効率は上がるが、1 行
更新あたりの書き込み増幅（`N × dim × 4 + N × 8` バイト）が増える。既定候補と上限
（`MAX_CHUNK_BYTES` を `MAX_SCAN_TOTAL_BYTES`・`MAX_ARENA_TOTAL_BYTES` と整合する
確保前検証値として新設）は実測（#362 相当のプロファイル）に基づき別途確定する。

### 2. 削除（tombstone）

`delete_row`／`update_row`／TASK-120 置換の削除側は、チャンク値の `slot_bits` の
live ビットを落とすのみとし、**`row_ids[slot]`／embedding 本体は書き戻さない**
（0 埋め等のゼロ化は行わない。上記「物理設計案」節のとおり非 live スロットの
`row_ids`／embedding はデコーダが読み取り対象から常に除外するため、内容を
ゼロ化する必要も意味も無い。ゼロ化しないことで tombstone 時の書き込みバイト量も
`slot_bits` の該当ビットのみに抑えられる。header 部分のみの更新であっても redb
の値全体の再書き込みが発生する点に留意）。コーデックの破損検証（T1）は
「非 live スロットの `row_ids`／embedding の内容」を正規形の一部とはみなさず、
検証対象外とする。`live_count` が閾値（例: 50%）を下回ったチャンク
は再パック対象とする。再パック方式の比較:

| 方式 | 内容 | 判断 |
| ---- | ---- | ---- |
| 同一 write txn 内・対象チャンクのみ（推奨） | 削除対象チャンクの再パックを削除処理と同一 txn で行う | 実装・回復検証の範囲が閉じる |
| 遅延コンパクション（バックグラウンド） | 別途スイープ処理で断片化チャンクをまとめて再パック | 実装複雑度が高く、回復契約（RECOVER 系）との整合検証が別途必要 |

初期実装は前者（同一 txn 内・対象チャンクのみ）に限定する案を推奨する。

再パックでスロットが詰め直されると、生き残った行の `slot` が移動する。この移動は
チャンク値内の `row_ids[slot]`（新スロット位置へ書き直し）と、移動した各行の
`user_rows` 値の `locator(chunk_no, slot)` の両方を更新して初めて整合する。再パック
は同一 write txn 内で完結させる（本節冒頭の推奨方式）ため、この 2 箇所の更新も
同一 txn 内で完結し、commit 境界を跨いだ不整合（`row_ids` は新位置だが `locator`
が旧位置を指す、または逆）は生じない。crash-test・`power_loss.rs`（下記「7.
クラッシュ回復・crash-test」節）の検証対象に「再パック後の `row_ids` ⇔ 移動行
`locator` の相互整合」を明記する。

### 3. 増分反映（TASK-120）との統合

`replace_typed_rows_by_text_key` は「path 一致行の全削除 → 新規行挿入」の構成。
削除は tombstone、挿入は末尾（open）チャンクへ追記（満杯なら新チャンク作成）とする。
`INDEX-4` の一括投入上限（`crates/engine/src/batch_limits.rs`）とは独立に、新設の
`MAX_CHUNKS_PER_FILE`・`MAX_INDEX_TOTAL_BYTES` 相当の上限を維持する。

### 4. 世代整合

チャンク書き込みは既存経路と同一 txn 内で行い、`bump_table_generation_in_txn`／
`bump_generation_and_commit` の呼び出し位置は変更しない。これにより
`PrefilterCache`／`DictionaryCache`／`USING PLAN` の世代照合（Issue #285）は無変更
のまま有効に働く。`crates/engine/tests/table_generation_bump_coverage.rs` の走査
対象へチャンク書き込みモジュールを追加する必要がある。

### 5. チャンク単位可視性ビットマップ

チャンク header の `public_live`／`private_live` 集計値を用い、
`PolicyContext::is_visible(tenant, Public)` と `is_visible(tenant, Private)` の
両方が false となるチャンクは値本体（f32 部）を読まずスキップする。

- 判定は必ず `PolicyContext::is_visible` の単一照合パスを通す（独自のテナント
  比較を新設しない。[security.md](../../.claude/rules/security.md) P0）
- テナント単位キーのため要求元テナント以外のチャンクを `range` 走査で開かない
  設計も可能だが、`Public` 行は他テナントからも可視のため全テナント走査は残る
  （可視性判定の順序契約は現行と同じに保つ）
- header 破損時は「不可視だからスキップ」と判断せず fail-closed で `Err` とする
  （`decode_row_tenant_and_visibility` と同じ理由）

### 6. アライメント

「アライメントに関する事実」節の内容を踏まえた選択肢比較:

| 案 | 内容 | 判断 |
| -- | ---- | ---- |
| (a) `bytemuck` 追加 | `try_cast_slice` で `&[u8]` → `&[f32]` 変換 | redb 借用スライスのアライメントが保証されないため非決定的に失敗しうる。**採用理由なし** |
| (b) パディングレイアウト | 値内オフセットを 4 の倍数に揃える | 値先頭自体が未整列なら無意味。redb 側に制御手段がない。**不採用** |
| (c) 連続コピー（推奨） | チャンク値の f32 部を `as_chunks::<4>()` + `from_le_bytes` で一括変換（現行と同じ safe API・自動ベクトル化） | `unsafe` 非導入・依存追加なし。利得は per-entry コストの償却と 1 チャンク 1 回の連続コピーに求める |

結論: (c) を採用する。`bytemuck` は不採用とする（依存最小方針。採用する場合は
[dependency-policy.md](../../.claude/rules/dependency-policy.md) に従いユーザー
承認＋ `=x.y.z` 完全固定が必須）。

### 7. クラッシュ回復・crash-test

行テーブル・チャンクテーブル・op_ledger の 3 テーブル横断原子性は、同一
`redb::WriteTransaction` で担保する（`BATCH_LOG_TABLE`／`OP_LEDGER_TABLE` と同型の
契約）。影響を受ける検証面と必要な拡張:

| 検証面 | 現状 | 必要な拡張 |
| ------ | ---- | ---------- |
| `scripts/crash_test.sh` | `Storage::put` 経路を検証 | チャンク経路用の `crash_tool` サブコマンド追加 |
| `scripts/crash_test_cross_table.sh` | `BatchWriteTxn::log_batch` を検証 | 3 テーブル（行・チャンク・台帳）横断整合検証への拡張 |
| `crates/engine/tests/power_loss.rs` | `StorageBackend` 差し替え電源断モデル | 部分書き戻し像でのチャンク値と locator の不整合検出（fail-closed 拒否）の確認。`row_ids[slot]` ⇔ `user_rows` 側 `locator` の相互不整合（再パック中の電源断を含む）も同一の fail-closed 検出対象に含める |
| `crates/engine/tests/index_failure_injection.rs`（RECOVER-9） | 再構築処理の途中失敗注入 | チャンク再パック処理の途中失敗で commit 済み行が無傷であることの確認。途中失敗後も `row_ids` と `locator` が旧配置のまま一貫していること（半端な書き換えが残らないこと）を含める |

`recovery/commit_boundary.rs` の choke point 経由は維持する。

## コスト見積り

- **読み取り**: per-entry コスト（≈225ns/行の走査＋ヘッダデコード分）が概ね `1/N`
  に償却される見積り。25,000 行・N=256 では約 98 エントリまで削減される計算になる。
  KNN の arena 構築段（≈468ns/行 の残り ≈243ns/行 = f32 デコード＋コピー）は
  連続コピー化により減少が見込まれる（定量は #362 の実測で確定させる。本 ADR では
  上限見積りとして提示するに留める）
- **副次効果**: 行値から embedding が抜けるため `COUNT(*)`／集計／`GROUP BY`／
  TASK-120 の path 一致走査（metadata のみ参照）で leaf ページあたりの行数が
  増え、ページ読み取り数が減る見込み（dim=128 で行値 ≈540B → ≈60B 程度への縮小）
- **書き込み**: 1 行挿入/削除あたりチャンク値全体の再書き込み（`N × dim × 4 + N × 8`
  バイト）が追加コストとなる。TASK-120 の一括投入（ファイル単位で複数行）では
  チャンク単位にまとまるため増幅は相対的に小さいが、単発 DML
  （`insert_row`/`delete_row`/`update_row`）では悪化するため、N の上限設定と
  想定ワークロードの前提を明記する必要がある
- **空間**: tombstone 分の浮き（再パック閾値で上限を設ける）・チャンク header
  オーバーヘッド（N あたり数十バイト）

## 行単位レイアウトとの互換・移行方針

選択肢比較:

| 案 | 内容 |
| -- | ---- |
| (1) 一斉移行 | 行フォーマット v3 へ一斉移行し、旧 DB は fail-closed 拒否（既存の `IncompatibleRowKeyFormat`／`ROW_FORMAT_VERSION` と同型の方針） |
| (2) opt-in（推奨） | カタログにテーブル単位のレイアウト属性（`layout: row \| chunked`）を持たせ、既定は `row` に据え置いて opt-in（`CATALOG_FORMAT_VERSION_LINE` の更新を伴う） |
| (3) 両立 | `RowStore` 抽象で両レイアウトの読み取りを共存させ、書き込みのみ切り替える |

推奨は (2) ＋オフライン再構築ツール（`crates/engine/examples/` に rebuild
サブコマンドを追加。旧レイアウトを読み新レイアウトへ全件書き直す。オンライン
マイグレーションは提供しない）。理由: 実装着手直後でデータ互換保証は不要という
既存判断と整合しつつ、#362／#363 の結果に応じてテーブル単位で効果検証できるため。

旧 `ROWS_TABLE`（`Storage::put` 系・`txn.rs`・`crash_tool` 系）はテーブルスコープ
行ストア（`user_rows/{table}`）とは別系統であり、本 ADR の対象外とする。

## Issue #363（arena キャッシュ）との関係・採否判断の材料

Issue #363 が SQL 表層に世代整合 arena キャッシュを導入すると、同一世代内の反復 KNN
では arena 再構築自体が発生しなくなるため、チャンク化による KNN 側の利得は
「コールドスタート・書き込み後の再構築・`SearchTimeFilter`（動的ポリシー）・
`batch_search`」に限定される（下記「T3 の対象範囲・集計の読み取り元」のとおり
集計（`SUM`／`GROUP BY`／`HAVING` 等）はチャンク走査へ移らず縮小後の `user_rows`
を走査し続けるため、この一覧から除く）。

判断基準案: #362 の実測で「走査・ヘッダデコード段」が KNN 内訳の支配項であり、
かつ書き込み頻度が高く #363 のキャッシュヒット率が低いワークロードを重視する
場合に採用する。そうでなければ Rejected（または Deferred）とする、という判断
分岐を申し送る。

## 実装タスク分解案（Issue 起票は行わない）

本 ADR 承認後、[out-of-scope-tracking.md](../../.claude/rules/out-of-scope-tracking.md)
の承認制に従いユーザー承認を得たうえで Issue を起票する。以下は分解「案」のみ。

| # | タスク | 依存 | 主な対象 |
| - | ------ | ---- | -------- |
| T1 | チャンクテーブル定義・コーデック（header 検証・上限定数・`unsafe` なし） | — | `storage/chunk.rs`（新規）・`catalog.rs`（`user_vecs` 命名・`drop_table`） |
| T2 | 書き込み経路の結線（挿入・削除・更新・TASK-120 置換・tombstone・同一 txn） | T1 | `tenant.rs`・`recovery/ledger.rs` 連携・`table_generation_bump_coverage.rs` |
| T3 | 読み取り経路の結線（`VectorArena`・`SearchTimeFilter`・`batch_search` のチャンク走査＋可視性集計スキップ。集計は下記「T3 の対象範囲・集計の読み取り元」のとおりチャンク走査へ移さない） | T1 | `arena.rs`・`rls.rs`・`batch_search.rs`・`sql/aggregate.rs`・`sql/group_by.rs` |
| T4 | カタログのレイアウト属性と opt-in・オフライン再構築ツール | T1 | `catalog.rs`・`examples/` |
| T5 | 回復検証の拡張（crash_tool サブコマンド・cross-table 3 テーブル・power_loss・RECOVER-9 注入） | T2 | `scripts/`・`examples/crash_tool*.rs`・`tests/` |
| T6 | 再パック（コンパクション）と閾値 | T2 | `storage/chunk.rs`・`tenant.rs` |
| T7 | 効果実測（`feature_bench` 前後比較・`sql_c1_bench`・#362 内訳との突合）と README「実装方針（要点）」反映 | T3・T4 | `examples/feature_bench.rs`・`docs/` |

### T3 の対象範囲・集計の読み取り元（codex-review P1 指摘への対応）

提案するチャンク値（`row_ids`・embedding・`slot_bits`）には `SUM`／`AVG`／`MIN`／
`MAX`／`GROUP BY`／`HAVING` が要求するスカラー metadata が含まれない。metadata は
引き続き `user_rows` 側にのみ存在するため、これらの集計は **チャンク走査へは
移さず、embedding を外した縮小後の `user_rows`（行フォーマット v3）を走査し続ける**
（`sql/aggregate.rs`・`sql/group_by.rs` は `user_rows` の `range`／`iter` 走査を
維持し、チャンクテーブルには触れない）。T3 における `sql/aggregate.rs`・
`sql/group_by.rs` への変更は「チャンク走査への切り替え」ではなく、行フォーマット
v3（embedding を含まない縮小行）へ追随するデコード経路の更新に限定する。

この方針は「コスト見積り」節の副次効果（行値縮小により集計・`GROUP BY`・TASK-120
の path 一致走査が縮小後の `user_rows` を走査する前提でページ読み取り数が減る
見積り）と整合する。チャンクからポイントルックアップして `user_rows` を走査する
利点を失う設計（全チャンク→全行ポイントルックアップ）は採らない。

例外として `COUNT(*)`（`WHERE` を伴わず metadata 不要な場合に限る）は、チャンク
header の `live_count`／`public_live`／`private_live` 集計値を可視性判定に用いて
`user_rows` を一切走査せずに算出できる余地があるが、この最適化は本 ADR の対象外
（将来検討）とし、T3 では実装しない。`WHERE`・宣言的フィルタ API を伴う集計は
metadata 参照が必須のため、上記「候補段階での metadata 取得」節と同じ理由で
`user_rows` 走査（縮小後）を維持する。

## セキュリティ考慮（OWASP Top 10 + AGENTS.md P0）

| 観点 | 本タスク（ADR 執筆）での対応 | ADR が将来実装に課す要件 |
| ---- | ---------------------------- | ------------------------ |
| private spec 漏えい（P0） | spec 本文・非公開設計議論を転記していない。参照は TASK-nn／ビヘイビア ID／パスのみ。数値は public Issue #344／#361 由来の実測値と本リポ定数に限定 | — |
| 秘密情報混入（P0） | 実トークン・資格情報は記載していない | — |
| テナント境界（P0） | — | 可視性判定は `PolicyContext::is_visible` の単一照合パスのみを用いる。チャンクスキップは同 predicate の結果から導出し、独自テナント比較を新設しない。チャンクはテナント単位キー。header 破損は fail-closed（スキップしない）。エラーに他テナントの存在情報を含めない |
| インジェクション | — | テーブル名は `validate_identifier` 通過後のみ `user_vecs/{table}` へ用いる（`user_rows` と同じ扱い） |
| 不安全な設計／DoS | — | チャンク header の `slot_count`・`dim`・値長は確保前に上限検証（`MAX_EMBEDDING_DIM`・新設 `MAX_CHUNK_BYTES`）。`checked_*` 演算を用いる。走査総量は `MAX_ARENA_ROWS`／`MAX_ARENA_TOTAL_BYTES` と整合させる |
| `unsafe`（P1） | — | 導入しない（`isa.rs` 限定テストにより構造的に禁止）。zero-copy 再解釈は採らない |
| 依存の無断追加（P1） | `bytemuck` は追加していない（不採用理由を本 ADR に記載。採用する場合はユーザー承認＋ `=x.y.z` 固定が必須） | — |
| 回復契約（RECOVER 系） | — | 3 テーブル横断書き込みは同一 `WriteTransaction`・`commit_boundary::commit` 経由に限定する |
| CI・ワークフロー改変（P1） | 変更なし | — |

## スコープ外・申し送り

本 ADR で決めないこと:

- チャンクサイズ N の最終値（実測で決める）
- ANN 索引との併用（#367）
- GPU 常駐行列（`gpu_batch.rs`）との直結。チャンク値を直接転送する可能性は将来
  検討として記載のみ
- 旧 `ROWS_TABLE` の扱い

ADR の Accepted 化・実装タスク（T1〜T7）の Issue 起票・README「実装方針（要点）」
への反映は、いずれもオーナー承認後の別作業とする
（[out-of-scope-tracking.md](../../.claude/rules/out-of-scope-tracking.md) の
承認制）。

## 参照

- 「現状の機構」節のコードポインタ
- 先行 ADR: [table-generation-rejection-granularity.md](./table-generation-rejection-granularity.md)・
  `crash-tolerance-reverification.md`・`multi-dim-table-coexistence.md`・
  `c1-p95-dedicated-env-reverification.md`
