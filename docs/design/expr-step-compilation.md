# 式評価のステップ列コンパイル化（Issue #353）

## 対象

`crates/engine/src/sql/udf_call.rs` の `eval()`（束縛済み式 `BoundExpr` の再帰
ツリーウォーク評価）は、`sql/exec.rs`（WHERE 事前/事後フィルタ・投影
`ProjectedColumn::Computed`）・`sql/aggregate.rs`（`AggregateInput::ScalarExpr`・
WHERE）・`sql/group_by.rs`（WHERE）の行ループから共通で呼ばれ、行ごとに再帰・
enum マッチの分岐分散が発生していた。本 Issue は PostgreSQL の `ExprState`
（ステップ配列化＋直接ディスパッチ）の発想に倣い、束縛時に一度だけ平坦な
ステップ列（`sql::expr_program::ExprProgram`）へコンパイルし、行ループでは
明示スタックによる線形実行へ置き換えた。LLVM JIT 等の依存追加は行わない
（dependency-policy）。

## 方式

- `sql::expr_program::ExprProgram::compile(&BoundExpr)`: `BoundExpr` 木を
  後行順（postorder）で `ExprStep` の平坦な列へ変換する。`BoundExpr::Binary`
  は lhs→rhs、`Builtin`/`WasmCall` の引数は左から右へ、いずれも無条件
  （短絡評価なし）に評価するため（`BoundExpr` に `And`/`Or` 相当の分岐評価
  ノードは存在しない）、平坦化は再帰 `eval` と同一の評価順・エラー発生順を
  厳密に保つ
- `ExprProgram::eval(id, embedding, scratch: &mut Vec<ExprValue>)`: 明示スタック
  （`scratch`）で線形実行する（再帰しない）。`scratch` は行ループの外で 1 回
  だけ確保し、行ごとに使い回す
- 定数畳み込み: コンパイル時にリテラルのみからなるスカラー算術・比較部分式を
  1 回だけ評価し `ConstScalar`/`ConstBool` へ置換する（`try_fold_scalar`）。
  畳み込み中にエラーになる部分式（例: 定数 0 除算）は畳み込まず、平坦化のみ
  行って実行時評価に委ねる（defer-on-error）。「可視行が 1 行も評価されなければ
  エラーは発生しない」という既存契約を、畳み込みの導入で変えないための判断
  （`sql_udf_call.rs::constant_subexpression_error_is_deferred_and_does_not_fail_a_query_with_no_visible_rows`／
  `constant_subexpression_error_still_fails_a_query_with_a_visible_row` で
  両方向を回帰テスト化）。`WasmCall` はバックエンドが非決定的でありうることと
  EXT-6 の行単位 deadline/中断契約を維持するため、畳み込み対象に含めない
- 値レベルの評価規則（0 除算・非有限値・`f32` キャスト後 Infinity の
  fail-closed 判定、`try_reserve_exact` による確保失敗時の `54000` 写像）は
  `udf_call::eval_binary`／`udf_call::apply_builtin`／`udf_call::finite_scalar`
  として 1 箇所に集約し、再帰 `eval`（参照実装として残置）と
  `ExprProgram::eval` の両方が共有する（複製しない）
- 公開 API 非破壊: `BoundStatement`／`BoundAggregate` の既存フィールド
  `expr_filters: Vec<BoundExpr>`（EXPLAIN・テストの可観測性用）は残置し、
  新設した `expr_filter_programs: Vec<ExprProgram>`（`pub(crate)`）を実行経路
  が見る。`AggregateInput::ScalarExpr` はタプル variant から
  `{ source: BoundExpr, program: ExprProgram }` 構造体 variant へ変更した
  （クレート内 `pub(crate)` enum のため後方互換の制約なし）。`pub` enum
  `ProjectedColumn::Computed` は形状を変えず、`sql::exec::project_rows` の
  入口（行ループの外）で `Computed` 列だけを 1 回コンパイルする

## 行ループからの撤去範囲

以下の 6 箇所すべてで `udf_call::eval`（再帰）の呼び出しを
`ExprProgram::eval`（線形実行）へ置換した:

- `sql/exec.rs`: `on_visible_row` の WHERE 事前フィルタ、DISTANCE 事後フィルタ、
  投影 `ProjectedColumn::Computed`
- `sql/aggregate.rs`: WHERE 行フィルタ、`AggregateInput::ScalarExpr` の観測
- `sql/group_by.rs`: WHERE 行フィルタ

`udf_call::eval` は行ループの production 経路からは撤去し、セマンティクスの
参照実装（`ExprProgram::compile`/`eval` の差分テストの基準）としてのみ残した。

## 検証

- `cargo test -p engine`: `sql_udf_call`／`sql_surface`／`sql_aggregate`／
  `sql_group_by`／`sql_evaluation_order`／`wasm_udf_contract`／
  `tenant_isolation`／`tenant_breach` を含む既存テストは無修正で green
  （評価結果・エラー契約が不変であることの確認）
- `sql::expr_program` の新規ユニットテスト（12 件）: 定数畳み込み・
  defer-on-error・`IdRef`/`VectorRef` の非畳み込み・`Builtin`/`WasmCall`（スタブ）・
  比較演算・ステップ数が `MAX_EXPR_NODES` を超えないことの境界検証を含み、
  代表的な式木について再帰 `eval` との差分一致を確認する
- `cargo fmt --all -- --check`／`cargo clippy --workspace --all-targets
  --all-features -- -D warnings`
- 機械確認: `grep -rn "udf_call::eval\b" crates/engine/src/sql/` で
  production 行ループに再帰呼び出しが残っていないことを確認済み
  （コメント・`sql::expr_program` 内の参照実装呼び出しのみが残る）

## ベンチ前後比較（受け入れ条件 3）

計画が名指した `feature_bench` 相当の汎用ベンチ例は本リポの
`crates/engine/examples/` に存在せず（`concurrent_write_bench`・`crash_tool`・
`crash_tool_cross_table`・`high_dim_bench`・`multi_dim_bench` のみ）、SQL 表層の
既存ベンチ（`benches/sql_c1_bench.rs`）は spec 由来の非公開閾値環境変数
（`BENCH_SQL_C1_MAX_P95_MS`／`BENCH_SQL_C1_MIN_RECALL`）が前提で、自動運転の本
実装セッションからは値を持たず実行できない。そのため、行ループの分岐機構
そのもの（再帰ツリーウォーク vs 平坦ステップ列の線形実行）を同一プロセス内
（同一ビルド・同一マシン）で直接比較する手動専用ベンチを
`sql::expr_program::tests::bench_recursive_eval_vs_compiled_program`
（`#[ignore]`）として追加した。両実装は本 PR で並存する（`udf_call::eval` は
参照実装として残置）ため、この比較は「Issue #353 が変えた部分」の前後差を
そのまま表す。

実行方法:

```sh
cargo test -p engine --release --lib \
  sql::expr_program::tests::bench_recursive_eval_vs_compiled_program \
  -- --ignored --nocapture
```

対象式は WHERE 述語の典型形に近い `vec_norm(embedding) > 2.0`（16 次元ベクトル、
行依存の組み込み関数呼び出し中心。定数畳み込みは効かない構成で、行ごとの
評価コストそのものを測る）。同一環境（本開発環境）で 3 回実測:

| 実行 | recursive（再帰 `eval`、20 万回） | compiled（`ExprProgram::eval`、20 万回） | 差分 |
| ---- | -------------------------------- | ---------------------------------------- | ---- |
| 1 | 9.268 ms | 8.964 ms | -3.3% |
| 2 | 9.321 ms | 8.898 ms | -4.5% |
| 3 | 9.239 ms | 8.924 ms | -3.4% |
| 4 | 9.391 ms | 8.902 ms | -5.2% |

再帰版比で概ね 3〜5% の短縮を安定して観測した。改善幅は小さいが、これは
対象式（`Builtin` 1 段＋`Binary` 1 段）自体が浅く、組み込み関数本体
（`vec_norm` の L2 ノルム計算）が支配的コストであるため妥当な範囲と考えられる
（WHERE 句がより深いネスト・複数の式述語を持つクエリほど、行ごとの再帰呼び出し
削減・分岐分散の解消による相対的な寄与が大きくなると見込まれる）。受け入れ
条件は「前後比較の記録」であり数値ゲートではないため、実測値をそのまま
記録する。専有環境での SQL 表層 C1/WHERE 経路の再測定（`bench-c1` 等）は
オーナー／管理者作業として引き続き別途の実測対象とする。

## 対象外（out-of-scope-tracking）

- `USING PLAN` のソフトブーストヒント（`path_hint`/`kind_hint`）の
  `sql/exec.rs` 結線は本 Issue の対象外（既存の申し送り事項を変更しない）
- `sql_c1_bench.rs` 等の専有環境ベンチによる本変更の実測反映は、閾値
  secrets を保持するオーナー／管理者作業として申し送る
