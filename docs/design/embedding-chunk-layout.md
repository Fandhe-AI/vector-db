# ADR: embedding チャンク連続格納レイアウト（Issue #364）

- ステータス: Proposed（本 ADR の公開はオーナー承認済み〔2026-09-01〕。設計の
  Accepted 化は別途オーナー判断で別コミットにて更新）
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
  #350（embedding 非参照集計のデコードスキップ。`layout: chunked` でも
  `user_rows` の行値サイズは縮小しないため独立の最適化として効果を持つ。下記
  「副次効果（P1-1 是正により撤回）」節参照）・#366・#367
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
`COUNT(*)` は KNN コストの約半分を占める。**この ≈225ns/行 は `COUNT(*)` の
SQL 表層合成コスト（txn 開始・走査・
ヘッダデコード・述語評価の枠組み等を含む）であり、走査＋ヘッダデコードのみを
単離した値ではない**。走査＋ヘッダデコードを単離して計測した `#362`
（`docs/design/knn-stage-profile.md`。Accepted・段別内訳ベンチ実装済み）の
実測では S1（走査のみ）≈46.8ns/行・S2（走査＋ヘッダデコード）≈56.4ns/行であり、
SQL 表層合成コスト（≈225ns/行）より 1 桁小さい。下記「コスト見積り」節の
チャンク化による `1/N` 償却対象は、この #362 の単離実測（走査＋ヘッダデコード
分のみ）を根拠とし、`COUNT(*)` の ≈225ns/行 をそのまま償却対象とはしない
（両者の測定範囲が異なることを明示する。codex-review P2 指摘への対応）。

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
  コストの N 行分への償却＋1 チャンク 1 回の連続コピー」に求める。**ただし
  `user_rows` への点検索分（権威検証のためのポイントルックアップ）はこの償却の
  対象外であり、下記「コスト見積り」節で分離して見積る**

## 提案レイアウト

### 物理設計案

- 新規動的テーブル `user_vecs/{table_name}`（`user_rows/{table}` と同じ命名規約・
  `drop_table` 同一 txn 削除の対象に追加）。加えて、新規カタログテーブル
  `world_completeness`（キー `(table_name: &str, tenant_id: &str, chunk_no: u64)`・
  値 `(generation: u64, live_count: u16)`）を `catalog.rs` に追加し、
  `drop_table` の同一 txn 削除対象へ `user_vecs/{table}` と併せて含める
  （詳細・役割は下記「5. チャンク単位可視性ビットマップ」節「孤立行のオンライン
  検出（world 完全性マーカー）」参照）
  - キー: `(tenant_id: &str, chunk_no: u64)`
  - 値: `[version u8 | generation u64 | dim u32 | slot_count u16 | live_count u16 |
    public_live u16 | private_live u16 | slot_bits (2bit/slot: live, visibility) |
    row_ids (u64 × slot_count) | f32 LE × slot_count × dim]`
    （**`generation`** はカタログ `world_completeness` エントリが保持する権威
    `generation` の複製であり、当該 world の書き込み経路（下記「書き込み経路の
    同一 txn 不変条件」参照）が `world_completeness.generation` の更新と**同一
    write txn 内**で書き戻す。他の header 集計値〔`live_count`／`public_live`／
    `private_live`〕と同じく非権威の複製であり、権威は常に `world_completeness`
    側にある。この複製を header 自身に持たせるのは、「孤立行のオンライン検出
    （world 完全性マーカー）」節ステップ 0 で `world_completeness` の権威値との
    突合対象として header 自身の `(generation, live_count)` を読む必要があり、
    header にフィールドが無いと突合できないため。`row_ids[slot]` はそのスロット
    の行 `id`。チャンクはテナント単位キーのため
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
  - **`slot_bits` の visibility ビット・header の `public_live`／`private_live`
    集計値は、いずれも `user_rows` 側行ヘッダの tenant／visibility から** `insert_row`
    **等の書き込み経路が同一 txn 内で複製した派生値であり、権威（authoritative）
    な値ではない**。権威は常に `user_rows` 側行ヘッダ（`decode_row_tenant_and_visibility`）
    にある。この単一権威原則を「admission 方向」と「除外方向」で分けて適用する
    （下記「権威検証を行うタイミング」節・「5. チャンク単位可視性ビットマップ」節。
    codex-review P0 指摘への対応。両節の分割契約が唯一の正で、本節はその要約）:
    - **admission 方向（無条件に保証）**: チャンク側複製の値だけを根拠に行を
      可視と確定して admit することは一切行わない。ポイントルックアップに到達した
      候補は、まず取得した行の header が走査元の `(chunk_no, slot)` を指し戻す
      ことを確認する locator／逆参照の同一性検査（テナントデータに依存しない
      構造的検査）を経てから、`user_rows` 側権威 tenant／visibility で RLS 判定
      （`PolicyContext::is_visible`）を行い、この判定こそが admission の唯一の
      根拠である。**locator／逆参照の不一致・ポイントルックアップ自体の miss は、
      占有者を特定できない状態であるにもかかわらず `Err` にはしない**（この検査は
      走査元の `(chunk_no, slot)` と、行側が保持する `locator` を突き合わせる
      ため、`chunk_no`・`slot` いずれの側の破損でも占有者を特定できなくなる。
      よって chunked 高速経路を放棄し、**同一 tenant の全 world について
      既収集候補を破棄したうえで `user_rows` の当該 `tenant_id` 範囲走査
      （locator フィルタなし・`arena.rs::build_filtered_with_rows_and_limits_in_txn`
      と同一処理）へ tenant 単位フォールバックする**（`chunk_no` による
      world 単位フィルタは、破損しうる当のフィールドを頼りに絞り込むことに
      なり成立しないため用いない。詳細・理由は下記「権威検証を行うタイミング」
      節「フォールバック契約」参照。codex-review P0 指摘への対応——`Err` にすると
      他テナントの locator 状態でクエリの成否が変わるエラーオラクルになるため）。
      RLS 不可視と判定された候補は残りの複製値検査（3 点。`row_ids[slot]`・
      tenant・`slot_bits` 可視性。locator の同一性検査はそれより前に済んでいる）
      を経ずに黙ってスキップし `Err` にはしない（他テナントの不可視行の複製
      ドリフト〔`slot_bits`・可視性集計〕を `Err` 経由で観測させない。
      Issue #137 と同種のエラーオラクルを避ける設計であり、row-store
      （`arena.rs::build_filtered_with_rows_and_limits_in_txn`）が header
      デコード成功後に不可視行を検証なしでスキップする挙動と同一の契約。
      Bugbot High 指摘への対応）。RLS 可視と判定された候補についてのみ続けて
      残りの複製値検査（3 点）を行い、不一致も同様に `Err` にはせず**当該
      tenant**のフォールバックを発動させる（検出手段を問わず一律に tenant
      単位で発動する契約に統一している。世界単位フィルタは、同一 world 内で
      チャンク側破損とこの複製値ドリフトが併発した場合に絞り込み条件自体が
      破損しうるため用いない。黙って除外もしない。下記「権威検証を行う
      タイミング」節「フォールバック契約」参照）
    - **除外方向（`slot_bits`／header 集計値によるスキップ最適化。保証範囲が
      限定的）**: この最適化はポイントルックアップに到達する前に候補を減らす
      ためのものであり、除外判定自体は自己検証されない。正しさは書き込み経路の
      同一 txn 不変条件（下記「4. 世代整合」・T2）とコミット前後の障害注入検証
      （T5）に依拠し、コミット後の複製値のみの破損（デコードは成功するが値が
      誤っている——bit-rot 等）で本来可視な行が誤って除外される事象は、
      ポイントルックアップに到達する前に候補から外れるためチャンク走査だけでは
      検出できない残存リスクとして許容する（ポイントルックアップに到達した候補は
      上記 admission 方向のフォールバックで回復するため、この残存リスクは
      「候補にすら到達しない」除外方向の最適化に限定される）。この残存リスクは
      T8（下記「5. チャンク単位可視性ビットマップ」節）による帯域外整合性検証が
      存在することを前提とし、T8 未実装のあいだは `layout: chunked` のチャンク
      header／`slot_bits` スキップ最適化を有効化しない（`layout: row` は本 ADR の
      対象外であり影響を受けない）
  - KNN・`SearchTimeFilter`・`batch_search` はチャンク走査で得たスロットの
    `row_ids[slot]` から直接 `(tenant_id, id)` を復元し、`user_rows` へ
    ポイントルックアップして権威 tenant／visibility を取得する（metadata の
    デコードは RLS 可視が確定した後・`WHERE`・宣言的フィルタ API を伴う経路
    のみで行う。詳細な順序は下記「権威検証を行うタイミング」節参照）。
    `user_rows` の全件走査は発生しない（この逆引きが無いと KNN 結果からの
    行復元に全件走査が必要になり、本 ADR が狙う per-entry コスト削減が
    成立しない。codex-review 指摘）。
    **権威検証を行うタイミング（確定方針。以下がこのトピックの唯一の定義であり、
    他の節はこの記述のみを参照する。Bugbot Medium 指摘〔locator 不一致を
    `is_visible` より前に検査しないとドリフト時に可視行が黙って脱落する〕と
    codex-review P0 指摘〔統一順序が `is_visible` より前に全 live スロットの
    locator を検査し他テナント行起因でも `Err` にするため、呼び出し元に不可視な
    他テナント側の locator 状態でクエリの成否が変わるエラーオラクルになる〕は、
    「locator 不一致・ポイントルックアップ miss を検出した際の帰結」を
    `Err` からフォールバック（検出手段を問わず一律に tenant 単位で回収する。
    下記「フォールバック契約」節参照。同一 world 内で複数クラスの破損が
    併発すると検出手段別の使い分けは取りこぼしを生むため単一契約へ統一する）
    へ置き換えることで両立させる。以下が
    唯一の設計であり、旧来の「`Err` にする段階的改訂」の経緯は残さない）**:
    - **row-store（`layout: row`）との対応**: 現行の
      `arena.rs::build_filtered_with_rows_and_limits_in_txn` は `user_rows` を
      直接走査するため、走査中に得た行エントリの header デコード
      （`decode_row_tenant_and_visibility`）がそのまま権威 tenant／visibility を
      無償で与える（追加のポイントルックアップは発生しない）。`predicate`
      （RLS 段）はこの header デコード結果に対して適用され、通過した行だけが
      embedding・metadata のデコード（`decode_row_embedding_and_metadata_into`）・
      `on_visible_row`（SCALAR 段）・アリーナへの push（ランキング候補化）へ進む
      固定順序であり、本 ADR はこの順序を変えない（冒頭「現状の機構」表 #3 参照）。
      **この row-store 経路は、下記フォールバックの権威な実装先そのものでもある**
    - **chunked（`layout: chunked`）経路の対応**: チャンク走査で得られるのは
      `slot_bits`／header 集計値という非権威複製のみであり、権威
      tenant／visibility は `user_rows` 側にしかない。row-store と同じ
      「権威判定は SCALAR 段・ランキングより前」という相対位置を保ったまま、
      権威データの取得手段だけが異なる：row-store は走査中の header デコードで
      無償に得られるのに対し、chunked は `user_rows` への**ポイントルックアップ**
      でしか得られない。したがって **「RLS predicate を通過した後にポイント
      ルックアップする」のではなく、逆に「ポイントルックアップの結果（権威
      tenant／visibility）が RLS predicate 判定そのものを構成する」**（chunked
      経路には、ポイントルックアップに先立って行える権威な RLS predicate 判定は
      存在しない）
    - **フォールバック契約（採用する再設計。本節の中核。Bugbot Medium 指摘への
      対応で検出手段別の使い分けを撤回し tenant 単位へ統一）**: フォール
      バックの発動契機は複数ある（ステップ 0/1: `world_completeness` の
      3 値照合の不一致・マーカーエントリ欠落・チャンク header 自体のデコード
      失敗／ステップ 2: locator／逆参照の同一性検査の不一致・ポイント
      ルックアップ自体の miss／ステップ 4: 残りの複製値検査〔3 点〕の不一致）
      が、**発動時にどこまでを破棄・置換するかは検出手段を問わず一律に
      tenant 単位とする**（以前の記述は「検出手段が行側 `locator` を参照
      するか否か」で world 単位／tenant 単位を機械的に使い分けていたが、
      同一 world 内で複数クラスの破損が併発すると world 単位フォールバック
      が取りこぼしを生むため撤回する。詳細は下記「却下した代替案（検出手段別
      に世界単位／テナント単位を使い分ける契約）」参照）:

      **tenant 単位フォールバック（唯一のフォールバック契約）**: chunked
      経路の当該 tenant について、まだチャンク走査から収集していた候補
      （他 world 分・複製値検査を通過済みのものを含む）を**すべて破棄**し、
      `user_rows/{table}` の `tenant_id` 前置キー範囲全体（`chunk_no` による
      フィルタを行わない）の走査結果で**置換**する（embedding は `user_rows`
      側の権威コピーから取得する。上記「物理設計案」節「採用する設計判断
      （トレードオフ）」参照）。`chunk_no` フィルタを外すことで、行側
      `locator.chunk_no` が破損して別の値を指していても、その行自体は
      `tenant_id` 一致のみで拾われるため取りこぼさない（`predicate`／
      `on_visible_row` 判定自体は変更しない）。同 tenant の他 world の候補も
      まとめて破棄したうえでこの単一の範囲走査結果に置き換えるため、
      `chunk_no` フィルタなしで生じうる重複候補は生じない（範囲走査自体が
      当該 tenant の生存行の網羅的な集合を 1 回で与えるため）。他 tenant
      （別 `tenant_id` のチャンク）の候補・処理はこのフォールバックの影響を
      一切受けない。

      フォールバック先の row-store 範囲走査は非再帰・一発限り（当該 tenant
      について chunked 経路へ再入しない）であり、その走査自体が `Err` を
      返す場合（`user_rows` 側 header デコード失敗等、row-store 経路が本来
      `Err` にする破損クラス）は、その `Err` をそのまま呼び出し元へ伝播する
      （フォールバックが新たな情報を付加しない）。

      - **却下した代替案（検出手段別に世界単位／テナント単位を使い分ける
        契約。旧設計。不採用）**: codex-review P1 指摘を受け当初検討した案は、
        発動時の破棄・置換範囲を「検出手段が行側 `locator`（`(chunk_no,
        slot)`）を参照するか否か」で機械的に使い分けるものだった——ステップ 2
        （locator／逆参照の同一性検査）は行側 `locator` を参照するため
        tenant 単位、ステップ 0/1（`world_completeness` の 3 値照合）・
        ステップ 4（残りの複製値検査）は行側 `locator` を参照しないため
        world 単位（`chunk_no` フィルタ付き）とし、1 チャンクの局所的な破損が
        無関係な他 world の chunked 高速経路まで一律に破棄しないよう影響
        範囲を最小化する狙いだった。**この使い分けは、同一 world 内で複数
        クラスの破損が併発した場合に取りこぼしを生む（Bugbot Medium 指摘）**:
        ステップ 0/1 の world 単位フォールバックは、フォールバック先の
        `user_rows` 範囲走査を「各行が保持する `locator.chunk_no` フィールド
        が当該 world の `chunk_no` と一致するか」で絞り込む方式であり、
        絞り込み条件そのものが `locator.chunk_no` フィールドに依存する。
        ある world でチャンク側（`slot_bits`／`live_count`）が破損し
        （ステップ 0/1 が検出）、同じ world をホームとする別の生存行の
        `locator.chunk_no` も bit-rot で破損している場合、ステップ 0/1 の
        world 単位フォールバックは**チャンク走査（ステップ 1 以降）自体を
        行わずに**発動するため、この行は home world のチャンク走査で
        ステップ 2（locator／逆参照の同一性検査）に到達する機会を持たない。
        かつ world 単位フォールバック先の範囲走査は、当のフィールド
        （bit-rot した `locator.chunk_no`）を絞り込み条件に使うため、この行を
        対象 world の権威結果から除外してしまう——`chunk_no` フィルタ済みの
        世界単位フォールバックでは、絞り込みの根拠自体が破損しているケースを
        回収できない。この行は tenant 単位フォールバックへも回らないため、
        生存行が黙って結果集合から欠落する（「両クラスの孤立行を常に回収
        する」という不変条件が同一 world 内の複合破損下では成立しない）。
        この漏れを構造的に塞ぐため、**検出手段を問わずすべてのフォール
        バック発動契機で `chunk_no` フィルタを行わず、常に tenant 単位で
        回収する契約を採用する**（1 チャンクの局所的な破損が無関係な他
        world の chunked 高速経路まで一律に破棄させ影響範囲が拡大するという
        トレードオフはあるが、複合破損下での網羅性を優先する）
      - **「row-store 経路と同一の結果集合になる」という前提（網羅性の根拠と
        残存リスク。codex-review P1 指摘への対応）**: 上記の置換が row-store
        経路と同一の結果集合になるのは、**当該 tenant の当該テーブルの生存行が
        `(chunk_no, slot)` に対して 1 対 1 に分割される**という前提（パーティ
        ション前提）が成立している場合に限る。この前提は書き込み経路の同一
        txn 不変条件（上記「行単位レイアウトとの互換・移行方針」節: 再パックで
        スロットが移動した場合はチャンク側 `row_ids` の更新と同一 txn 内で行の
        `user_rows` locator も更新し、両者の不一致を残さない）により、
        コミット直後は常に成立する。**成立しなくなるのは、コミット後の複製値
        のみの破損（bit-rot 等）により、ある生存行の locator が指す `chunk_no`
        のチャンクにその行を指し戻す live スロットが存在しない「孤立行
        （orphan row）」が生じた場合である**。孤立行には性質の異なる 2 種類が
        ある: (A) チャンク側の `slot_bits`／`live_count` 自体が破損し、行を
        指す live スロットが失われるケース——このケースは (i) どのチャンク
        走査でも live スロットとして列挙されず、かつ home world の
        `live_count` 系がマーカー・popcount と乖離するため、下記「孤立行の
        オンライン検出（world 完全性マーカー）」節のステップ 0/1 が検出し
        **tenant 単位フォールバックへ回すことで回収する**。(B) チャンク側は
        無傷のまま、行自身が保持する `locator.chunk_no`（または `slot`）
        のみが破損し、行が指す側と home world のチャンクが指し返す側とで
        不一致になるケース——このケースは home world 側のチャンク・マーカーが
        いずれも無傷（3 値照合は一致したまま）であるため `world_completeness`
        の 3 値照合では検出できず、**ステップ 2 の locator／逆参照の同一性
        検査（home world のチャンク走査で当該行に到達した時点で発火する）で
        検出し、同じく tenant 単位フォールバックへ回すことで回収する**。
        (A)・(B) いずれの検出手段も発動時のフォールバック範囲を tenant 単位へ
        統一しているため、**同一 world 内で (A)・(B) が併発しても（例:
        あるチャンクの `slot_bits` が破損し、同じ world をホームとする別の
        生存行の `locator.chunk_no` も bit-rot している場合）、(A) を検出した
        時点で当該 tenant の全 world を対象とした tenant 単位フォールバックが
        既に発動しており、フォールバック先の `user_rows` 範囲走査は
        `chunk_no` フィルタを持たないため (B) の行も `tenant_id` 一致のみで
        取りこぼさず回収する**（上記「却下した代替案（検出手段別に世界単位／
        テナント単位を使い分ける契約）」参照。この点検の恒久的な帯域外担保は
        引き続き T8（世界単位の帯域外整合性検証。「5. チャンク単位可視性
        ビットマップ」節）が担い、T8 はオンライン検出（同一 txn 不変条件・
        マーカー突合・tenant 単位フォールバック）が実装バグ等で破れていないか
        を運用時に確認する役割へ位置づけを改める（下記節参照）
      効果:
      1. **フォールバックが発動した tenant（当該 tenant の全 world）に
         ついては、結果集合が row-store 経路と同一になる**ため、可視行の
         誤除外・短い結果集合の返却が構造的に起きない（Bugbot Medium 指摘・
         codex-review P1 指摘への対応。ドリフトで live スロットが無関係な
         不可視行を指す場合も、当該 tenant 全体を権威 range 走査へ切り替える
         ため、正しい占有者が検証されずスキップされる余地がない）。**この
         同一性の保証はフォールバックが実際に発動した範囲についてのみ成立する
         が、下記「孤立行のオンライン検出（world 完全性マーカー）」節の
         `world_completeness` マーカー突合（チャンク側破損由来の孤立行）と
         ステップ 2 の locator／逆参照の同一性検査（行側 `locator` 破損由来の
         孤立行）はいずれも同じ tenant 単位フォールバックへ回すため、同一
         world 内で両クラスが併発しても取りこぼしなく回収する（上記「却下した
         代替案（検出手段別に世界単位／テナント単位を使い分ける契約）」参照）。
         上記 2 種類の孤立行のいずれも次回以降のチャンク走査でホーム world
         への到達時点で必ず検出されフォールバックが発動するため（P1-2 是正）、
         孤立行のケースも実質的にこの保証の対象に含まれる。マーカー突合自体が
         破損している残存ケース（下記「マーカー自体が破損・欠落している場合」
         参照）は T8 の帯域外検証に委ねる**
      2. **`Err` が発生し得るのは row-store 経路でも `Err` になる場面に限られる**
         （header デコード失敗等、現行と同一の破損クラス）。複製ドリフト
         （locator 不一致・3 点検査の不一致）を理由に `Err` が返ることはなく、
         他テナントの locator 状態・複製ドリフトの有無によってクエリの成否が
         変わる経路が存在しない（codex-review P0 指摘への対応。Issue #137 と
         同種のエラーオラクルを新規に追加しない）
      3. **フォールバックの有無・回数は結果集合・エラー応答に現れない**
         （呼び出し元の wire 可視契約——返る行・エラーの `wire_code`——には
         影響しない。ただし、当該 tenant のコストが chunked 高速経路から
         row-store 経路の走査コストへ退化する分、単発クエリのレイテンシは
         変化しうる。このレイテンシ差は残存として明示し、クエリ結果・
         エラーとしては観測不能である一方、計測はプロセス内メトリクス／T8
         の帯域外検証で扱う〔下記「コストへの帰結」節〕）
      統一順序（両経路の相対位置が完全一致することを保つ。段階〔候補時点／
      返却時点〕による例外を設けない）:
      0. **（新設。P1-2 是正・Bugbot Medium 指摘への対応で実行順序を是正）**
         当該 world のチャンク走査を開始する前に、まずチャンク header
         （`version`／**`generation`**／`dim`／`slot_count`／`live_count`／
         `public_live`／`private_live`。`slot_bits`／`row_ids`／embedding 本体
         は含まない固定長部分）を読みデコードする。**デコードに失敗した場合は
         `Err` にせず**、ステップ 1（チャンク走査）を行わずに直接当該 tenant を
         丸ごと row-store 範囲走査へ tenant 単位フォールバックする（上記「5.
         チャンク単位可視性ビットマップ」節「チャンク header 自体のデコード
         失敗時は…」・上記「フォールバック契約」節と同一の帰結。この時点で
         当該 world のチャンク走査自体が未実施のため、破棄すべき既収集候補は
         当該 tenant の他 world 分に限られる）。デコードに成功したら、カタログの
         `world_completeness` エントリ（権威 `(generation, live_count)`）を
         点検索し、**今読んだ**チャンク header 自身が保持する
         `(generation, live_count)`（複製）と突合する（詳細・突合対象・
         マーカー破損時の扱いは下記「孤立行のオンライン検出（world 完全性
         マーカー）」節参照）。不一致・マーカーエントリ欠落はいずれも `Err`
         にせず、ステップ 1（チャンク走査）を行わずに直接当該 tenant を
         丸ごと row-store 範囲走査へ tenant 単位フォールバックする
      1. ステップ 0 を通過した world についてのみ、チャンク走査で live スロットを
         列挙する（`slot_bits` の live ビットのみを正とする。T3b で有効化する
         スキップ最適化は候補をさらに減らす最適化にすぎず、T3a では全 live
         スロットが対象）。このとき `slot_bits` から `popcount`（立っている
         live ビット数）を求め、ステップ 0 で読み取り済みのチャンク header
         自身が保持する `live_count`（複製。ステップ 0 で `world_completeness`
         と一致確認済みのため `world_completeness` の権威値の代理として扱える）
         と突合する（`slot_bits` のデコードはこの列挙で既に行っているため追加
         コストは popcount 計算のみ）。ステップ 0・ステップ 1 を合わせて
         `world_completeness.live_count` ↔ チャンク header `live_count` ↔
         `popcount(slot_bits)` の 3 値照合が完結する。一致しない場合は `Err`
         にせず、この時点までに収集した当該 tenant の全 world の候補を破棄した
         うえで当該 tenant を丸ごと row-store 範囲走査へ tenant 単位
         フォールバックする（詳細は下記「孤立行のオンライン検出（world 完全性
         マーカー）」節参照）
      2. 各候補スロットについて `user_rows` へポイントルックアップし、
         **metadata はまだデコードせず** `decode_row_tenant_and_visibility` の
         みで権威 tenant／visibility を取得する（header デコードのみ。これが
         row-store の走査中 header デコードに相当する権威データの取得手段）。
         取得直後、同じ header に含まれる `locator(chunk_no, slot)`
         （行フォーマット v3。下記「行単位レイアウトとの互換・移行方針」節
         参照）が走査元の `(chunk_no, slot)` と一致することを検査する
         （locator／逆参照の同一性検査。ポイントルックアップが `row_ids[slot]`
         の指す `id` を見つけられない miss を含む）。不一致・miss はいずれも
         スロットの占有者を特定できていない状態であり、可視性が未確定のまま
         `is_visible` 判定を行うことができない。**この場合は `Err` にせず、
         上記フォールバック契約に従い当該 tenant を丸ごと row-store の
         `tenant_id` 範囲走査（`chunk_no` フィルタなし）へ切り替える**（この
         検査は行側 `locator` を参照するため、破損しうる `chunk_no`
         フィールド自体を絞り込み条件に使う世界単位フィルタでは回収できない
         （上記「フォールバック契約」節「却下した代替案」参照）。テナント
         データに依存しない構造的検査という性質は変わらず、`is_visible`
         判定より前に行っても他テナントの存在情報を漏らさない。ただし帰結が
         `Err` ではなくフォールバックである点が旧来の記述からの変更点）
      3. ステップ 2 の locator／逆参照の同一性検査を通過した候補についてのみ、
         同ステップで取得した権威 tenant／visibility を
         `PolicyContext::is_visible` へ渡して RLS 判定を行う（これが row-store の
         `predicate` に相当する判定であり、チャンク側複製はこの判定に一切
         用いない）。**RLS 不可視と判定された候補は、残りの複製値検査（後述の
         3 点。以下「複製値検査」）を行わず・フォールバックも発動させず、ここで
         黙ってスキップする**（`continue` に相当。row-store の `arena.rs` が
         「header デコードが成功した不可視行は検証せずスキップする」挙動と
         同一の契約。他テナントの不可視行に対して複製ドリフト（`slot_bits`・
         可視性集計）を検証すること自体が、その行の存在・破損をフォールバック
         の発動有無〔観測不能とはいえ将来の計測経路〕経由で観測させる余地に
         繋がるため、可視性が確定する前の複製値検査は行わない。ただしステップ 2
         の locator／逆参照の同一性検査はテナントデータに依存しない構造的検査
         のため、この黙殺スキップの対象外——不可視候補であっても占有者を
         特定できなければステップ 2 の時点で既にフォールバックが発動している）
      4. **RLS 可視と判定された候補についてのみ**、残りの複製値検査（3 点。
         `row_ids[slot]` と取得した行の `id` の一致・チャンクキーの `tenant_id`
         と取得した行の `tenant_id` の一致・`slot_bits` の visibility ビットと
         取得した行の visibility の一致。locator の同一性はステップ 2 で検査
         済みのためここでは扱わない）を行う。1 つでも不一致なら、`Err` にも
         せず・黙って除外もせず、上記フォールバック契約に従い当該 tenant を
         丸ごと row-store 範囲走査へ切り替える（改ざん・破損を結果集合の
         縮小で隠蔽しない。誤った行を返さない fail-closed 性は、フォールバック
         先が権威経路であることによって担保される。この検査は可視と確定済みの
         候補についてのみ発生するため、フォールバック発動自体が他テナントの
         存在情報を漏らすことはない）
      5. 複製値検査を通過した候補（フォールバックが発動しなかった tenant）だけが
         次段へ進む。`WHERE`・宣言的フィルタ API を伴う経路（KNN・
         `SearchTimeFilter`・`batch_search`）は、ここで初めて metadata を
         デコードして SCALAR 段（`on_visible_row`）を評価し通過した行だけを
         アリーナへ積む（`arena.rs::build_filtered_with_rows` の `predicate` →
         `on_visible_row` の 2 段構成を維持しつつ、`on_visible_row` に必要な
         metadata デコードを複製値検査の**後**に結線する）。伴わない経路
         （フィルタなし KNN・`SearchTimeFilter`・`batch_search`）は metadata
         デコードを省略しそのままアリーナへ積む。フォールバックが発動した
         tenant（当該 tenant の全 world）については、この手順の代わりに
         row-store 範囲走査結果（同じ `predicate` → `on_visible_row` 経路）を
         そのままアリーナへ積む
      6. アリーナへ積まれた行だけが距離計算・ランキングの対象になる（不可視行は
         ランキング候補にすら含めない）
      この順序は row-store brute-force 経路（header デコード → `predicate`
      〔RLS 判定〕→ 不可視行は検証せずスキップ → アリーナ構築 → 距離計算・
      ランキング）と「header デコード〔権威取得〕→ RLS 判定 → 不可視行の
      黙殺スキップ → 可視行のみ以降の検証・デコード」という相対順序が完全に一致
      し、`WHERE` 付き経路・フィルタなし経路のいずれにも例外なく適用する
    - **コストへの帰結（両経路で対称。「コスト見積り」節に反映）**: 上記
      ステップ 2 のポイントルックアップ（header デコードのみ）は経路によらず
      「チャンク走査で候補になりうる全 live スロット数」分発生する
      （`O(候補 live スロット数)`。`slot_bits` スキップが T3b で有効化される
      までは概ね全 live スロット）。ステップ 2 の locator／逆参照の同一性検査は
      同じ header デコード結果（`locator(chunk_no, slot)` は行フォーマット v3
      の header に含まれる）に対して行うため、追加のポイントルックアップ・
      追加のデコードを伴わず、母数もステップ 2 のポイントルックアップと同じ
      `O(候補 live スロット数)` のまま増えない。ステップ 3 の `is_visible`
      判定はステップ 2 で得た権威値に対して行うため追加のポイントルックアップを
      伴わない。ステップ 4 の複製値検査（3 点）は、ステップ 3 で RLS 可視と
      判定された候補分のみに発生する（`O(RLS 可視行数)`。不可視候補は検証を
      経ずにステップ 3 でスキップされるため、複製値検査の母数から外れる）。
      ステップ 5 の metadata デコードは `WHERE`・宣言的フィルタ API を伴う経路
      でのみ、ステップ 3 で RLS 可視と判定された行数分だけ追加発生する
      （`O(RLS 可視行数)`。複製値検査と同じ母数）。**フォールバックはいずれの
      検出手段（ステップ 0/1・ステップ 2・ステップ 4）でも一律に tenant 単位で
      発動するため、発動した場合はその時点までに同 tenant の各 world で行って
      いたチャンク走査・ポイントルックアップ・locator 検査分すべてが無駄になり、
      当該 tenant の row-store `tenant_id` 範囲走査コストに退化する**（上限は
      「当該 tenant の row-store 経路のコスト + `O(当該 tenant の全 world の
      チャンク走査)`」）。tenant 単位フォールバックは発動しても他 tenant の
      chunked 経路には影響しない
    - **却下した代替案**:
      - **ランキング後の権威検証＋不足分の再取得（refill）**: チャンク側複製値
        （非権威）でランキングした上位候補に対し返却直前に権威検証を行い、
        不一致・不可視で落ちた分は候補プールの次点（まだランキングに含めて
        いない残りの live スロット）から補充して権威検証を繰り返し、k 件の
        可視行が揃うか候補プールを使い尽くすまで続ける案。非権威な複製値で
        ランキングしてから検証する構造そのものが、不可視行の embedding を
        一時的にランキング計算へ混入させ、下記「不変条件」のうち「不可視行が
        順位・件数に影響しない」を構造的に満たさない（不可視行の距離値が
        「誰が k+1 番目の再取得候補になるか」に影響し、再取得の有無・回数と
        いう観測可能な形で他テナントの存在情報が漏れうる）。この漏えい経路は
        本 ADR が維持すべき不変条件そのものと自己矛盾するため、候補段階の
        ポイントルックアップコストを抑えられる可能性の有無にかかわらず
        **採用しない**（Rejected。codex-review P0 指摘への対応）
      - **locator 不一致・複製値検査不一致を即 `Err` にする案（旧設計。不採用）**:
        統一順序を `is_visible` より前に置くことで Bugbot Medium 指摘
        （可視行の黙殺脱落）は解消できるが、`Err` の発生条件が「他テナント側の
        locator 状態・複製ドリフトの有無」に依存するため、呼び出し元は
        自テナントには一切不可視な他テナント行の保存状態の変化によってクエリの
        成否（`Err` になるか否か）が変わる経路を観測できてしまう。「テナント
        データを参照しない構造的検査」であっても、結果が他テナントの保存状態に
        依存する事実は変わらず、Issue #137 と同種のエラーオラクルになる
        （codex-review P0 指摘）。世界単位／tenant 単位フォールバック（上記
        採用案）は `Err` を発生させず結果集合を row-store 経路と同一化する
        ことで、Bugbot Medium・codex-review P0 双方の要求を同時に満たすため、
        この案は**不採用**とする（Rejected）
    - **不変条件（T3a・T5 の検証対象に追加。Bugbot Medium 指摘・codex-review P0
      指摘への対応）**:
      1. **短い結果集合を返さない**: 可視行が k 件以上存在するにもかかわらず、
         不可視行の混入・除外に起因して k 未満の結果が返ることはない
         （フォールバックにより結果集合が常に row-store 経路と同一になるため
         構造的に満たす。Bugbot Medium 指摘への対応）
      2. **不可視行が順位・件数に影響しない（存在情報漏えいなし）**: 他テナントの
         private 行を含む不可視行の embedding・存在が、返却される可視行の順位・
         件数・有無を左右しない（上記統一順序はランキング候補に不可視行を
         含めないため構造的に満たす）
      3. **複製ドリフト（locator 不一致・3 点複製値検査の不一致）が `Err`
         （エラーオラクル）として観測されない**: 他テナントの locator・複製値の
         ドリフト・破損によって呼び出し元へ `Err` が返ることは一切ない（上記
         フォールバック契約により、いずれのドリフトも row-store 範囲走査への
         切り替えとして処理され `Err` に到達しないため、構造的に満たす。
         codex-review P0 指摘への対応）
      4. **`Err` が発生し得るのは row-store 経路でも `Err` になる場面に限られる**:
         chunked 経路固有の複製ドリフトを理由に新たな `Err` クラスが追加される
         ことはない（`user_rows` 側 header デコード失敗等、row-store 経路が
         本来 `Err` にする破損に限り、フォールバック先の row-store 走査経由で
         `Err` が伝播しうる）
      4 条件は `crates/engine/tests/index_failure_injection.rs`（RECOVER-9）・
      `crates/engine/src/arena.rs` の構築時フィルタ検証群
      （`build_filtered_excludes_rows_failing_the_predicate_at_construction_time`
      等）と同種の「他テナントの不可視行が混在してもクエリ結果が縮まない・
      漏えいしない」ことを確認する回帰テストに加え、**ドリフト注入でフォールバックが
      発動し、結果が row-store 経路と一致し `Err` が出ないこと**を確認する
      回帰テストとして T3a・T5 に追加する（下記「実装タスク分解案」節の T3a・
      T5 行を参照）
    - **`slot_bits`／header 集計値によるスキップ最適化（T3b）は検査コストを
      削減する手段であり、安全性（不可視行を候補から除外する）の前提ではない**
      （上記「物理設計案」節・下記「5. チャンク単位可視性ビットマップ」節で
      規定した「除外方向は候補を減らす最適化にのみ作用し、admission の確定に
      使わない」原則の帰結）。上記統一順序でも、admission（可視・不可視の確定）
      の可否は常に `user_rows` 側権威 tenant／visibility に対する
      `PolicyContext::is_visible`（ステップ 3）のみで決まり、`slot_bits` の
      有効・無効（T3a 既定 off／T3b 以降 on）は安全性の結論を変えない。残りの
      複製値検査（ステップ 4・3 点）は admission が確定した**後**に可視行の
      データ対応関係を守るための検証であり、admission そのものの根拠には
      用いない（locator／逆参照の同一性検査はステップ 2 に属し admission の
      前提〔占有者の識別〕を守るための検査であり、性質が異なる）。T3b 有効化
      前後で変わるのはステップ 1 で列挙される候補の母数（＝ステップ 2 の
      ポイントルックアップ数）のみである
    - **locator／逆参照の同一性検査（ステップ 2。フォールバック契約。Bugbot
      Medium 指摘・codex-review P0 指摘への対応）**: `user_rows` へのポイント
      ルックアップで取得した行の header に含まれる `locator(chunk_no, slot)`
      （行フォーマット v3）が、走査元の `(chunk_no, slot)` と一致することを
      検査する。この検査はテナントデータ（tenant／visibility）を一切参照しない
      構造的整合性検査であり、`is_visible` 判定（ステップ 3）より前に置いても
      他テナントの存在情報を漏らさない。不一致・ポイントルックアップ自体の miss
      （`row_ids[slot]` の指す `id` が存在しない場合）は、いずれもスロットの
      占有者を特定できていない状態であり可視性を判定する前提が成立しないため、
      **`user_rows` 側の tenant が何であっても `Err` にはせず、当該 world
      （`(tenant_id, chunk_no)`）を丸ごと row-store 範囲走査へフォールバックする**
      （上記「フォールバック契約」節参照。旧設計〔他テナント行であっても常に
      `Err`〕はエラーオラクルになるため不採用。この検査は候補になりうる全 live
      スロットに対して発生する（`O(候補 live スロット数)`。ステップ 2 の header
      デコードと同じ母数・同じデータから追加のポイントルックアップなしに行える）
    - **残りの複製値検査の内容（3 点。フォールバック判定。codex-review P0 指摘・
      Bugbot Medium 指摘への対応）**: この経路（チャンク走査から `row_ids[slot]`
      を経由した点検索）は `layout: chunked`（v3）のテーブルに限って発生する
      （`layout: row` のテーブルは `user_vecs` を持たず本節の対象外。上記
      「物理設計案」節参照）ため、次の 3 点は常にすべて揃って検証できる
      （`locator(chunk_no, slot)` の一致は上記の locator／逆参照の同一性検査
      としてステップ 2 で先に検査済みのため、ここでは扱わない）:
      1. `row_ids[slot]` と取得した行の `id` が一致する
      2. チャンクキーの `tenant_id` と取得した行の `tenant_id` が一致する
      3. `slot_bits` の visibility ビットと取得した行の visibility が一致する
      この検証は上記統一順序のステップ 3 で **RLS 可視と判定された候補**分のみ
      に発生し（経路によらず `O(RLS 可視行数)`。チャンク走査そのものを O(N)
      から変えない）、本 ADR が狙う per-entry コスト削減の前提（下記「コスト
      見積り」節）と矛盾しない。admission（可視・不可視の確定）自体は常に
      `user_rows` 側の権威 tenant／visibility（ステップ 2 の header デコード・
      locator／逆参照の同一性検査・ステップ 3 の `is_visible`）に対して行われる
      ため、チャンク側 `slot_bits`／`row_ids` の複製ドリフトが admission の
      結論を誤らせることはない。残りの複製値検査が守るのは admission ではなく、
      **可視と確定した候補**について、チャンク走査で得た `row_ids`（複製）が
      指すスロットのデータ（embedding・行の対応関係）が実際にその行のもので
      あるという対応関係の整合である。不一致（例: 再パック中の書き込み途中像の
      混入等）を放置すると、可視行のはずが誤ったスロットの embedding に
      置き換わる形で結果集合を歪めるおそれがあるため、検証結果は「短い結果
      集合を黙って返す」のではなく、上記フォールバック契約に従い当該 tenant を
      丸ごと row-store 範囲走査へ切り替える（結果集合の縮小で改ざん・破損を
      隠蔽しない。fail-closed 性〔誤った行を返さない〕は、フォールバック先が
      権威経路であることで担保される）。ただし、この検証は **RLS 可視と
      判定された候補についてのみ**保証するものであり、次の 2 種類の候補は
      いずれも対象外である:
      - **不可視と判定された候補**（統一順序ステップ 3。locator／逆参照の
        同一性検査は通過済みで占有者は正しく特定できているが、権威
        tenant／visibility に基づく `is_visible` 判定で不可視と確定し、検証前に
        黙ってスキップされた候補）。他テナントの不可視行の複製ドリフト
        （`slot_bits`・可視性集計）をフォールバックの発動有無として観測可能に
        しない（エラーオラクル防止・観測可能性の最小化。Bugbot High 指摘への
        対応）ための意図的な除外であり、admission に関わる安全性上の要請による
      - **候補に到達する前**の `slot_bits`／header 集計値によるスキップ段階
        （T3b。ステップ 1 で候補を減らす方向にのみ作用する）で除外された候補。
        こちらは検査コストを削減するための最適化であり、動機が異なる（上記
        「除外方向」の限定的保証・下記「5. チャンク単位可視性ビットマップ」節
        参照）
- 行値（`user_rows`）の物理レイアウトは、下記「行単位レイアウトとの互換・移行方針」
  で採用する (2) opt-in 方式のもとでは **テーブルのカタログ `layout` 属性に従い
  2 形式が併存する**（codex-review P1 指摘への対応。一律に v3 化するとは規定しない）:
  - `layout: row`（既定・非 opt-in）: 現行の v2 のまま据え置く。embedding は
    引き続き行値に inline 格納され、既存のデコード経路（`decode_row_header`／
    `decode_row_embedding_and_metadata_into`）は無変更で動作する
  - `layout: chunked`（opt-in 済みテーブルのみ）: 行値に
    `locator(chunk_no u64, slot u16)` を追加した
    `[header | locator(chunk_no, slot) | embedding | metadata]` へ（行フォーマット
    v3）。**embedding 本体は行値から外さず、`user_rows` 側の値に引き続き権威
    （authoritative）コピーとして保持する**（P1-1 是正。旧設計〔下記却下案〕は
    embedding を行値から外し `user_vecs` チャンクを唯一の格納先とする案だった
    が、チャンク破損・locator 不一致でフォールバックした際に `user_rows` 側で
    距離計算用 embedding を取得できず、「row-store と同一の結果集合を返す」
    「新しい `Err` クラスを生じさせない」というフォールバック契約〔下記「権威
    検証を行うタイミング」節〕が成立しなかったため撤回する。`user_vecs`
    チャンクは embedding の**唯一の格納先ではなく**、権威コピー〔`user_rows`〕
    から走査高速化のために作られた**派生複製**という位置づけに改める）。
    v3 は `layout: chunked` のテーブルにのみ書き込まれ、`layout: row` のテーブルの
    行が v3 になることはない。この行→チャンクの locator は「行 id からチャンク
    位置を引く」順方向専用であり、上記 `row_ids` 配列（チャンク→行の逆方向）と
    役割が異なる。再パック（下記「削除（tombstone）」節）でスロットが移動した
    場合はチャンク側 `row_ids` の更新に加え、移動した行の `user_rows` locator も
    同一 txn 内で更新する（`row_ids` と `locator` の不一致を残さない）。
    書き込み経路は従来どおり `user_rows`（embedding 込み v3 行値）とチャンク
    （`user_vecs` の複製 embedding・`row_ids`・`slot_bits`）を同一 write txn で
    更新する（下記「4. 世代整合」節・T2）
  - **採用する設計判断（トレードオフ）**: 上記のとおり `user_rows` に embedding
    権威コピーを残すため、`layout: chunked` テーブルは embedding 分のストレージを
    実質約 2 倍（`user_rows` 側 + `user_vecs` 側）消費する（詳細・数値は下記
    「コスト見積り」節「空間」参照）。これはフォールバック時に権威経路
    （row-store 相当の `user_rows` 範囲走査）が単独で距離計算まで完結できる
    ことの対価であり、本 ADR は可用性・結果集合の正しさをストレージコストより
    優先する
  - **却下した代替案（`user_rows` から embedding を除く案）**: 当初案では
    `layout: chunked` の行値から embedding を完全に外し `user_vecs` チャンクを
    embedding の唯一の格納先とすることで `user_rows` 側の行値を縮小し、
    `COUNT(*)`／集計／`GROUP BY`／TASK-120 の path 一致走査（下記「T3a の対象
    範囲・集計の読み取り元」節参照）の leaf ページ密度を上げる副次効果を狙って
    いた。しかしこの案では、チャンク破損・locator 不一致・複製値検査不一致で
    当該 tenant を `user_rows` 範囲走査へフォールバックしても、その範囲走査は
    embedding を持たない行しか読めず KNN の距離計算が行えない。フォールバック
    先で距離計算不能というのは row-store 経路には存在しない新たな失敗モードで
    あり、フォールバック契約の前提（row-store 経路と同一の結果集合になる・
    新しい `Err` クラスを生じさせない）を満たせない。**採用しない**
  - デコーダはテーブルの `layout` 属性を読み、v2/v3 いずれの形式で読むかを分岐
    する（`ROW_FORMAT_VERSION` の単純な拒否ではなく、テーブル単位の属性で経路を
    切り替える）。異なる `layout` の行が同一テーブル内に混在することはない
    （テーブル単位の属性でありレイアウト変更はオフライン再構築ツール経由の
    全件書き直しでのみ発生するため）
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
は再パック対象とする。

**集計値の原子更新不変条件（codex-review P1 指摘への対応）**: `slot_bits` の
live ビットはチャンク単位・テナント単位（`public_live`／`private_live`）の
集計値の分解であり複製ではない。header の `live_count`・`public_live`・
`private_live` は「そのチャンク内で該当条件を満たす live スロットの個数」を
表すため、live ビットを 1 本落とす／立てる操作は必ず対応する集計値の
増減と対になる。したがって `delete_row`（tombstone）・`update_row`（embedding
差し替え・visibility 変更を含む）・TASK-120 置換の削除側・再パックのいずれの
経路も、対象スロットの `slot_bits` 更新と `live_count`・`public_live`・
`private_live` の増減を**同一 write txn 内で原子的に**行う（一方のみ更新して
commit する中間状態を作らない）。この不変条件が崩れると「4. 世代整合」節・
「5. チャンク単位可視性ビットマップ」節のチャンク単位スキップ判定が実際の
live スロット集合と乖離し、可視であるべき行が誤ってスキップされる／削除済み
行が再パック判定に誤って算入される。将来 `COUNT(*)` を集計値のみから算出する
最適化（下記「T3a の対象範囲・集計の読み取り元」節の例外候補）を検討する場合も
この原子性が前提になる。検証対象:

- T2（書き込み経路の結線）: 単体テストで、削除・更新（embedding 差し替え・
  visibility 変更）・TASK-120 置換の各経路について、コミット後のチャンク値
  `live_count`／`public_live`／`private_live` が実スロット走査結果（`slot_bits`
  の live ビット集計）と一致することを確認する。加えて、下記「5. チャンク単位
  可視性ビットマップ」節「孤立行のオンライン検出（world 完全性マーカー）」の
  `world_completeness` エントリもこの原子性の対象に含め、コミット後の
  `world_completeness.(generation, live_count)` が、チャンク header 自身が
  複製として保持する `(generation, live_count)` フィールドと一致すること
  （両者がコミットのたび同一 write txn で揃って更新されること）を同じ単体
  テストで確認する
- T5（回復検証の拡張）: `index_failure_injection.rs`（RECOVER-9）・
  `power_loss.rs` に、削除・更新・再パックの途中失敗／部分書き戻し像で
  `slot_bits` と `live_count`／`public_live`／`private_live` が不整合な状態の
  まま commit されないこと（fail-closed 拒否、または commit 前失敗として無傷に
  戻ること）を検証対象として明記する
- T8（帯域外整合性検証ツール）: 運用時の恒久的な担保として、世界単位で
  `live_count`／`public_live`／`private_live`／`slot_bits` を `user_rows` 全件
  走査結果と突合し、この原子性が破れた場合（実装バグ・bit-rot 等）の残存
  リスクを検出する

再パック方式の比較:

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
対象へチャンク書き込みモジュールを追加する必要がある。下記「5. チャンク単位
可視性ビットマップ」節「孤立行のオンライン検出（world 完全性マーカー）」の
`world_completeness` エントリ更新も、この既存の同一 write txn に相乗りする
だけであり、独自の commit 境界・追加の `bump_table_generation_in_txn` 呼び出し
位置を新設しない。

### 5. チャンク単位可視性ビットマップ

チャンク header の `public_live`／`private_live` 集計値を用い、
`PolicyContext::is_visible(tenant, Public)` と `is_visible(tenant, Private)` の
両方が false となるチャンクは値本体（f32 部）を読まずスキップする。

- 判定は必ず `PolicyContext::is_visible` の単一照合パスを通す（独自のテナント
  比較を新設しない。[security.md](../../.claude/rules/security.md) P0）
- テナント単位キーのため要求元テナント以外のチャンクを `range` 走査で開かない
  設計も可能だが、`Public` 行は他テナントからも可視のため全テナント走査は残る
  （可視性判定の順序契約は現行と同じに保つ）
- **チャンク header 自体のデコード失敗時は「不可視だからスキップ」と判断せず、
  当該 world（`(tenant_id, chunk_no)`）を丸ごと `user_rows` 範囲走査へ
  フォールバックする**（上記「権威検証を行うタイミング」節の「フォールバック
  契約」と同一の帰結。row-store 経路はそもそもチャンクを読まないため、
  チャンク header のデコード失敗は row-store 経路が `Err` にする破損クラスに
  該当しない——`Err` にすると row-store 経路には存在しない新たな失敗モードを
  chunked 経路にのみ追加することになる。したがってチャンク header 破損は
  「即 `Err`」ではなく他の複製ドリフトと同じくフォールバックの発動契機として
  扱う）
- **チャンク単位・スロット単位のいずれも、`slot_bits`／header 集計値による判定は
  「候補を減らす」方向にのみ作用してよく、「候補を確定的に admit する」方向には
  使わない**（codex-review P0 指摘への対応。上記「物理設計案」節の単一権威
  原則を可視性ビットマップの運用へ適用したもの）。デコード失敗・意味的不整合の
  いずれも同じフォールバック契約で扱うため、両者を異なる fail-closed クラスとして
  区別する必要はない:
  - **デコード失敗**（本節冒頭のチャンク header 破損）: チャンク値自体が壊れて
    読めないケース。フォールバック（上記）を発動する
  - **意味的不整合**（チャンク側複製と `user_rows` 側権威値の食い違い）: チャンク
    値は正常にデコードできるが、複製した visibility／`row_ids` が権威と一致しない
    ケース。デコードは成功するため本節の判定だけでは検出できず、上記「権威検証を
    行うタイミング」節の統一順序のうち、`locator(chunk_no, slot)` の不一致は
    ステップ 2 の locator／逆参照の同一性検査（`is_visible` 判定より前・
    **tenant 単位**フォールバック発動。検出手段を問わず一律に tenant 単位で
    回収する契約〔上記「フォールバック契約」節参照〕の一環）で、`row_ids[slot]`・
    tenant・`slot_bits` visibility の不一致はステップ 3 の `is_visible` 判定を
    経てステップ 4 で行う残りの複製値検査（3 点。こちらも同じ契約に従い
    **tenant 単位**フォールバック発動）で検出・フォールバック発動とする。
    ただし後者（ステップ 4 の
    複製値検査）の検出は **RLS 可視と判定された候補**（残りの複製値検査節参照）
    にのみ保証される。次の 2 種類の候補はいずれもステップ 4 の複製値検査の対象に
    含まれない（locator／逆参照の同一性検査はステップ 2 でテナントを問わず行う
    ため、この 2 種類の限定の対象外）:
    1. 本節のチャンク／スロットスキップ（`slot_bits`／header 集計値による T3b
       最適化）で除外され、そもそもポイントルックアップへ進まなかった候補
    2. ポイントルックアップ・header デコード・locator／逆参照の同一性検査には
       通過した（占有者は正しく特定できている）が、権威 tenant／visibility に
       基づく `is_visible` 判定（統一順序ステップ 3）で不可視と確定し、残りの
       複製値検査の前に黙ってスキップされた候補

    2 は admission 判定を権威値のみで行い、不可視候補を検証せずスキップする
    row-store（`arena.rs`）と同一の契約であり、他テナントの不可視行の複製
    ドリフトをフォールバックの発動有無としても観測させない（Issue #137 と同種の
    エラーオラクルを避ける。Bugbot High 指摘への対応）ための意図的な設計である。
    1 の「候補を減らす最適化」とは動機が異なる（1 はコスト最適化、2 は
    エラーオラクル防止という安全性上の要請）。デコードは正常に成功したまま
    複製値が誤っている（bit-rot 等のコミット後破損）ケースで、**可視行**が
    誤って除外される事象は、上記 1（T3b スキップ）の範囲でのみクエリ経路の
    検証だけでは検出できない残存リスクとして残る（ポイントルックアップに到達
    した候補はフォールバックで回復するため、この残存リスクは「候補にすら到達
    しない」T3b 除外方向の最適化に限定される。上記「物理設計案」節
    「admission 方向／除外方向」を参照）。2（不可視候補の黙殺スキップ）は
    安全性上意図的な設計であり「残存リスク」ではない
  - **除外判定自体の整合性検証（T8。上記の残存リスクへの対応。破損チャンクの
    修復は T8 の責務とする）**: 除外方向の正しさは書き込み時の同一 txn 不変条件
    （「4. 世代整合」節・T2）とコミット境界を跨ぐ障害注入検証（T5）でのみ担保
    され、コミット後の複製値のみの破損はクエリ経路では検出されない。この残存
    リスクをクエリの都度検出することはコスト見積りの前提（per-entry コスト
    削減）と両立しないため、帯域外（クエリのホットパス外）の**世界単位
    （`(tenant_id, chunk_no)` 単位）整合性検証ツール**を T8 として実装タスク
    分解案に追加する。T8 は各 world について `live_count`／`public_live`／
    `private_live`／`slot_bits` を対応する `user_rows` エントリ集合の全件走査
    結果と突合し、不一致を operator が対処すべき fail-closed エラーとして報告
    する（`crash_tool` 系の運用者専用サブコマンドと同様の位置付け。クエリ応答
    経路には結線しない）。下記「孤立行のオンライン検出（world 完全性
    マーカー）」節で導入する `world_completeness` の `(generation, live_count)`
    もこの全件走査結果との突合対象に含め、マーカー自体の破損・ドリフト
    （オンライン検出が正しく機能しているかそのもの）を T8 が帯域外で
    バックストップする。**T8 が実装されるまでは、`layout: chunked` テーブルの
    チャンク header／`slot_bits` によるスキップ最適化（本節・「残りの複製値検査の
    内容」節の対象を減らす方向の最適化）を有効化しない**。これにより「不一致を
    黙って除外しない」契約は、admission 方向（無条件・フォールバック経由で
    回復）に加え除外方向（T8 による帯域外検証込みで運用時に担保）の双方に
    ついて、最適化を有効化する前提条件として一貫させる

#### 孤立行のオンライン検出（world 完全性マーカー。P1-2 是正）

上記「除外判定自体の整合性検証（T8）」は帯域外（運用者が任意のタイミングで
実行する）ツールであり、T8 を実行するまでの間は孤立行（上記「権威検証を行う
タイミング」節「フォールバック契約」の「「row-store 経路と同一の結果集合に
なる」という前提」参照）が生じていてもクエリ経路からは検出できない
（codex-review P1-2 指摘）。これは「他のスロットが整合していればフォール
バックが発動せず黙って欠落する」という欠落であり、T8 という**事後**検出だけ
では「クエリ時の row-store 同等性」を保証したことにならない。これを埋める
ため、以下のオンライン完全性マーカーをクエリのホットパスへ追加する:

- **データモデル**: `catalog.rs` へ新規テーブル `world_completeness`
  （`TableDefinition<(&str, &str, u64), (u64, u16)>` 相当。キー
  `(table_name, tenant_id, chunk_no)`・値 `(generation, live_count)`）を追加
  する。`TABLE_GENERATION_TABLE`（`catalog.rs`）と同じ「未作成 = 0」規約は
  **採らない**——世界単位マーカーでは未作成（エントリ欠落）を `live_count = 0`
  （＝当該 world に生存行なし）と解釈すると、初回書き込み前のスロットに
  対する fail-open な黙認になりかねない。したがって**エントリの欠落は
  「一致しない」（＝マーカー未整備）として扱い、無条件にフォールバックへ回す**
  （後述「マーカー自体が破損・欠落している場合」参照）
- **書き込み経路の同一 txn 不変条件**: `world_completeness` の当該 world
  エントリ、および上記「物理設計案」節で新設したチャンク header 自身の
  `generation`／`live_count` フィールドは、その world のチャンク値
  （`slot_bits`／`live_count`）を更新するすべての書き込み経路（`insert_row`・
  `delete_row`（tombstone）・`update_row`・TASK-120 置換・再パック）が、
  チャンク側 `live_count` 更新（「2. 削除（tombstone）」節「集計値の原子更新
  不変条件」参照）と**同一 write txn 内**で更新する（`world_completeness` 側
  ・チャンク header 側のいずれか一方のみを更新して commit する中間状態を
  作らない）。`generation` は `catalog::bump_table_generation_in_txn` と同型の
  `checked_add` 単調増加カウンタとし、当該 world への書き込みのたび `+1` する
  （テーブル単位世代〔`TABLE_GENERATION_TABLE`〕とは別カウンタであり、世界
  単位に閉じる。`world_completeness` 側が権威、チャンク header 側は同一 txn で
  書き戻される複製）。この不変条件により、コミット直後は
  `world_completeness.(generation, live_count)` == チャンク header
  `(generation, live_count)` == `(_, popcount(slot_bits))` の 3 値が常に
  一致する（「4. 世代整合」節参照。`bump_table_generation_in_txn` 等の既存
  commit 前呼び出し位置は変更しない——本マーカーは既存の同一 txn に相乗りする
  だけで新たな commit 境界を作らない）
- **クエリ時の突合（3 値照合）**: 上記「権威検証を行うタイミング」節の統一
  順序ステップ 0（チャンク header をデコードし〔デコード失敗は tenant 単位
  フォールバック〕、`world_completeness` 権威値 `(generation, live_count)` と
  今読んだチャンク header 自身の `(generation, live_count)` 複製を突合）・
  ステップ 1（ステップ 0 を通過した world についてのみ、チャンク走査で得た
  `slot_bits` の `popcount` 実測値とステップ 0 で読み取り済みのチャンク header
  `live_count` との再突合）で 3 値を照合する。**いずれか 2 値でも不一致なら、
  `Err` にはせず**
  当該 tenant を丸ごと row-store 範囲走査へ tenant 単位フォールバックする
  （上記「権威検証を行うタイミング」節「フォールバック契約」と同一の帰結——
  `world_completeness`
  も `layout: chunked` にのみ存在する非権威側の検証用複製であり、チャンク
  header のデコード失敗・`slot_bits` 不一致と同じ「chunked 経路にのみ存在する
  複製ドリフト」のクラスに属する。row-store 経路には `world_completeness` を
  参照する場面自体が無いため、これを `Err` にすると row-store 経路には存在
  しない新たな失敗モードを chunked 経路にのみ追加することになり、上記「権威
  検証を行うタイミング」節が禁じる「複製ドリフトが `Err` として観測される」
  状態を作る）。**このオンライン検出が構造的にカバーするのは、チャンク側
  （`slot_bits`／`live_count`）自体が破損し、行を指す live スロットが失われる
  ケースに限る**: このケースでは、生存行が孤立行になった瞬間（自身の
  locator が指すチャンクの live スロットから到達できなくなった瞬間）に、
  その home world の実際の live 集合と `world_completeness`／チャンク header
  の `live_count` が必ず乖離する（同一 txn 不変条件の下ではこのクラスの
  孤立行の発生自体が `live_count` の更新漏れと等価であるため）。したがって
  この孤立行は当該 world のチャンク走査**開始時**に必ず検出され、当該 tenant
  を丸ごと row-store 範囲走査へ tenant 単位フォールバックする——「他スロットが
  整合していればフォールバックが発動せず黙って欠落する」という P1-2 指摘の
  欠落は生じない。
  **一方、チャンク側は無傷のまま行自身の `locator.chunk_no`（または `slot`）
  のみが破損するケース（上記「フォールバック契約」節「「row-store
  経路と同一の結果集合になる」という前提」参照）では、home world 側の
  3 値（`world_completeness`・チャンク header `live_count`・`popcount`）は
  いずれも変化しないため、本マーカーの 3 値照合では検出できない**。この
  ケースは本マーカー（ステップ 0/1）ではなく、チャンク走査が当該行の live
  スロットへ実際に到達した時点のステップ 2（locator／逆参照の同一性検査）で
  検出し、同じく tenant 単位フォールバックへ回すことで回収する（「開始時」
  ではなく「当該行の候補到達時」に検出される点が本マーカーとは異なる）。
  この 2 つの検出経路（ステップ 0/1 のマーカー突合・ステップ 2 の locator
  検査）は検出タイミングこそ異なるが、いずれも同じ tenant 単位フォール
  バックへ回すため、**同一 world 内でチャンク側破損（ステップ 0/1 検出）と
  行側 locator 破損（ステップ 2 検出）が併発しても、ステップ 0/1 の検出時点で
  当該 tenant の全 world が row-store 範囲走査（`chunk_no` フィルタなし）に
  置き換わっており、ステップ 2 が本来検出するはずだった行も `tenant_id`
  一致のみで取りこぼさず回収される**（旧設計〔検出手段別に世界単位／
  テナント単位を使い分ける契約〕ではこの併発ケースで取りこぼしが生じて
  いた。上記「フォールバック契約」節「却下した代替案」参照）。孤立行の
  2 クラスいずれについても「他スロットが整合していればフォールバックが
  発動せず黙って欠落する」という P1-2 指摘の欠落が生じない
- **マーカー自体が破損・欠落している場合**: `world_completeness` テーブル自体が
  デコードできない、または当該 world のエントリが欠落している場合も、
  **`Err` にはせず**当該 tenant を丸ごと row-store 範囲走査へ tenant 単位
  フォールバックする（上記と同じ理由——`world_completeness` はチャンク header や `slot_bits` と
  同じ「chunked 経路にのみ存在する非権威複製」であり、その破損・欠落を
  `Err` にすると、要求元テナントから不可視な他テナントの world の保存状態
  （マーカーが壊れているか・書き込まれているか）によってクエリの成否が
  変わるエラーオラクルになる。上記「権威検証を行うタイミング」節「フォール
  バック契約」・Bugbot Medium 指摘・codex-review P0 指摘と同じ理由での
  fail-closed 判断であり、`user_rows` 側 header デコード失敗〔row-store 経路
  でも `Err` になる破損クラス〕とは異なるクラスとして扱う）。この場合の
  「照合に用いる権威値そのものが読めない」という状態は、フォールバック先の
  row-store 範囲走査そのものが `user_rows` 側 header デコード失敗で `Err` に
  なるケースとは独立であり、混同しない
- **保証範囲の限定（スコープ）**: `world_completeness` が権威として持つのは
  `live_count`（生存スロット数）のみであり、`public_live`／`private_live`
  （visibility 別内訳）は対象に含めない。したがって本マーカーが構造的に
  保証するのは「**チャンク側の破損に起因する孤立行が生じていないこと**」
  であり、「特定のスロットの visibility ビットが正しいこと」までは保証しない
  （`live_count` が変わらないまま特定スロットの visibility ビットのみが
  ドリフトするケースは本マーカーでは検出されず、引き続き上記「権威検証を
  行うタイミング」節ステップ 4 の複製値検査〔RLS 可視候補に対する
  `row_ids[slot]`・tenant・`slot_bits` visibility の 3 点突合〕と T8 の帯域外
  検証に委ねる）。**同様に、行側 `locator`（`chunk_no`／`slot`）のみが破損する
  孤立行の網羅性も本マーカーの保証範囲外である**（上記「クエリ時の突合
  （3 値照合）」節参照。このクラスはステップ 2 の locator／逆参照の同一性検査
  と tenant 単位フォールバックが担う。本マーカーが構造的に保証する網羅性は
  「チャンク側破損由来の孤立行が無いこと」に限定され、「行側 locator 破損由来の
  孤立行が無いこと」まではステップ 2 と合わせて初めて保証される）。この限定に
  より、T3b（`slot_bits`／header 集計値によるスキップ最適化の有効化）を
  T8 完了前に有効化しない方針（上記「除外判定自体の整合性検証（T8）」参照）は
  変更しない——本マーカーが単独で構造的に保証するのはチャンク側破損由来の
  網羅性（孤立行の不在）のみであり、除外方向の視認性最適化に必要な
  「visibility ビットの正しさ」までは保証しないため

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
| `crates/engine/tests/power_loss.rs` | `StorageBackend` 差し替え電源断モデル | 部分書き戻し像でのチャンク値と locator の不整合を検出し、フォールバックすること（`Err` にはならず、結果が row-store 経路と一致すること）の確認。`row_ids[slot]` ⇔ `user_rows` 側 `locator` の相互不整合（再パック中の電源断を含む。ステップ 2 の検出）・`slot_bits` visibility ⇔ `user_rows` 側 tenant／visibility の不整合（権威検証。上記「権威検証を行うタイミング」節参照。ステップ 4 の検出）のいずれも**検出手段を問わず tenant 単位**フォールバックが発動し結果が row-store 経路と一致することを確認する（上記「フォールバック契約」節参照）。加えて、**同一 world 内でこの 2 種類の不整合が併発する場合**（例: あるチャンクの `slot_bits` visibility ドリフトと、同じ world をホームとする別行の `locator` bit-rot が同時に生じる）でも tenant 単位フォールバックにより両方の不整合が取りこぼしなく回収され結果が row-store 経路と一致することを確認する（Bugbot Medium 指摘への対応。旧設計〔検出手段別に世界単位／テナント単位を使い分ける契約〕ではこのケースで取りこぼしが生じていた）。`user_rows` 側 header デコード失敗（row-store 経路でも `Err` になる破損）のみ引き続き fail-closed で `Err` とする |
| `crates/engine/tests/index_failure_injection.rs`（RECOVER-9） | 再構築処理の途中失敗注入 | チャンク再パック処理の途中失敗で commit 済み行が無傷であることの確認。途中失敗後も `row_ids` と `locator` が旧配置のまま一貫していること（半端な書き換えが残らないこと）、および `slot_bits` visibility と行側 tenant／visibility の一致が崩れていないことを含める。`row_ids`／行側 `locator` の一致が崩れた状態を注入した場合、`slot_bits` visibility と行側 tenant／visibility の一致が崩れた状態を注入した場合のいずれも**検出手段を問わず tenant 単位**フォールバックが発動し、結果が row-store 経路と一致すること（`Err` にならないこと）を検証対象に加える。加えて、同一 world 内で両不整合を同時注入した場合も tenant 単位フォールバックにより取りこぼしなく回収され結果が row-store 経路と一致することを検証対象に含める |

`recovery/commit_boundary.rs` の choke point 経由は維持する。

## コスト見積り

- **読み取り（フィルタなし経路。`WHERE`・宣言的フィルタ API を伴わない KNN・
  `SearchTimeFilter`・`batch_search`）（Bugbot High 指摘・codex-review P1 指摘
  への対応で見積りを改訂。上記「権威検証を行うタイミング」節参照）**: 本経路の
  コストは性質の異なる 2 種類のアクセスの和であり、**両者を一括りに `1/N` 償却
  として扱わない**（codex-review P2 指摘への対応。以前の記述は「走査＋
  ポイントルックアップ＋ヘッダデコード」を一体として `1/N` に償却されると
  読める書き方をしていたが、根拠を欠いていたため分離する）:
  1. **チャンク走査（連続アクセス・`1/N` 償却対象）**: チャンク table 自体の
     走査・チャンク header のデコードは、走査＋ヘッダデコードのみを単離した
     現行の per-entry コスト（#362 実測の S2 ≈56.4ns/行。上記「現状の機構」節
     参照。`COUNT(*)` の SQL 表層合成コスト ≈225ns/行はここでは用いない）に
     対応する処理を N 行分の 1 チャンクへ集約するため `1/N` 償却が見込める
     （25,000 行・N=256 では約 98 チャンクまで削減される計算。この数値は #362 の
     S2 実測を基準とし、SQL 表層合成コストを含まない）。これは redb の
     leaf ページを前から連続して読み進める点で現行の `table.iter()` 走査と
     同じアクセスパターンであり、償却の根拠は #362 の単離実測の延長線上にある
  2. **`user_rows` 点検索（ランダムアクセス・償却されない）**: 権威検証の
     統一順序（ステップ 2・3）では、チャンク走査で候補になりうる**全 live
     スロット**について `user_rows` への B-tree ポイントルックアップ（キー
     `(tenant_id, id)` での探索）＋エントリ取得＋ header デコードのみのデコード
     と、同じ header デコード結果に対する locator／逆参照の同一性検査が発生する
     （`slot_bits` スキップが T3b で有効化されるまでは概ね走査対象の可視候補
     全体）。この点検索は `O(候補 live スロット数)` 項を持ち、**チャンク走査の
     `1/N` 償却の対象外**（母数〔候補 live スロット数〕はチャンク化によって
     縮小しない。locator／逆参照の同一性検査は追加のポイントルックアップを
     伴わないため `O()` の項を増やさない。「この経路のポイントルックアップは
     最終的な返却行数分のみに限られる」という以前の見積りは撤回する）。
     **さらに、この点検索 1 回あたりのコストが現行の連続走査 1 行あたりの
     コスト（#362 実測の S2 ≈56.4ns/行。上記 1. と同じ基準）と同等である保証も
     ない**——B-tree のランダムなポイントルックアップ（ページキャッシュの
     ヒット率が連続走査より低下しうる）は、一般に連続走査より 1 回あたりの
     コストが高くなりうる。T3b の `slot_bits` スキップが有効化される T8 完了前
     （T3a の時点）は、このスキップが既定無効のため候補 live スロット数 ≈
     走査対象の可視行数全体（≈ チャンク化前の全行数 N_scan に近い）となり、
     点検索コストの母数が実質的に縮小しない。したがって **可視率が高く点検索が
     支配的な条件（T3a 時点・RLS 可視行が走査対象の大半を占めるワークロード）
     では、チャンク走査側の `1/N` 償却分（上記 1. の #362 S2 基準）を点検索側の
     ランダムアクセスコストが上回り、フィルタなし経路の総コストが現行の連続
     走査（#362 実測の S2 ≈56.4ns/行）より悪化しうる**（T3b の `slot_bits`
     スキップ有効化後は候補 live スロット数が縮小しこの条件は緩和される見込み
     だが、それも実測が前提）。**P1-1 是正（`user_rows` に embedding 権威コピー
     を維持）により、この点検索が触れる `user_rows` の行値サイズは
     `layout: row`（v2）と同等のまま縮小しない**（dim=128 で ≈540B 程度。下記
     「副次効果」節参照）。header デコードのみで embedding 本体は読まないため
     デコードコスト自体は変わらないが、redb の leaf ページあたりに収まる
     エントリ数（ページ密度）は行値縮小案より低いままであり、上記の
     「ランダムアクセスは一般に連続走査より 1 回あたりのコストが高くなりうる」
     という悪化方向の懸念を弱めない（強めることはあっても弱めることはない）
  ステップ 3 の `is_visible` 判定で不可視と確定した候補はここで検証なしに
  スキップされるため、続くステップ 4 の残りの複製値検査（3 点）は RLS 可視と
  判定された候補分のみに発生する（`O(RLS 可視行数)`）。この経路は metadata
  デコード（ステップ 5）を行わないため、フィルタなし経路の総コストは
  `O(world 数)`（ステップ 0・`world_completeness` へのポイントルックアップ。
  下記「5. チャンク単位可視性ビットマップ」節「孤立行のオンライン検出（world
  完全性マーカー）」参照。`O(候補 live スロット数)` に比べ十分小さい加算項
  だが、走査対象 world 数が多いワークロードでは無視できない）
  `+ O(N_scan / N) 償却分`（チャンク走査。上記 1.）
  `+ O(候補 live スロット数)`（`user_rows` 点検索・header デコード＋
  locator／逆参照の同一性検査。上記 2.・非償却）
  `+ O(RLS 可視行数)`（残りの複製値検査）の和になる。**この総コストの実測は
  #362 の測定設計（`docs/design/knn-stage-profile.md` の S1〜S4 段構成）に
  「S2' 点検索」相当の段（S1 の連続走査に対し、同じ行集合を `(tenant_id, id)`
  でランダムに `user_rows` へ点検索した場合の ns/行）を追加して行うことを
  提案する（本 ADR は提案のみで #362 側の実装は行わない。T7 の効果実測着手前に
  この段を含めた計測を行い、上記の悪化しうる条件が実際に生じるかを採否判断の
  前提に加える）**。ランキング後に不足分を再取得する案は、非権威な複製値で
  ランキングしてから検証する構造ゆえ不可視行の embedding がランキング計算に
  混入し、再取得の有無・回数を通じて他テナントの存在情報が漏れうるため却下した
  （上記「権威検証を行うタイミング」節「却下した代替案」参照。コスト面の利点は
  不変条件〔存在情報漏えいなし〕を満たさない代償としては採用しない）。KNN の
  arena 構築段（≈468ns/行 の残り ≈243ns/行 = f32 デコード＋コピー）は連続
  コピー化により減少が見込まれる（定量は #362 の実測で確定させる。本 ADR では
  上限見積りとして提示するに留める）。**フォールバックが発動する tenant がある
  場合**、上記の見積りに加え、当該 tenant の全 world 分の無駄になったチャンク
  走査・ポイントルックアップ・locator 検査のコストと、`user_rows` の
  `tenant_id` 範囲走査（row-store 経路と同一コスト）が加算される（コストは
  「当該 tenant の row-store 経路のコスト + `O(当該 tenant の全 world の
  チャンク走査)`」が上限であり、フォールバックが chunked 経路のコストを
  完全に相殺することはない）。フォールバックの発生頻度・レイテンシへの
  寄与は T7 の効果実測（またはプロセス内メトリクス）で計測し、クエリ結果・
  エラー応答としては現れない（上記「権威検証を行うタイミング」節「フォールバック
  契約」参照）
- **読み取り（metadata 必須経路。`WHERE`・宣言的フィルタ API を伴うクエリ）**:
  上記の per-entry 削減はチャンク走査・ヘッダデコード段と embedding 分（f32
  デコード・コピー）にのみ適用され、metadata 分は適用されない。この経路の
  `user_rows` ポイントルックアップも上記フィルタなし経路の 1./2. と同じ性質
  （連続アクセスの `1/N` 償却 対 ランダムアクセスの非償却。ランダムアクセス
  1 回あたりのコストが現行の連続走査より高くなりうる点・悪化しうる条件も同様）
  であり、以下ではその差分（metadata デコード項の追加）のみを記す。権威検証の
  統一順序（上記「権威検証を行うタイミング」節）に従い、ポイントルックアップ・
  locator／逆参照の同一性検査自体はフィルタなし経路と同じ母数（候補になりうる
  全 live スロット数）で発生するが、残りの複製値検査（3 点）・metadata の
  デコード（SCALAR 段評価に必要）はいずれもステップ 3 で RLS 可視と判定された
  行数分だけ発生する。読み取りコストのモデルは次の 5 項の和で表す:
  `O(world 数)`（ステップ 0・`world_completeness` へのポイントルックアップ。
  上記フィルタなし経路と同じ加算項）・
  `O(N_scan / N) 償却分`（走査＋ヘッダデコード＋ embedding デコード）・
  `O(候補 live スロット数)` の `user_rows` ポイントルックアップ＋locator／
  逆参照の同一性検査（header デコードのみ）・`O(RLS 可視行数)` の残りの
  複製値検査（3 点）・`O(RLS 可視行数)` の metadata デコード（SCALAR 段評価
  込み）。フィルタなし経路との差は最後の metadata デコード項のみであり、
  「フィルタなし経路と metadata 必須経路のポイントルックアップ母数のオーダーが
  近づく」という以前の見積りは、権威検証の統一順序のもとでは両経路の
  ポイントルックアップ母数が常に同一（`O(候補 live スロット数)`）であることの
  帰結として撤回・置換する（両者の差は候補母数のオーダーではなく、metadata
  デコード項の有無に限られることを明確にする。残りの複製値検査（3 点）の母数
  〔`O(RLS 可視行数)`〕は両経路で同一）。RLS 可視行数が走査対象の大半を占めるワークロードでは、
  metadata デコード項が per-entry 削減効果を相殺しうる。T7 の効果実測では、
  この 2 経路（フィルタなし／metadata 必須）を分離して個別に測定し（測定条件:
  同一コーパス・同一 N に対し `WHERE` 句の有無のみを変えた A/B）、フィルタなし
  経路の改善幅と metadata 必須経路の改善幅（またはその欠如）を別々に報告する
- **副次効果（P1-1 是正により撤回）**: 旧設計（`user_rows` から embedding を
  外す案。上記「物理設計案」節「却下した代替案（`user_rows` から embedding を
  除く案）」参照）が見込んでいた `COUNT(*)`／集計／`GROUP BY`／TASK-120 の
  path 一致走査（metadata のみ参照）での leaf ページ密度向上（dim=128 で行値
  ≈540B → ≈60B 程度への縮小）は生じない。`layout: chunked` の `user_rows`
  行値サイズは `layout: row`（v2）と同等のまま据え置かれる（`locator` 分
  〔`chunk_no: u64` + `slot: u16` ＝ 10 バイト〕のみ増加。詳細は上記「T3a の
  対象範囲・集計の読み取り元」節参照）。Issue #350 の embedding 非参照集計
  デコードスキップ（`DecodeTier`）はデコードコスト削減として引き続き有効
  だが、ページ読み取り数の削減効果は伴わない
- **書き込み**: 1 行挿入/削除あたり `user_rows` 側の行値全体（embedding 込み・
  v2 相当サイズ）の再書き込みに加え、チャンク値全体の再書き込み
  （`N × dim × 4 + N × 8` バイト）が同一 txn 内で発生する。P1-1 是正前の見積り（チャンク値
  のみが追加コスト）と異なり、**1 行挿入/削除の書き込み増幅は「行値本体（従来
  どおり）＋チャンク値全体の再書き込み」の合算**になる。TASK-120 の一括投入
  （ファイル単位で複数行）ではチャンク単位にまとまるため増幅は相対的に小さい
  が、単発 DML（`insert_row`/`delete_row`/`update_row`）では悪化するため、N の
  上限設定と想定ワークロードの前提を明記する必要がある
- **空間**: `layout: chunked` テーブルは embedding 分のストレージを実質約 2 倍
  （`user_rows` 側の権威コピー + `user_vecs` 側の派生複製）消費する（上記
  「物理設計案」節「採用する設計判断（トレードオフ）」参照。dim=128 では
  embedding 部だけで 1 行あたり 512B の重複となる）。これに加え tombstone 分の
  浮き（再パック閾値で上限を設ける）・チャンク header オーバーヘッド（N あたり
  数十バイト）・新設 `world_completeness` エントリ（world あたり
  `(u64, u16)` ＝ 10 バイト程度）が空間コストとして加わる

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

(2) を採用する場合、(1) の「行フォーマット v3 へ一斉移行」は選択しない
（(1)／(2) は互いに排他的な選択肢であり、本 ADR は (2) のみを推奨する）。
`layout: row`（既定・非 opt-in）のテーブルは `locator` を追加した v3 形式へは
移行せず、v2（embedding inline）のまま恒久的に維持される。v3（行フォーマット。
`locator` を追加するが embedding は `user_rows` 側の権威コピーとして引き続き
保持する。上記「物理設計案」節参照）が
書き込まれるのは `layout: chunked` へ opt-in 済みのテーブルに限られる（上記
「物理設計案」節参照）。これにより、opt-in していないテーブルの検索経路が
embedding を取得できなくなる退行は生じない。

旧 `ROWS_TABLE`（`Storage::put` 系・`txn.rs`・`crash_tool` 系）はテーブルスコープ
行ストア（`user_rows/{table}`）とは別系統であり、本 ADR の対象外とする。

## Issue #363（arena キャッシュ）との関係・採否判断の材料

Issue #363 が SQL 表層に世代整合 arena キャッシュを導入すると、同一世代内の反復 KNN
では arena 再構築自体が発生しなくなるため、チャンク化による KNN 側の利得は
「コールドスタート・書き込み後の再構築・`SearchTimeFilter`（動的ポリシー）・
`batch_search`」に限定される（下記「T3a の対象範囲・集計の読み取り元」のとおり
集計（`SUM`／`GROUP BY`／`HAVING` 等）はチャンク走査へ移らず `user_rows`
を走査し続けるため（行値サイズは P1-1 是正により縮小しない。上記「T3a の
対象範囲・集計の読み取り元」参照）、この一覧から除く）。

判断基準案: #362 の測定設計（`docs/design/knn-stage-profile.md` の S1〜S4 段別
内訳。SQL 表層合成コストである ≈468ns/行 全体との比率ではなく、S1〜S4 各段間
差分での比率を指す）の実測で「走査・ヘッダデコード段」が内訳の支配項であり、
かつ書き込み頻度が高く #363 のキャッシュヒット率が低いワークロードを重視する
場合に採用する。そうでなければ Rejected（または Deferred）とする、という判断
分岐を申し送る。**この判断は上記「コスト見積り」節の非償却項（`user_rows`
点検索）を無視すると誤って通過しうる**（codex-review P2 指摘への対応）ため、
上記「S2' 点検索」相当の段（#362 の測定設計への追加提案）を実測したうえで、
「走査・ヘッダデコード段の削減幅」が「S2' 点検索コスト（`O(候補 live スロット
数)`）の増分」を上回ることを追加の採用条件とする。T3a 時点（`slot_bits`
スキップ既定無効）では候補 live スロット数が縮小しないため、この条件は T8 完了
・T3b 有効化後の実測（またはその見込みの事前検証）を前提として判定する。

## 実装タスク分解案（Issue 起票は行わない）

本 ADR 承認後、[out-of-scope-tracking.md](../../.claude/rules/out-of-scope-tracking.md)
の承認制に従いユーザー承認を得たうえで Issue を起票する。以下は分解「案」のみ。

| # | タスク | 依存 | 主な対象 |
| - | ------ | ---- | -------- |
| T1 | チャンクテーブル定義・コーデック（header 検証〔新設 `generation` フィールドを含む固定長部分のデコードを含む〕・上限定数・`unsafe` なし）＋ `world_completeness` カタログテーブル定義（`(table_name, tenant_id, chunk_no)` → `(generation, live_count)`。上記「5. チャンク単位可視性ビットマップ」節「孤立行のオンライン検出（world 完全性マーカー）」参照） | — | `storage/chunk.rs`（新規）・`catalog.rs`（`user_vecs`・`world_completeness` 命名・`drop_table`） |
| T2 | 書き込み経路の結線（挿入・削除・更新・TASK-120 置換・tombstone・同一 txn）。**チャンク側 `live_count` 更新と同一 write txn 内で `world_completeness` の当該 world エントリ（`generation`・`live_count`）と、チャンク header 自身が複製として保持する `generation`・`live_count` フィールドの双方を更新する**（上記「孤立行のオンライン検出」節「書き込み経路の同一 txn 不変条件」参照。呼び忘れは T8 の帯域外検証まで検出されない孤立行の再発に直結するため、対象の書き込み経路すべてに結線すること） | T1 | `tenant.rs`・`recovery/ledger.rs` 連携・`table_generation_bump_coverage.rs` |
| T3a | 読み取り経路の結線（`VectorArena`・`SearchTimeFilter`・`batch_search` のチャンク走査＋権威検証〔「権威検証を行うタイミング」節の統一順序（**ステップ 0・`world_completeness` 突合**→header デコード→locator／逆参照の同一性検査（不一致・miss は同一 tenant の全 world の既収集候補を破棄し `user_rows` の当該 `tenant_id` 範囲走査〔`chunk_no` フィルタなし〕へ**tenant 単位**フォールバック。`Err` にはしない）→ `is_visible` 判定→不可視行は検証せず黙ってスキップ→可視行のみ残りの複製値検査（3 点。`slot_bits` の `popcount` と `world_completeness`／チャンク header `live_count` の 3 値照合を含む。不一致は検出手段を問わず同一 tenant の全 world の既収集候補を破棄し**tenant 単位**フォールバック）→ metadata／SCALAR）。フィルタなし経路も同節の統一順序で実装する〕。集計は下記「T3a の対象範囲・集計の読み取り元」のとおりチャンク走査へ移さない。チャンク header／`slot_bits` によるスキップ最適化はコード上実装するが**既定無効**（フィーチャーフラグ／設定で off）とし、T3b（T8 完了後）まで有効化しない。検証には実際に書き込まれたチャンク〔T2〕と `layout: chunked` 選択・v2/v3 分岐〔T4〕の両方が必要なため依存に含める。**検証対象に「権威検証を行うタイミング」節の不変条件（短い結果集合を返さない・不可視行が順位/件数に影響しない・複製ドリフトが `Err` として観測されない・`Err` は row-store 経路でも `Err` になる破損に限られる）を追加**〔Bugbot Medium 指摘・codex-review P0 指摘への対応。他テナントの不可視行・private 行を混在させたコーパスで、可視行が k 件以上あるとき返却が k 未満へ縮まらないこと、不可視行の有無で可視行の順位・件数が変化しないこと、他テナントの複製ドリフト（locator 不一致・3 点検査不一致・チャンク header 破損）が `Err` として観測されないこと、`row_ids[slot]` がドリフトして無関係な不可視行を指す場合にスロットが黙って落ちず**当該 tenant が tenant 単位フォールバックし結果が row-store 経路と一致すること**（フォールバック発動テスト。locator／逆参照の同一性検査の直接回帰）、**チャンク側（`slot_bits`・`live_count`・`world_completeness`）を無傷のまま生存行 1 件の `user_rows` 側 `locator.chunk_no` のみを A→B へ破損させ（3 値照合はいずれも通過する条件を保ったまま）、home world A のチャンク走査がこの行に到達した際に tenant 単位フォールバックが発動し結果集合が row-store 経路と一致すること（P1 是正の直接回帰。3 値照合が通過する条件下での検証であることを明記し、マーカー検出との重複による vacuous pass を避ける）**、および**同一 world 内でチャンク側破損（`world_completeness` の 3 値照合不一致）と行側 `locator.chunk_no` の bit-rot（別の生存行）を併発させ、ステップ 0/1 が検出した時点で発動する tenant 単位フォールバックが後者の行も取りこぼさず結果集合が row-store 経路と一致すること**（Bugbot Medium 指摘への直接回帰。旧設計〔検出手段別に世界単位／テナント単位を使い分ける契約〕ではこの複合破損ケースで取りこぼしが生じていた）を回帰テスト化する〕） | T1・T2・T4 | `arena.rs`・`rls.rs`・`batch_search.rs`・`sql/aggregate.rs`・`sql/group_by.rs` |
| T3b | T3a で実装したチャンク header／`slot_bits` によるスキップ最適化の有効化（既定 off → on への切り替え）。T8（帯域外整合性検証ツール）の完了を前提条件とする（下記「除外判定自体の整合性検証」節参照。**T8 未完了のまま有効化しない**） | T3a・T8 | `arena.rs`・`rls.rs`・`batch_search.rs`（フラグ切り替えのみ） |
| T4 | カタログのレイアウト属性と opt-in・オフライン再構築ツール。`drop_table` の同一 txn 削除対象へ `user_vecs/{table}` と併せて `world_completeness` の当該テーブル分エントリを追加する | T1 | `catalog.rs`・`examples/` |
| T5 | 回復検証の拡張（crash_tool サブコマンド・cross-table 3 テーブル・power_loss・RECOVER-9 注入。「2. 削除（tombstone）」節「集計値の原子更新不変条件」の途中失敗検証を含む。**T3a と同じ「短い結果集合を返さない」「不可視行が順位/件数に影響しない」「複製ドリフトが `Err` として観測されない・`Err` は row-store 経路でも `Err` になる破損に限られる」不変条件を、途中失敗・再構築後の状態でも保つことを RECOVER-9 注入の検証対象へ追加。チャンク側破損由来のドリフト注入・行側 `locator.chunk_no`／`slot` のみの破損注入（チャンク側・`world_completeness` は 3 値照合が通過する条件のまま）のいずれも**検出手段を問わず tenant 単位**フォールバックが発動し、結果が row-store 経路と一致すること（`Err` が出ないこと）を含める。加えて、同一 world 内で両者を併発させる注入でも tenant 単位フォールバックにより取りこぼしなく回収され結果が row-store 経路と一致することを含める。加えて、`world_completeness` エントリの更新のみが途中失敗した状態（チャンク側 `live_count` は更新済み・`world_completeness` は旧値のまま、またはその逆）を再現できないこと（同一 txn 内不変条件により commit 単位で両者は必ず揃うこと）を検証対象に追加する**〔Bugbot Medium 指摘・codex-review P0 指摘・P1-2 指摘への対応〕） | T2 | `scripts/`・`examples/crash_tool*.rs`・`tests/` |
| T6 | 再パック（コンパクション）と閾値。再パックで `row_ids`／`user_rows` locator が更新される場合も `live_count`（したがって `world_completeness`）自体は不変（生存スロット数は変わらない）ことを維持する | T2 | `storage/chunk.rs`・`tenant.rs` |
| T7 | 効果実測（`feature_bench` 前後比較・`sql_c1_bench`・#362 内訳との突合）と README「実装方針（要点）」反映。**`world_completeness` 突合（ステップ 0）・複製値検査の 3 値照合（ステップ 1）の追加コストを含めて計測する**（上記「コスト見積り」節参照） | T3b・T4 | `examples/feature_bench.rs`・`docs/` |
| T8 | 世界単位（`(tenant_id, chunk_no)` 単位）整合性検証ツール（`live_count`／`public_live`／`private_live`／`slot_bits` と対応する `user_rows` 全件走査結果の突合。運用者専用・クエリ応答経路には結線しない。「5. チャンク単位可視性ビットマップ」節「除外判定自体の整合性検証」参照。「2. 削除（tombstone）」節「集計値の原子更新不変条件」の恒久的な帯域外担保を兼ねる）。**孤立行のうちチャンク側破損由来のクラスはステップ 0（`world_completeness` 突合）によりオンラインで担保され、行側 `locator` 破損由来のクラスはステップ 2（locator／逆参照の同一性検査・tenant 単位フォールバック）によりオンラインで担保されるため（P1-2 是正。上記「孤立行のオンライン検出（world 完全性マーカー）」節参照）、T8 の役割は「オンライン検出そのものが正しく機能しているか」の帯域外バックストップ（`world_completeness` エントリ自体の破損・欠落・`generation`／`live_count` の実測値からのドリフトの検出）と、オンライン検出の保証範囲外である per-slot visibility ドリフト（`public_live`／`private_live`／`slot_bits` visibility ビット）の網羅性検証に位置づけを改める。**T8 の突合は `row_ids[slot]`（チャンク→行）と行側 `locator`（行→チャンク）の**双方向**で行い、片方向のみの突合では検出できない行側 `locator` 単独破損（`row_ids[slot]` は正しいが行側 `locator` が別のチャンクを指す）も帯域外でバックストップする** | T2 | `examples/crash_tool*.rs` 系（新規サブコマンド）・`storage/chunk.rs` |

### T3a の対象範囲・集計の読み取り元（codex-review P1 指摘への対応）

提案するチャンク値（`row_ids`・embedding・`slot_bits`）には `SUM`／`AVG`／`MIN`／
`MAX`／`GROUP BY`／`HAVING` が要求するスカラー metadata が含まれない。metadata は
引き続き `user_rows` 側にのみ存在するため、これらの集計は **チャンク走査へは
移さず、`user_rows` を走査し続ける**（`sql/aggregate.rs`・`sql/group_by.rs` は
`user_rows` の `range`／`iter` 走査を維持し、チャンクテーブルには触れない）。
対象テーブルが `layout: chunked`（opt-in 済み）の場合、走査する `user_rows` は
`locator(chunk_no, slot)` を追加した行フォーマット v3 だが、**P1-1 是正により
embedding は行値から外れず引き続き含まれる**（上記「物理設計案」節「採用する
設計判断（トレードオフ）」参照）ため、行値サイズは `layout: row`（v2）と
実質同等のまま（`locator` 分のみ微増）である。T3a における
`sql/aggregate.rs`・`sql/group_by.rs` への変更は「チャンク走査への切り替え」では
なく、v3（`locator` フィールドが挿入された行）へ追随するデコード経路の
オフセット更新に限定する。embedding デコードのスキップ自体は Issue #350 の
`DecodeTier` によるデコードコスト削減として引き続き有効だが、行値サイズが
縮小しないため leaf ページ読み取り数の削減は伴わない（詳細は上記「コスト
見積り」節「副次効果（P1-1 是正により撤回）」参照）。`layout: row`
（既定・非 opt-in）のテーブルでは行値は引き続き v2（embedding inline）の
ままであり、集計の走査・デコード経路は本 ADR による変更を受けない。

チャンクからポイントルックアップして `user_rows` を走査する利点を失う設計
（全チャンク→全行ポイントルックアップ）は採らない。

例外として `COUNT(*)`（`WHERE` を伴わず metadata 不要な場合に限る）は、チャンク
header の `live_count`／`public_live`／`private_live` 集計値を可視性判定に用いて
`user_rows` を一切走査せずに算出できる余地があるが、この最適化は本 ADR の対象外
（将来検討）とし、T3a では実装しない。`WHERE`・宣言的フィルタ API を伴う集計は
metadata 参照が必須のため、上記「権威検証を行うタイミング」節の統一順序と同じ
理由で `user_rows` 走査を維持する。**この将来最適化は「物理設計案」節・
「5. チャンク単位可視性ビットマップ」節で定めた単一権威原則（チャンク側複製は
候補を減らす方向にのみ使い、確定的な admit には使わない）の唯一の例外候補である
ことに注意する**。`COUNT(*)` は個々の行を返却しないため、統一順序のステップ 2
（`user_rows` へのポイントルックアップ）を行う対象行自体が存在せず、上記の
検証パターンをそのまま適用できない。したがって、この最適化を将来実装する場合は
`live_count`／`public_live`／`private_live` 集計値そのものの改ざん・破損検出
（チャンク書き込み経路が同一 txn 内で集計値と行側 tenant／visibility を同時に
更新することの保証、および集計値と実スロット走査の定期的な整合検証等）を伴う
別途の整合性保護境界を設計してからでなければ採用しない。この保護境界の設計を
伴わずに集計値のみで `user_rows` 走査を省略する実装は、本 ADR が禁じる
「チャンク側複製のみで確定的に可視と判定する」パターンに該当するため採用しない
（codex-review P0 指摘への対応）。

## セキュリティ考慮（OWASP Top 10 + AGENTS.md P0）

| 観点 | 本タスク（ADR 執筆）での対応 | ADR が将来実装に課す要件 |
| ---- | ---------------------------- | ------------------------ |
| private spec 漏えい（P0） | spec 本文・非公開設計議論を転記していない。参照は TASK-nn／ビヘイビア ID／パスのみ。数値は public Issue #344／#361 由来の実測値と本リポ定数に限定 | — |
| 秘密情報混入（P0） | 実トークン・資格情報は記載していない | — |
| テナント境界（P0） | — | 可視性判定は `PolicyContext::is_visible` の単一照合パスのみを用いる。チャンクスキップは同 predicate の結果から導出し、独自テナント比較を新設しない。チャンクはテナント単位キー。エラーに他テナントの存在情報を含めない。**チャンク側 `slot_bits`／header 集計値・`world_completeness` エントリはいずれも `user_rows` 側行ヘッダの tenant／visibility から派生した複製であり単一権威ではない**（`user_rows` 側行ヘッダが権威。`embedding` 本体も P1-1 是正により `user_rows` 側が権威コピーで `user_vecs` チャンク側は走査高速化用の派生複製である）。単一権威原則は方向で保証範囲が異なる: **admission 方向は無条件**——チャンク走査を開始する前に、まずチャンク header をデコードし（デコード失敗なら以降を行わずフォールバック）、`world_completeness`（権威 `(generation, live_count)`）と今読んだチャンク header 自身の `(generation, live_count)` を突合し（ステップ 0）、ステップ 0 を通過した world についてのみチャンク走査を行い `slot_bits` から求めた `popcount` とステップ 0 で読み取り済みのチャンク header `live_count` を再突合する（ステップ 1。ステップ 0・1 を合わせて `world_completeness.live_count` ↔ チャンク header `live_count` ↔ `popcount(slot_bits)` の 3 値照合が完結する）。いずれか 2 値でも不一致・マーカーエントリ欠落・チャンク header デコード失敗なら `Err` にはせず**当該 tenant を丸ごと**（`chunk_no` フィルタなし。検出手段を問わず一律に tenant 単位で回収する契約〔下記〕に統一しているため）row-store 範囲走査へフォールバックする——これにより「孤立行（生存行が到達不能になり黙って結果集合から欠落する事象）」のうちチャンク側破損由来のクラスはクエリ時にオンラインで検出される（P1-2 是正。詳細は「5. チャンク単位可視性ビットマップ」節「孤立行のオンライン検出（world 完全性マーカー）」参照）。この点検を通過した world についてのみ、チャンク走査で候補になりうる全 live スロットについて、まず `user_rows` へポイントルックアップし header デコードのみで権威 tenant／visibility を取得する。取得直後、テナントデータに一切依存しない構造的整合性検査として、取得した行の header が走査元の `(chunk_no, slot)` を指し戻すこと（locator／逆参照の同一性検査）を確認し、不一致・ポイントルックアップ自体の miss は、いずれも**`Err` にはせず**、そのチャンク（world = `(tenant_id, chunk_no)`）の chunked 高速経路を放棄し、**同一 tenant の全 world について既収集候補を破棄したうえで `user_rows` の当該 `tenant_id` 範囲走査（`chunk_no` フィルタなし。embedding は `user_rows` 側の権威コピーから取得）へ tenant 単位フォールバックする**（`chunk_no` フィルタ付きの世界単位フォールバックでは、絞り込み条件自体（行側 `locator.chunk_no`）が破損しうる同じフィールドに依存するため、同一 world 内でチャンク側破損（ステップ 0/1）と行側 `locator` 破損（本ステップ）が併発した場合に取りこぼしを生む（Bugbot Medium 指摘）。この漏れを構造的に塞ぐため、検出手段を問わずすべてのフォールバック発動契機で `chunk_no` フィルタを行わず、常に tenant 単位で回収する契約へ統一する（上記「権威検証を行うタイミング」節「フォールバック契約」「却下した代替案」参照）。**RLS の `predicate`／`PolicyContext::is_visible` 判定自体はこの範囲拡張の影響を受けず、フォールバック先の権威経路〔`user_rows` 側ヘッダ〕でそのまま適用される**——`chunk_no` フィルタを外すのは locator ベースの world 分割のみであり、fail-closed 性は変わらない。詳細・却下案は上記「権威検証を行うタイミング」節「フォールバック契約」参照。他 tenant の候補は影響を受けない。codex-review P0 指摘・P1 指摘への対応——`Err` にすると他テナント側の locator 状態でクエリの成否が変わるエラーオラクルになるため）。フォールバック先の row-store 走査自体が `Err` を返す場合（`user_rows` 側 header デコード失敗等、row-store 経路でも `Err` になる破損）は、その `Err` をそのまま伝播する。この検査を通過した候補についてのみ `PolicyContext::is_visible` による RLS 判定を行う。**RLS 不可視と判定された候補はここで残りの複製値検査（3 点）を行わずフォールバックも発動させず、黙ってスキップする**（他テナントの不可視行の `slot_bits`・可視性集計のドリフトを観測させないため。Issue #137 と同種のエラーオラクルを避ける設計であり、row-store〔`arena.rs::build_filtered_with_rows_and_limits_in_txn`〕が header デコード成功後に不可視行を検証なしでスキップする挙動と同一の契約。Bugbot High 指摘への対応）。**RLS 可視と判定された候補についてのみ**、`WHERE` 経路・フィルタなし経路のいずれも距離計算・ランキングより前（`WHERE` 系はさらに metadata デコードより前）に残りの複製値検査（`row_ids[slot]`・tenant・`slot_bits` visibility の 3 点突合。locator の同一性は上記のとおり `is_visible` 判定より前で全候補に対し検査済みのため、ここでは扱わない。`layout: chunked` に限って発生するため常に 3 点とも検証する）を行い、不一致も同様に `Err` にせず・黙って除外もせず、**当該 tenant を丸ごと**（検出手段を問わず tenant 単位で回収する契約に統一しているため `chunk_no` フィルタは用いない）row-store 範囲走査へフォールバックする（誤った行を返さない fail-closed 性は、フォールバック先が権威経路であることで担保される。詳細な順序は「権威検証を行うタイミング」節参照）。**除外方向（`slot_bits`／header 集計値によるチャンク・スロットスキップ）は保証が限定的**——ポイントルックアップに到達する前の除外はこの複製値検査の対象外であり、コミット後の複製値のみの破損による誤除外はチャンク走査だけでは検出できない残存リスクとして明示する（ポイントルックアップに到達した候補はフォールバックで回復するため、残存リスクは「候補にすら到達しない」除外方向の最適化に限定される）。**`world_completeness` マーカーが構造的に保証するのはチャンク側破損由来の孤立行が無いこと（生存行数の一致）のみであり、行側 `locator` のみの破損由来の孤立行の網羅性（ステップ 2・tenant 単位フォールバックが担う）や `public_live`／`private_live`／`slot_bits` visibility ビットの個別の正しさまでは保証しない**（上記「孤立行のオンライン検出」節「保証範囲の限定（スコープ）」参照）ため、visibility ビットの残存リスクは世界単位の帯域外整合性検証ツール（T8。`row_ids` ⇔ 行側 `locator` の双方向突合を含む）を前提として運用時に担保し、**T8 実装まで除外方向の最適化を有効化しない**（「権威検証を行うタイミング」節・「5. チャンク単位可視性ビットマップ」節参照）。**フィルタなし経路（`WHERE`・宣言的フィルタ API を伴わない KNN・`SearchTimeFilter`・`batch_search`）も、上記の統一順序（header デコード→locator／逆参照の同一性検査→ `is_visible` 判定→不可視行は検証せず黙ってスキップ→可視行のみ残りの複製値検査）でランキング前の候補段階に権威検証を行う（ランキング後に不足分を再取得する案は、非権威な複製値でランキングしてから検証する構造ゆえ存在情報漏えいの残存差分があり Rejected として却下済み。locator 不一致・複製値検査不一致を即 `Err` にする旧案も、他テナント側の保存状態でクエリの成否が変わるエラーオラクルになるため Rejected として却下済み）ことで「短い結果集合を返さない」「不可視行が順位・件数に影響しない」「複製ドリフトが `Err` として観測されない」「`Err` は row-store 経路でも `Err` になる破損に限られる」の 4 不変条件を満たすことを実装タスク（T3a・T5）の検証対象とする（フォールバックの有無・回数は結果集合・エラー応答には現れず〔呼び出し元の wire 可視契約には影響しない〕、レイテンシ差のみが残存としてプロセス内メトリクス／T8 の帯域外検証の対象となる——フォールバックは検出手段を問わず一律に tenant 単位（blast radius は常に当該 tenant の全 world）で発動し、この点も wire 可視契約には現れない。Bugbot Medium 指摘・codex-review P0 指摘への対応。「権威検証を行うタイミング」節参照） |
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

ADR の Accepted 化・実装タスク（T1〜T8）の Issue 起票・README「実装方針（要点）」
への反映は、いずれもオーナー承認後の別作業とする
（[out-of-scope-tracking.md](../../.claude/rules/out-of-scope-tracking.md) の
承認制）。

## 参照

- 「現状の機構」節のコードポインタ
- 先行 ADR: [table-generation-rejection-granularity.md](./table-generation-rejection-granularity.md)・
  `crash-tolerance-reverification.md`・`multi-dim-table-coexistence.md`・
  `c1-p95-dedicated-env-reverification.md`
