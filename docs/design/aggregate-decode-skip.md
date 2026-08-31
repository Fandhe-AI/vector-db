# ADR: 集計経路の embedding 非参照デコードスキップと必要列限定デコード（Issue #350）

- ステータス: Accepted
- 対応: Issue #350
- 関連ポインタ: `docs/spec/05-tasks.md`（TASK-166・TASK-167）・
  `docs/spec/04-behavior/sql-surface.md`（SQL-13・SQL-14）・
  `docs/spec/04-behavior/rls.md`（RLS-7・RLS-8）・`docs/spec/04-behavior/data-model.md`
  （TABLE-12）。spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 変更コード: `crates/engine/src/row_codec.rs`・`crates/engine/src/storage.rs`・
  `crates/engine/src/sql/udf_call.rs`・`crates/engine/src/sql/aggregate.rs`・
  `crates/engine/src/sql/group_by.rs`

## 背景

集計実行経路（`sql::aggregate::execute_aggregate`・`sql::group_by::execute_grouped_aggregate`）
は、可視行に対して常に embedding の完全デコード（`Vec<f32>` 確保）とスキーマ全スカラー
列のパース（毎行 `Vec<Option<&str>>` 確保）を行っていた。`COUNT(*)`・`COUNT(col)`・
`SUM(id)` のように embedding や大半のスカラー列を参照しないクエリでも、行数に比例した
不要なデコード・確保コストが発生していた。

## 設計

### 参照列集合の導出

`sql::aggregate::ReferencedColumns::derive` が、束縛済み集計文
（`items`・`metadata_filters`・`expr_filters`・`GROUP BY` キー列）から

- `scalar_mask`（`schema.columns` と同じ長さのマスク。`row_codec::scan_scalar_columns_masked`
  へそのまま渡す）
- `needs_embedding`（`ScalarExpr` 項目・`WHERE` 式述語が embedding へ到達するか）
- `needs_vector_presence`（`COUNT(<VECTOR 列>)` があるか。embedding 自体は不要で
  dim のみ必要）

を一度だけ導出する。embedding へのアクセスは束縛段で `BoundExpr::VectorRef` に
一元化されているため（`sql::udf_call::eval` 参照）、`udf_call::references_embedding`
による式木の再帰走査（`Builtin`/`Binary`/`WasmCall` は引数へ再帰、非網羅 `match` 禁止）
は過小評価が起きない精密判定になる。

### 3 段階デコード（`DecodeTier`）

可視行 1 件あたりのデコードを、クエリが実際に要求する範囲まで縮小する。

| tier | 内容 | 使用関数 |
| ---- | ---- | -------- |
| `Fast` | ヘッダ（tenant・visibility）＋ TABLE-12 のキー/ヘッダ tenant 整合検査＋ dim・embedding 境界・metadata 境界の構造検証（`Vec<f32>`／`&str` 化は省略） | `storage::decode_row_tenant_and_visibility`・`storage::verify_row_key_tenant`・`storage::decode_row_dim_and_metadata_borrowed` |
| `DimAndScalar` | `Fast` と同じ構造検証に加え、マスク済み metadata 列の `&str` 化まで行う | `storage::decode_row_dim_and_metadata_borrowed`・`row_codec::scan_scalar_columns_masked` |
| `Embedding` | embedding を含む完全デコード | `storage::decode_row_embedding_and_metadata_into`（スクラッチ `Vec<f32>` 再利用） |

`Fast` と `DimAndScalar` はどちらもヒープ確保を伴わない
`storage::decode_row_dim_and_metadata_borrowed`（dim・embedding 境界・metadata 境界・
末尾余剰バイトの構造検証。破損は `XX000`）を通る。両者の違いは metadata 列の
presence・長さ・UTF-8 の構造検証を、`Fast` は結果を一切保持しない
`row_codec::validate_scalar_columns`（`Vec` 確保なし）で行い、`DimAndScalar` は
`&str` 化した結果を `Vec<Option<&str>>` へ保持する `row_codec::scan_scalar_columns_masked`
で行う点のみで、破損検知（fail-closed 契約）に差は無い（両者は走査本体
`row_codec::scan_scalar_columns_validated` を共有し、検証済みの値を `Vec` へ積むか
捨てるかだけを呼び出し元が選ぶ。PR #369 codex-review P1 指摘対応・2 件: (1) `Fast` が
この構造検証自体を丸ごと省略し、破損した可視行を `COUNT(*)`/`SUM(id)` 等の集計結果へ
黙って含めてしまう契約変更になっていた旧実装を修正。(2) その後も `Fast` が
`scan_scalar_columns_masked` を呼び毎行 `Vec<Option<&str>>` を確保しており、`Fast` は
結果 `Vec` 生成を省略するという本 tier 契約に反していた実装バグを修正）。

`Fast` は `sql::aggregate::execute_aggregate`（`GROUP BY` なし）専用で、
`needs_embedding`・`needs_vector_presence`・`scalar_mask` がすべて偽、かつ
`metadata_filters`・`expr_filters` がいずれも空の場合のみ選択する（`COUNT(*)`・
`COUNT(id)`・`SUM`/`AVG`/`MIN`/`MAX(id)` の組み合わせ）。`GROUP BY`
（`sql::group_by::execute_grouped_aggregate`）はグループキー列を必ず読む必要があるため
`Fast` を選ばず、`needs_embedding` の有無で `DimAndScalar`／`Embedding` の 2 段階のみを
切り替える。

可視性判定（ヘッダのみ・デコード前）と TABLE-12 のキー/ヘッダ tenant 整合検査は、
どの tier でも必ず・同じ順序で行う。この 2 つはデコード「範囲」の縮小と独立した
不変条件であり、`ReferencedColumns`・`DecodeTier` はここへ一切手を加えない。

### `scan_scalar_columns_masked` / `validate_scalar_columns`（必要列限定デコード）

`row_codec::scan_scalar_columns_masked(schema, buf, mask)` と
`row_codec::validate_scalar_columns(schema, buf)` は、内部の走査本体
`row_codec::scan_scalar_columns_validated` を共有する。`mask[i] == false` の列も
presence・宣言長（`MAX_TEXT_FIELD_LEN`）・バッファ境界の構造検証に加え UTF-8 妥当性検証
まで常に行い（codex-review P1 指摘・PR #369: 未参照列でも不正 UTF-8 を含む永続行を
fail-closed で拒否する既存のエラー契約（`XX000`）を維持する必要があるため）、
`scan_scalar_columns_masked` は検証済みの値のうち要求列だけを `&str` として
`Vec<Option<&str>>` へ積む一方、`validate_scalar_columns` は検証のみ行い値を一切
保持・返却しない（呼び出し元は `Vec` を確保しない）。untrusted な行バッファに
対する構造検証・内容検証は、マスク外の列・`validate_scalar_columns` 経由のいずれでも
一切弱めない。既存 `scan_scalar_columns` は全列 `true` の薄いラッパーへ変更した
（呼び出し元・テストは無変更で green）。

`DecodeTier::Fast`（`scalar_mask` が全 `false`。`metadata_filters` も必ず空。
`MetadataFilter` は自身の列を必ずマスクへ反映するため）は `validate_scalar_columns`
を使い、走査そのもの（presence・列長・UTF-8・余剰バイトの構造検証）は省略しない一方、
`Vec<Option<&str>>` の確保は行わない（codex-review P1 指摘・PR #369・2 件。(1)
走査自体を省略すると破損した可視行を検出できないまま集計が成功してしまう
fail-open な契約変更になるため走査は必須。(2) 走査を `scan_scalar_columns_masked`
で行うと `Fast` でも毎行 `Vec` を確保してしまい「`Fast` は結果 `Vec` 生成を省略する」
という本 tier 契約に反するため、検証専用 API を新設して置き換えた）。`DimAndScalar`
は引き続き `scan_scalar_columns_masked` を使い、全 `false` マスクで呼び出しても
構造検証・UTF-8 妥当性検証は他マスクと同様に全列へ行われ、省略されるのは検証済み
`&str` の生成・保持だけである。

## 契約変更点（PR #369 codex-review P1 対応で撤回）

初版実装では `DecodeTier::Fast` 到達クエリが可視行の embedding/metadata セクションの
構造破損・次元不一致（従来 `XX000` で query 失敗）を検出しなくなっており、意図的な
契約変更として記述していた。しかし codex-review（PR #369）の指摘により、この契約
変更が spec 側の対応する定義変更を伴わないまま行われていた点が P1 として指摘され、
`Fast` にも `decode_row_dim_and_metadata_borrowed`（ヒープ確保を伴わない構造検証）を
必ず通す実装へ修正した。これにより従来の `XX000` fail-closed 契約は `Fast` を含む
全 tier で維持され、契約変更は生じていない（`Fast`/`DimAndScalar`/`Embedding` の差は
純粋にデコード「範囲」の縮小のみで、検証水準の差ではない）。RLS 可視性判定・
TABLE-12 のキー/ヘッダ tenant 整合検査は元々全 tier で維持していた。この挙動は
`sql::aggregate::tests` の固定テスト（後述）で明示的に固定する。

## `redb::Table::len()` への縮退は不採用

`len()` は不可視行を含む全件数を返すため、「RLS 可視集合＝テーブル全行」が静的に
証明できる場合に限り正しいが、現行の `PolicyContext`（tenant 別可視性・private/public）
にはその証明を engine 側で完結できる状態がなく、適用条件の誤りが即 fail-open（他
テナント行数の漏えい・RLS-7/RLS-8 違反）になる。`DecodeTier::Fast` で行数比例コストの
主要因（embedding 確保・全列パース）は既に除去できているため、縮退は採らない。
tenant プレフィクスの range scan 化等の更なる最適化はスコープ外（オーナー確認後に
別 Issue として起票候補）。

## 固定したテスト

- `crates/engine/src/row_codec.rs`: `scan_scalar_columns_masked` のマスク挙動（マスク
  外列でも構造検証・UTF-8 妥当性検証は維持し `&str` 化・保持のみ省略・不正 UTF-8 は
  マスク内外を問わず `Err`・マスク長不一致は `Err`）
- `crates/engine/src/storage.rs`: `decode_row_dim_and_metadata_borrowed` の完全デコード
  との一致・`decode_row` と同じ破損検知・`verify_row_key_tenant` の受理/拒否
- `crates/engine/src/sql/udf_call.rs`: `references_embedding` の直接参照・`Builtin`/
  `Binary` 経由の間接参照・非参照式
- `crates/engine/src/sql/aggregate.rs`:
  `count_star_fastpath_still_fails_closed_on_corrupted_embedding_section`
  （`DecodeTier::Fast` が embedding セクションの破損を fail-closed（`XX000`）で
  検出することを実証。PR #369 codex-review P1 対応の固定）・
  `count_vector_column_on_same_corrupted_row_still_fails_closed`（`VECTOR` 列参照経路
  〔`DecodeTier::DimAndScalar`〕も同じ破損を検出し、`Fast` との検証水準の差が無いことの
  対照テスト）
- 既存の結合テスト（`crates/engine/tests/sql_aggregate.rs`・`sql_group_by.rs`）は
  無変更のまま green（`WHERE`・`vec_norm` を含む式・`GROUP BY` 等の end-to-end oracle
  一致を維持）

## 性能実測について（申し送り）

計画段階では既存の `feature_bench.rs`（`SELECT COUNT(*)` の計測フェーズを含む想定）を
前後比較に使う想定だったが、実装時点で本リポジトリに同名のベンチは存在しなかった
（`crates/engine/benches/` には C1（検索）・GPU バッチ等の既存ベンチはあるが、集計経路
専用のベンチは無い）。本 Issue のスコープでは新規ベンチ整備を行わず、代わりに
`DecodeTier::Fast` が実際にデコードを行わないことを上記の破損注入テストで構造的に
実証する方針とした。行数比例の実測（レイテンシ・スループット差）が必要な場合は、
別途ベンチ整備をオーナー確認のうえ Issue 化する。
