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
  - **`slot_bits` の visibility ビット・header の `public_live`／`private_live`
    集計値は、いずれも `user_rows` 側行ヘッダの tenant／visibility から** `insert_row`
    **等の書き込み経路が同一 txn 内で複製した派生値であり、権威（authoritative）
    な値ではない**。権威は常に `user_rows` 側行ヘッダ（`decode_row_tenant_and_visibility`）
    にある。この単一権威原則を「admission 方向」と「除外方向」で分けて適用する
    （下記「権威検証を行うタイミング」節・「5. チャンク単位可視性ビットマップ」節。
    codex-review P0 指摘への対応。両節の分割契約が唯一の正で、本節はその要約）:
    - **admission 方向（無条件に保証）**: チャンク側複製の値だけを根拠に行を
      可視と確定して admit することは一切行わない。ポイントルックアップに到達した
      候補は必ず `user_rows` 側権威値で検証し、不一致は `Err`（黙って除外しない）
    - **除外方向（`slot_bits`／header 集計値によるスキップ最適化。保証範囲が
      限定的）**: この最適化はポイントルックアップに到達する前に候補を減らす
      ためのものであり、除外判定自体は自己検証されない。正しさは書き込み経路の
      同一 txn 不変条件（下記「4. 世代整合」・T2）とコミット前後の障害注入検証
      （T5）に依拠し、コミット後の複製値のみの破損（デコードは成功するが値が
      誤っている——bit-rot 等）で本来可視な行が誤って除外される事象はクエリ
      経路では検出できない残存リスクとして明示的に許容する。この残存リスクは
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
    他の節はこの記述のみを参照する）**:
    - **row-store（`layout: row`）との対応**: 現行の
      `arena.rs::build_filtered_with_rows_and_limits_in_txn` は `user_rows` を
      直接走査するため、走査中に得た行エントリの header デコード
      （`decode_row_tenant_and_visibility`）がそのまま権威 tenant／visibility を
      無償で与える（追加のポイントルックアップは発生しない）。`predicate`
      （RLS 段）はこの header デコード結果に対して適用され、通過した行だけが
      embedding・metadata のデコード（`decode_row_embedding_and_metadata_into`）・
      `on_visible_row`（SCALAR 段）・アリーナへの push（ランキング候補化）へ進む
      固定順序であり、本 ADR はこの順序を変えない（冒頭「現状の機構」表 #3 参照）
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
      存在しない。以前の記述はこの向きを取り違えており、権威判定前の全候補
      読み取りコストを過小評価していた。codex-review P1 指摘への対応）
    - **統一順序（単一のルール。段階〔候補時点／返却時点〕による例外を設けない。
      以前の記述は「返却直前」の経路のみを規定し `WHERE` 経路の候補時点
      ポイントルックアップを対象外としていたため、`slot_bits` で admit された
      private 行の public 複製が metadata デコードだけで返却されうる欠落が
      あった。Bugbot High 指摘への対応）**:
      1. チャンク走査で live スロットを列挙する（`slot_bits` の live ビットのみを
         正とする。T3b で有効化するスキップ最適化は候補をさらに減らす最適化に
         すぎず、T3a では全 live スロットが対象）
      2. 各候補スロットについて `user_rows` へポイントルックアップし、
         **metadata はまだデコードせず** `decode_row_tenant_and_visibility` の
         みで権威 tenant／visibility を取得する（header デコードのみ）
      3. 4 点検証（後述）を行う。1 つでも不一致なら可視・不可視のどちらとも
         確定せず、`is_visible` による再判定も行わず、**黙って除外もせず**
         fail-closed で `Err` を返す（改ざん・破損を結果集合の縮小で隠蔽しない）
      4. 一致時のみ、行側の権威値（チャンク側複製ではなく）を
         `PolicyContext::is_visible` へ渡して RLS 判定を確定する（これが
         row-store の `predicate` に相当する判定であり、チャンク側複製はこの
         判定に一切用いない）
      5. RLS 可視と判定された候補だけが次段へ進む。`WHERE`・宣言的フィルタ API
         を伴う経路（KNN・`SearchTimeFilter`・`batch_search`）は、ここで初めて
         metadata をデコードして SCALAR 段（`on_visible_row`）を評価し通過した
         行だけをアリーナへ積む（`arena.rs::build_filtered_with_rows` の
         `predicate` → `on_visible_row` の 2 段構成を維持しつつ、
         `on_visible_row` に必要な metadata デコードを 4 点検証の**後**に結線
         する）。伴わない経路（フィルタなし KNN・`SearchTimeFilter`・
         `batch_search`）は metadata デコードを省略しそのままアリーナへ積む
      6. アリーナへ積まれた行だけが距離計算・ランキングの対象になる（不可視行は
         ランキング候補にすら含めない）
      この順序は row-store brute-force 経路（RLS predicate → アリーナ構築 →
      距離計算・ランキング）と同じ「RLS 判定通過後・ランキング前」の相対位置に
      一致し、`WHERE` 付き経路・フィルタなし経路のいずれにも例外なく適用する
    - **コストへの帰結（両経路で対称。「コスト見積り」節に反映）**: 上記
      ステップ 2 のポイントルックアップ（header デコードのみ）は経路によらず
      「チャンク走査で候補になりうる全 live スロット数」分発生する
      （`O(候補 live スロット数)`。`slot_bits` スキップが T3b で有効化される
      までは概ね全 live スロット）。ステップ 5 の metadata デコードは
      `WHERE`・宣言的フィルタ API を伴う経路でのみ、ステップ 4 で RLS 可視と
      判定された行数分だけ追加発生する（`O(RLS 可視行数)`）。フィルタなし経路は
      ステップ 5 の metadata デコード分のコストを払わないが、ステップ 2 の
      ポイントルックアップ自体は `WHERE` 付き経路と同じ `O(候補 live スロット数)`
      を払う。「フィルタなし経路だけが返却行数分のポイントルックアップで済む」
      という以前の見積りは撤回する
    - **却下した代替案: ランキング後の権威検証＋不足分の再取得（refill）**:
      チャンク側複製値（非権威）でランキングした上位候補に対し返却直前に権威
      検証を行い、不一致・不可視で落ちた分は候補プールの次点（まだランキングに
      含めていない残りの live スロット）から補充して権威検証を繰り返し、k 件の
      可視行が揃うか候補プールを使い尽くすまで続ける案。非権威な複製値で
      ランキングしてから検証する構造そのものが、不可視行の embedding を一時的に
      ランキング計算へ混入させ、下記「不変条件」のうち「不可視行が順位・件数に
      影響しない」を構造的に満たさない（不可視行の距離値が「誰が k+1 番目の
      再取得候補になるか」に影響し、再取得の有無・回数という観測可能な形で
      他テナントの存在情報が漏れうる）。この漏えい経路は本 ADR が維持すべき
      不変条件そのものと自己矛盾するため、候補段階のポイントルックアップ
      コストを抑えられる可能性の有無にかかわらず**採用しない**（Rejected。
      codex-review P0 指摘への対応）
    - **不変条件（T3a・T5 の検証対象に追加。Bugbot High 指摘への対応）**:
      1. **短い結果集合を返さない**: 可視行が k 件以上存在するにもかかわらず、
         不可視行の混入・除外に起因して k 未満の結果が返ることはない（上記
         統一順序では候補段階で既に可視行のみに絞られるため構造的に満たす）
      2. **不可視行が順位・件数に影響しない（存在情報漏えいなし）**: 他テナントの
         private 行を含む不可視行の embedding・存在が、返却される可視行の順位・
         件数・有無を左右しない（上記統一順序はランキング候補に不可視行を
         含めないため構造的に満たす）
      2 条件は `crates/engine/tests/index_failure_injection.rs`（RECOVER-9）・
      `crates/engine/src/arena.rs` の構築時フィルタ検証群
      （`build_filtered_excludes_rows_failing_the_predicate_at_construction_time`
      等）と同種の「他テナントの不可視行が混在してもクエリ結果が縮まない・
      漏えいしない」ことを確認する回帰テストとして T3a・T5 に追加する
      （下記「実装タスク分解案」節の T3a・T5 行を参照）
    - **`slot_bits`／header 集計値によるスキップ最適化（T3b）は検査コストを
      削減する手段であり、安全性（不可視行を候補から除外する）の前提ではない**
      （上記「物理設計案」節・下記「5. チャンク単位可視性ビットマップ」節で
      規定した「除外方向は候補を減らす最適化にのみ作用し、admission の確定に
      使わない」原則の帰結）。上記統一順序でも、admission の可否は常に
      `user_rows` 側権威値に対する 4 点検証・`PolicyContext::is_visible` で
      決まり、`slot_bits` の有効・無効（T3a 既定 off／T3b 以降 on）は安全性の
      結論を変えない。T3b 有効化前後で変わるのはステップ 1 で列挙される候補の
      母数（＝ステップ 2 のポイントルックアップ数）のみである
    - **4 点検証の内容（fail-closed。codex-review P0 指摘・Bugbot High 指摘への
      対応）**: この経路（チャンク走査から `row_ids[slot]` を経由した点検索）は
      `layout: chunked`（v3）のテーブルに限って発生する（`layout: row` の
      テーブルは `user_vecs` を持たず本節の対象外。上記「物理設計案」節参照）
      ため、次の 4 点は常にすべて揃って検証できる:
      1. `row_ids[slot]` と取得した行の `id` が一致する
      2. チャンクキーの `tenant_id` と取得した行の `tenant_id` が一致する
      3. 取得した行の `locator(chunk_no, slot)` が走査元の `(chunk_no, slot)` と
         一致する
      4. `slot_bits` の visibility ビットと取得した行の visibility が一致する
      この検証は「ポイントルックアップに到達した候補」分のみに発生し
      （上記「統一順序」「コストへの帰結」のとおり、経路によらず
      `O(候補 live スロット数)`。チャンク走査そのものを O(N) から変えない）、
      本 ADR が狙う per-entry コスト削減の前提（下記「コスト見積り」節）と
      矛盾しない。誤って admit されたチャンク側複製上の private 行が上位 k 件
      から可視行を押し出す・`WHERE` 述語を通過して metadata を露出する形で
      結果集合を歪める可能性があるため、検証結果は「短い結果集合を黙って
      返す」のではなく fail-closed エラーとして伝播させる。ただし、この検証は
      ポイントルックアップに到達した候補についてのみ保証するものであり、
      「候補に到達する前」の `slot_bits`／header 集計値によるスキップ段階
      （T3b。ステップ 1 で候補を減らす方向にのみ作用する）はこの検証の対象外
      である（上記「除外方向」の限定的保証・下記「5. チャンク単位可視性
      ビットマップ」節参照）
- 行値（`user_rows`）の物理レイアウトは、下記「行単位レイアウトとの互換・移行方針」
  で採用する (2) opt-in 方式のもとでは **テーブルのカタログ `layout` 属性に従い
  2 形式が併存する**（codex-review P1 指摘への対応。一律に v3 化するとは規定しない）:
  - `layout: row`（既定・非 opt-in）: 現行の v2 のまま据え置く。embedding は
    引き続き行値に inline 格納され、既存のデコード経路（`decode_row_header`／
    `decode_row_embedding_and_metadata_into`）は無変更で動作する
  - `layout: chunked`（opt-in 済みテーブルのみ）: 行値から embedding を外し
    `[header | locator(chunk_no u64, slot u16) | metadata]` へ（行フォーマット v3）。
    v3 は `layout: chunked` のテーブルにのみ書き込まれ、`layout: row` のテーブルの
    行が v3 になることはない。この行→チャンクの locator は「行 id からチャンク
    位置を引く」順方向専用であり、上記 `row_ids` 配列（チャンク→行の逆方向）と
    役割が異なる。再パック（下記「削除（tombstone）」節）でスロットが移動した
    場合はチャンク側 `row_ids` の更新に加え、移動した行の `user_rows` locator も
    同一 txn 内で更新する（`row_ids` と `locator` の不一致を残さない）
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
  の live ビット集計）と一致することを確認する
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
- **チャンク単位・スロット単位のいずれも、`slot_bits`／header 集計値による判定は
  「候補を減らす」方向にのみ作用してよく、「候補を確定的に admit する」方向には
  使わない**（codex-review P0 指摘への対応。上記「物理設計案」節の単一権威
  原則を可視性ビットマップの運用へ適用したもの）。両者は性質が異なる fail-closed
  として区別する:
  - **デコード失敗**（本節冒頭の header 破損）: チャンク値自体が壊れて読めない
    ケース。「スキップしない＝即 `Err`」とする
  - **意味的不整合**（チャンク側複製と `user_rows` 側権威値の食い違い）: チャンク
    値は正常にデコードできるが、複製した visibility／`row_ids` が権威と一致しない
    ケース。デコードは成功するため本節の判定だけでは検出できず、上記「権威検証を
    行うタイミング」節の 4 点検証で検出・`Err` とする。ただしこの検出は
    **ポイントルックアップに到達した候補**（4 点検証節参照）にのみ保証される。
    本節のチャンク／スロットスキップ自体は、除外判定の対象になった行を
    ポイントルックアップへ進ませない（＝候補から外す）ため、この 4 点検証の
    対象から外れる。デコードは正常に成功したまま複製値が誤っている
    （bit-rot 等のコミット後破損）ケースで本来可視な行が誤って除外される
    事象は、クエリ経路の検証だけでは検出できない（codex-review P1 指摘。
    「不一致を黙って除外しない」契約は、あくまで admission 方向の
    fail-closed 保証であり、ポイントルックアップに到達する前の除外方向の
    最適化には及ばない。上記「物理設計案」節「admission 方向／除外方向」を
    参照）
  - **除外判定自体の整合性検証（T8。上記の残存リスクへの対応）**: 除外方向の
    正しさは書き込み時の同一 txn 不変条件（「4. 世代整合」節・T2）とコミット
    境界を跨ぐ障害注入検証（T5）でのみ担保され、コミット後の複製値のみの
    破損はクエリ経路では検出されない。この残存リスクをクエリの都度検出する
    ことはコスト見積りの前提（per-entry コスト削減）と両立しないため、
    帯域外（クエリのホットパス外）の**世界単位（`(tenant_id, chunk_no)` 単位）
    整合性検証ツール**を T8 として実装タスク分解案に追加する。T8 は各
    world について `live_count`／`public_live`／`private_live`／`slot_bits` を
    対応する `user_rows` エントリ集合の全件走査結果と突合し、不一致を
    operator が対処すべき fail-closed エラーとして報告する（`crash_tool` 系の
    運用者専用サブコマンドと同様の位置付け。クエリ応答経路には結線しない）。
    **T8 が実装されるまでは、`layout: chunked` テーブルのチャンク header／
    `slot_bits` によるスキップ最適化（本節・「4 点検証」節の対象を減らす方向の
    最適化）を有効化しない**。これにより「不一致を黙って除外しない」契約は、
    admission 方向（無条件）に加え除外方向（T8 による帯域外検証込みで運用時に
    担保）の双方について、最適化を有効化する前提条件として一貫させる

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
| `crates/engine/tests/power_loss.rs` | `StorageBackend` 差し替え電源断モデル | 部分書き戻し像でのチャンク値と locator の不整合検出（fail-closed 拒否）の確認。`row_ids[slot]` ⇔ `user_rows` 側 `locator` の相互不整合（再パック中の電源断を含む）に加え、`slot_bits` visibility ⇔ `user_rows` 側 tenant／visibility の不整合（権威検証。上記「権威検証を行うタイミング」節参照）も同一の fail-closed 検出対象に含める |
| `crates/engine/tests/index_failure_injection.rs`（RECOVER-9） | 再構築処理の途中失敗注入 | チャンク再パック処理の途中失敗で commit 済み行が無傷であることの確認。途中失敗後も `row_ids` と `locator` が旧配置のまま一貫していること（半端な書き換えが残らないこと）、および `slot_bits` visibility と行側 tenant／visibility の一致が崩れていないことを含める |

`recovery/commit_boundary.rs` の choke point 経由は維持する。

## コスト見積り

- **読み取り（フィルタなし経路。`WHERE`・宣言的フィルタ API を伴わない KNN・
  `SearchTimeFilter`・`batch_search`）（Bugbot High 指摘・codex-review P1 指摘
  への対応で見積りを改訂。上記「権威検証を行うタイミング」節参照）**: チャンク
  走査・ヘッダデコード段の per-entry コスト（≈225ns/行の走査＋ヘッダデコード分）
  は `1/N` 償却が引き続き見込める（25,000 行・N=256 では約 98 エントリまで
  削減される計算）。一方、権威検証の統一順序（ステップ 2）では、チャンク走査で
  候補になりうる全 live スロットに対し **header デコードのみ**のポイント
  ルックアップが発生するため（`slot_bits` スキップが T3b で有効化されるまでは
  概ね走査対象の可視候補全体）、`O(候補 live スロット数)` 項を持つ（metadata の
  デコードは発生しないため、下記「metadata 必須経路」の `O(RLS 可視行数)` 項
  より係数は小さいが、母数〔候補 live スロット数〕自体は縮小しない。「この
  経路のポイントルックアップは最終的な返却行数分のみに限られる」という以前の
  見積りは撤回する）。ランキング後に不足分を再取得する案は、非権威な複製値で
  ランキングしてから検証する構造ゆえ不可視行の embedding がランキング計算に
  混入し、再取得の有無・回数を通じて他テナントの存在情報が漏れうるため却下した
  （上記「権威検証を行うタイミング」節「却下した代替案」参照。コスト面の利点は
  不変条件〔存在情報漏えいなし〕を満たさない代償としては採用しない）。KNN の
  arena 構築段（≈468ns/行 の残り ≈243ns/行 = f32 デコード＋コピー）は連続
  コピー化により減少が見込まれる（定量は #362 の実測で確定させる。本 ADR では
  上限見積りとして提示するに留める）
- **読み取り（metadata 必須経路。`WHERE`・宣言的フィルタ API を伴うクエリ）**:
  上記の per-entry 削減はチャンク走査・ヘッダデコード段と embedding 分（f32
  デコード・コピー）にのみ適用され、metadata 分は適用されない。権威検証の
  統一順序（上記「権威検証を行うタイミング」節）に従い、ポイントルックアップ
  自体はフィルタなし経路と同じ母数（候補になりうる全 live スロット数）で発生
  するが、metadata のデコード（SCALAR 段評価に必要）はそのうち RLS 可視と
  判定された行数分だけ追加で発生する。読み取りコストのモデルは次の 3 項の和
  で表す: `O(N_scan / N) 償却分`（走査＋ヘッダデコード＋embedding デコード）・
  `O(候補 live スロット数)` の `user_rows` ポイントルックアップ（header デコード
  ＋4 点検証のみ）・`O(RLS 可視行数)` の metadata デコード（SCALAR 段評価込み）。
  フィルタなし経路との差は最後の metadata デコード項のみであり、
  「フィルタなし経路と metadata 必須経路のポイントルックアップ母数のオーダーが
  近づく」という以前の見積りは、権威検証の統一順序のもとでは両経路の母数が
  常に同一（`O(候補 live スロット数)`）であることの帰結として撤回・置換する
  （両者の差は候補母数のオーダーではなく、metadata デコード項の有無に限られる
  ことを明確にする）。RLS 可視行数が走査対象の大半を占めるワークロードでは、
  metadata デコード項が per-entry 削減効果を相殺しうる。T7 の効果実測では、
  この 2 経路（フィルタなし／metadata 必須）を分離して個別に測定し（測定条件:
  同一コーパス・同一 N に対し `WHERE` 句の有無のみを変えた A/B）、フィルタなし
  経路の改善幅と metadata 必須経路の改善幅（またはその欠如）を別々に報告する
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

(2) を採用する場合、(1) の「行フォーマット v3 へ一斉移行」は選択しない
（(1)／(2) は互いに排他的な選択肢であり、本 ADR は (2) のみを推奨する）。
`layout: row`（既定・非 opt-in）のテーブルは埋め込みを外した v3 形式へは
移行せず、v2（embedding inline）のまま恒久的に維持される。v3（行フォーマット）が
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
| T3a | 読み取り経路の結線（`VectorArena`・`SearchTimeFilter`・`batch_search` のチャンク走査＋権威検証〔「権威検証を行うタイミング」節の統一順序（ポイントルックアップ→権威判定→可視行のみ次段）・4 点検証。フィルタなし経路も同節の統一順序で実装する〕。集計は下記「T3a の対象範囲・集計の読み取り元」のとおりチャンク走査へ移さない。チャンク header／`slot_bits` によるスキップ最適化はコード上実装するが**既定無効**（フィーチャーフラグ／設定で off）とし、T3b（T8 完了後）まで有効化しない。検証には実際に書き込まれたチャンク〔T2〕と `layout: chunked` 選択・v2/v3 分岐〔T4〕の両方が必要なため依存に含める。**検証対象に「権威検証を行うタイミング」節の不変条件（短い結果集合を返さない・不可視行が順位/件数に影響しない）を追加**〔Bugbot High 指摘への対応。他テナントの不可視行・private 行を混在させたコーパスで、可視行が k 件以上あるとき返却が k 未満へ縮まらないこと、不可視行の有無で可視行の順位・件数が変化しないことを回帰テスト化する〕） | T1・T2・T4 | `arena.rs`・`rls.rs`・`batch_search.rs`・`sql/aggregate.rs`・`sql/group_by.rs` |
| T3b | T3a で実装したチャンク header／`slot_bits` によるスキップ最適化の有効化（既定 off → on への切り替え）。T8（帯域外整合性検証ツール）の完了を前提条件とする（下記「除外判定自体の整合性検証」節参照。**T8 未完了のまま有効化しない**） | T3a・T8 | `arena.rs`・`rls.rs`・`batch_search.rs`（フラグ切り替えのみ） |
| T4 | カタログのレイアウト属性と opt-in・オフライン再構築ツール | T1 | `catalog.rs`・`examples/` |
| T5 | 回復検証の拡張（crash_tool サブコマンド・cross-table 3 テーブル・power_loss・RECOVER-9 注入。「2. 削除（tombstone）」節「集計値の原子更新不変条件」の途中失敗検証を含む。**T3a と同じ「短い結果集合を返さない」「不可視行が順位/件数に影響しない」不変条件を、途中失敗・再構築後の状態でも保つことを RECOVER-9 注入の検証対象へ追加**〔Bugbot High 指摘への対応〕） | T2 | `scripts/`・`examples/crash_tool*.rs`・`tests/` |
| T6 | 再パック（コンパクション）と閾値 | T2 | `storage/chunk.rs`・`tenant.rs` |
| T7 | 効果実測（`feature_bench` 前後比較・`sql_c1_bench`・#362 内訳との突合）と README「実装方針（要点）」反映 | T3b・T4 | `examples/feature_bench.rs`・`docs/` |
| T8 | 世界単位（`(tenant_id, chunk_no)` 単位）整合性検証ツール（`live_count`／`public_live`／`private_live`／`slot_bits` と対応する `user_rows` 全件走査結果の突合。運用者専用・クエリ応答経路には結線しない。「5. チャンク単位可視性ビットマップ」節「除外判定自体の整合性検証」参照。「2. 削除（tombstone）」節「集計値の原子更新不変条件」の恒久的な帯域外担保を兼ねる）。**T3a のチャンク header／`slot_bits` によるスキップ最適化は本タスク完了まで有効化しない（有効化自体は T3b で行う）** | T2 | `examples/crash_tool*.rs` 系（新規サブコマンド）・`storage/chunk.rs` |

### T3a の対象範囲・集計の読み取り元（codex-review P1 指摘への対応）

提案するチャンク値（`row_ids`・embedding・`slot_bits`）には `SUM`／`AVG`／`MIN`／
`MAX`／`GROUP BY`／`HAVING` が要求するスカラー metadata が含まれない。metadata は
引き続き `user_rows` 側にのみ存在するため、これらの集計は **チャンク走査へは
移さず、`user_rows` を走査し続ける**（`sql/aggregate.rs`・`sql/group_by.rs` は
`user_rows` の `range`／`iter` 走査を維持し、チャンクテーブルには触れない）。
対象テーブルが `layout: chunked`（opt-in 済み）の場合、走査する `user_rows` は
embedding を外した縮小後の行フォーマット v3 であり、T3a における
`sql/aggregate.rs`・`sql/group_by.rs` への変更は「チャンク走査への切り替え」では
なく、v3（embedding を含まない縮小行）へ追随するデコード経路の更新に限定する。
`layout: row`（既定・非 opt-in）のテーブルでは行値は引き続き v2（embedding
inline）のままであり、集計の走査・デコード経路は本 ADR による変更を受けない。

この方針は「コスト見積り」節の副次効果（行値縮小により集計・`GROUP BY`・TASK-120
の path 一致走査が縮小後の `user_rows` を走査する前提でページ読み取り数が減る
見積り）と整合する。チャンクからポイントルックアップして `user_rows` を走査する
利点を失う設計（全チャンク→全行ポイントルックアップ）は採らない。

例外として `COUNT(*)`（`WHERE` を伴わず metadata 不要な場合に限る）は、チャンク
header の `live_count`／`public_live`／`private_live` 集計値を可視性判定に用いて
`user_rows` を一切走査せずに算出できる余地があるが、この最適化は本 ADR の対象外
（将来検討）とし、T3a では実装しない。`WHERE`・宣言的フィルタ API を伴う集計は
metadata 参照が必須のため、上記「権威検証を行うタイミング」節の統一順序と同じ
理由で `user_rows` 走査（縮小後）を維持する。**この将来最適化は「物理設計案」節・
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
| テナント境界（P0） | — | 可視性判定は `PolicyContext::is_visible` の単一照合パスのみを用いる。チャンクスキップは同 predicate の結果から導出し、独自テナント比較を新設しない。チャンクはテナント単位キー。header 破損は fail-closed（スキップしない）。エラーに他テナントの存在情報を含めない。**チャンク側 `slot_bits`／header 集計値は `user_rows` 側 tenant／visibility の派生複製であり単一権威ではない**（`user_rows` 側行ヘッダが権威）。単一権威原則は方向で保証範囲が異なる: **admission 方向は無条件**——複製は候補を減らす最適化にのみ使用し確定的な admit には使わず、ポイントルックアップに到達した候補は `WHERE` 経路・フィルタなし経路のいずれも距離計算・ランキングより前（`WHERE` 系はさらに metadata デコードより前）に権威検証（`row_ids[slot]`・tenant・`locator`・visibility の 4 点突合。`layout: chunked` に限って発生するため常に 4 点とも検証する）を行い、不一致は黙って除外・再判定せず `Err` とする（詳細な順序は「権威検証を行うタイミング」節参照）。**除外方向（`slot_bits`／header 集計値によるチャンク・スロットスキップ）は保証が限定的**——ポイントルックアップに到達する前の除外はこの 4 点検証の対象外であり、コミット後の複製値のみの破損による誤除外はクエリ経路では検出できない残存リスクとして明示する。この残存リスクは世界単位の帯域外整合性検証（T8）を前提として運用時に担保し、**T8 実装まで除外方向の最適化を有効化しない**（「権威検証を行うタイミング」節・「5. チャンク単位可視性ビットマップ」節参照）。**フィルタなし経路（`WHERE`・宣言的フィルタ API を伴わない KNN・`SearchTimeFilter`・`batch_search`）も、上記の統一順序（各 live スロットを先に `user_rows` へポイントルックアップして権威 tenant／visibility を取得し、その結果で RLS 判定を行ってから可視行のみを次段へ進める）でランキング前の候補段階に権威検証を行う（ランキング後に不足分を再取得する案は、非権威な複製値でランキングしてから検証する構造ゆえ存在情報漏えいの残存差分があり Rejected として却下済み）ことで「短い結果集合を返さない」「不可視行が順位・件数に影響しない」の 2 不変条件を満たすことを実装タスク（T3a・T5）の検証対象とする（Bugbot High 指摘・codex-review P0/P1 指摘への対応。「権威検証を行うタイミング」節参照） |
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
