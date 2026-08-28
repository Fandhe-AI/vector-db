# C1（純粋 Top-k）p95 専有環境再測定

- ステータス: Proposed（本 PR のマージ後、オーナーが専有環境で再実行し結果を記録した
  うえで別コミットで Accepted に更新する）
- 対応: TASK-83（Issue #60。ポインタ: `docs/spec/05-tasks.md`・Conditional Go 条件7）
- 前提: TASK-75（SQL 表層 SELECT の束縛・実行。Issue #56 / PR #184 でマージ済み）・
  TASK-158（性能計測プロトコル基盤。Issue #106 でマージ済み）
- 関連: TASK-127（provider 単体の性能・Recall 受け入れ基準回帰。`simd_bench.rs`）・
  TASK-144（Issue #121。条件6 の前例となる再測定 ADR）・TASK-165（wire 経由での
  end-to-end 再測定。本タスクの範囲外）
- 対象ビヘイビア: なし（基盤タスク。成果物は専有環境再測定レポートとゲート整備）

## 背景

本 PR は TASK-83（Conditional Go 条件7。ポインタ: `docs/spec/05-tasks.md`）に対する
実装として、SQL 表層 C1（純粋 Top-k）の p95 再測定ハーネス・合否ゲート・専有環境での
再実行手順を整備する。タスクの定義本文・数値基準・過去の PoC 実測値は spec が SSOT で
あり、本ドキュメントへは転記しない（`.claude/rules/spec-confidentiality.md`。ポインタ
表記に留める運用は `docs/design/concurrent-write-verification.md`〔TASK-144・条件6〕の
前例に倣う）。

TASK-158（性能計測プロトコル基盤 `crates/engine/benches/harness/`）の契約
（warmup 20 回以上・計測 20 回以上・中央値＋Q1/Q3・決定的シード RNG・interleaved A/B）
に従って計測する。

既存 `benches/simd_bench.rs`（TASK-127）は `SearchProvider` を直接叩く provider
単体の p95 であり、`EngineCore::execute_sql`（SQL 表層。`sql::exec::execute_statement`）
経由の C1 p95 は本タスクまで未計測だった。SQL 表層は毎クエリ
`VectorArena::build_filtered_with_rows_in_txn` により候補行を redb から再デコードする
（`core::EngineCore::search` が使う `PrefilterCache`〔TASK-169〕は SQL 経路では使われ
ない）ため、p95 の支配項が provider 側ではなく SQL 実行計画側にある可能性がある。
ロードマップ（MS-3）も基準未達時の切り分けを求めており、本タスクでは SQL 表層 vs
`EngineCore::search`（`VectorCore` trait 経由）の interleaved A/B を診断情報として
同時に取得する。

本タスクの測定対象は engine の内部実行器（SQL 表層 `EngineCore::execute_sql`）とし、
wire プロトコル経路（TASK-73・WIRE-1 の簡易クエリで `execute_sql` へ接続済み）の
往復コストは含めない。条件7 が対象とするのは SQL 表層自体の p95 であり、wire 経由の
end-to-end 再測定は TASK-165 の管轄とする。

## 検証設計

| 項目 | 内容 |
| ---- | ---- |
| 測定入口 | `crates/engine/benches/sql_c1_bench.rs`（`cargo bench --bench sql_c1_bench -p engine` / `make bench-c1`） |
| 測定対象 | `EngineCore::execute_sql(ctx, "SELECT id FROM documents ORDER BY embedding <=> '[...]' LIMIT 20")`（SQL-1 の規範形。`harness::sql_c1` が構造検証つきで組み立てる） |
| データ | 100,000 行 × 768 次元・k=20（本ベンチが選んだ計測構成。既存 `simd_bench.rs` の行数・次元に揃えて比較可能にしたもので、spec の計測条件の転記ではない。決定的シード RNG で生成し、`tenant::insert_rows` で 10,000 行単位のチャンク投入） |
| プロトコル | `harness::protocol::run`（warmup 20・計測 50・決定的シード）→ `harness::accept::p95_from_samples` / `check_p95_within_limit`。Recall@20 は `CpuScalarProvider`（厳密最近傍の独立オラクル）との Top-20 一致率を 20 クエリの worst-query で判定 |
| 診断 A/B | `harness::ab::run_ab` で SQL 表層 vs `EngineCore::search` を interleaved 計測し `median_ratio` を出力（**合否には含めない**。SQL 表層オーバーヘッドの切り分け材料。ただし `EngineCore::search` 側は `PrefilterCache`〔TASK-169〕を経由するため、テーブル内容が変わらない本ベンチでは初回以降キャッシュがウォームな状態で計測される——`median_ratio` は「SQL 表層のコールドな arena 再構築」と「キャッシュヒット時の `EngineCore::search`」の比であり、両経路の候補デコード実装そのものを対称条件で比較した値ではない点に注意） |
| 閾値注入 | `BENCH_SQL_C1_MAX_P95_MS`・`BENCH_SQL_C1_MIN_RECALL`（SQL-1 専用の repo variables。`simd_bench.rs` の `BENCH_MAX_P95_MS`／`BENCH_MIN_RECALL` は provider 単体 CORE-3・SEARCH-4・CORE-4 の基準であり出所が異なるため流用しない）。未設定・不正値は fail-closed で非ゼロ終了する。標準出力は既定では pass/fail・非数値状態のみを記録する（閾値・実測値そのものは出力しない）。実測値は `BENCH_SQL_C1_VERBOSE=1`（GitHub Actions 外の opt-in）に限り出力する（Issue #278。`contrast_bench.rs`・CORE-5 対応〔PR #224〕と同一方針） |
| 専有環境の宣言 | `BENCH_DEDICATED_ENV=1` の opt-in フラグ。未設定（既定）では「専有環境として宣言されていない」ことを明示し、条件7 の判定対象から除外する（p95/Recall 自体の pass/fail は常に出力） |
| CI 対象 | `tests/c1_bench_accept.rs`（`make ci` 対象）が SQL 文字列生成・識別子検証・往復性・環境記録の非 panic のみを時間非依存に検証する。時間依存の実測（`bench-c1`）は `.github/workflows/bench.yml` の `workflow_dispatch` 限定ジョブから実行し、schedule には含めない |

## 実測環境

本 PR の実測は、複数 Issue の worktree が並列稼働しうる**共有仮想化環境**（QEMU 仮想
CPU・KVM）上で行った。`sql_c1_bench.rs` が出力する `EnvReport`（OS/arch・論理コア数・
検出 ISA・`/proc/loadavg`）で環境条件を記録できる構造にしたが、本 PR のコミット時点
では 100,000 行 × 768 次元のフル規模実測は実行していない（下記「実測結果」参照）。

| 項目 | 値 |
| ---- | -- |
| OS/arch | linux/x86_64（`std::env::consts`） |
| 検出 ISA | AVX2+FMA（`engine::isa::current().isa()`。AVX-512 非対応） |
| 論理コア数 | 12（`std::thread::available_parallelism()`。他 Issue の worktree と共有） |
| 仮想化 | QEMU 仮想 CPU・KVM |
| spec の計測環境前提 | 本ホストとは非同一（他プロセスと CPU/IO を共有しない専有環境） |

## 実測結果

**本 PR のコミット時点では、100,000 行 × 768 次元のフル規模実測（`make bench-c1`）は
実行していない。** 理由: 本ホストは複数 Issue の worktree が並列稼働する共有仮想化
環境であり、専有環境を前提とする条件7 の判定材料としては最初から成立しない値になる
うえ、フル規模の実測（seeding + p95 50 回計測 + Recall 20 クエリ + interleaved A/B）は
所要時間・環境負荷の観点で本タスクの主眼（再測定ハーネス・ゲート・再実行手順の整備）
に見合わない。

代わりに、以下 2 点を本 PR 内で検証した:

1. **fail-closed の動作確認**: `BENCH_SQL_C1_MAX_P95_MS`／`BENCH_SQL_C1_MIN_RECALL`
   未設定・`BENCH_SQL_C1_MAX_P95_MS=0`・`BENCH_SQL_C1_MIN_RECALL=1.5` のいずれでも、
   データ投入前に非ゼロ終了することを確認した。
2. **計測経路の疎通確認**: 行数・次元・k を大幅に縮小した構成（200 行・8 次元・k=5）
   で `make bench-c1` 相当のバイナリを実行し、以下が正しく動作することを確認した
   （検証後、本コミットの内容は元の本番定数（100,000 行・768 次元・k=20）へ戻して
   ある）。
   - データ投入（`tenant::insert_rows` のチャンク投入）
   - SQL 表層 C1 の p95 計測・出力
   - `CpuScalarProvider` 参照実装との Recall@k（`recall_min=1.000000`）
   - SQL 表層 vs `EngineCore::search` の interleaved A/B 診断出力
   - `conditional_go_7` の「未評価」表示・`BENCH_DEDICATED_ENV=1` 時の「宣言済み」表示

この疎通確認により、seeding のテナント境界付き API 経由投入・SQL 文字列生成・
`EngineCore::execute_sql` 呼び出し・判定ヘルパの結線に実装上の欠陥がないことを
確認済みである。フル規模での実測値そのものは未取得のため、下表は専有環境での
再実行時に埋めるテンプレートとして扱う。

> [!NOTE]
> `sql_c1_bench.rs` は既定で実測値を public ログへ出力しない（Issue #278。
> 「専有環境での再実行手順」参照）。下表は「実施済み/未実施」「pass/fail」の
> **非数値状態**のみを記録する運用へ改めた。実測値の数値そのもの（p95・
> `recall_min`・`median_ratio`）は非公開記録先へ保存し、本ドキュメント（public
> 資産）へは転記しない（`docs/design/tier-latency-acceptance.md` と同運用）。

| 指標 | 実施状態 | pass/fail |
| ---- | -------- | --------- |
| p95（SQL 表層 C1、rows=100,000 dim=768 k=20） | 未実施 | — |
| Recall@20（vs `CpuScalarProvider`） | 未実施 | — |
| A/B `median_ratio`（SQL 表層 / `EngineCore::search`。診断情報・合否対象外） | 未実施 | 対象外 |

## Conditional Go 条件7 の判定: 未充足（判定保留）

理由:

- 専有環境が宣言されていない（`BENCH_DEDICATED_ENV=1` を設定した実測は行っていない）
- 本ホストは spec の計測環境前提（専有環境）と非同一の共有仮想化環境である
- フル規模の実測値そのものを本 PR では取得していない（上記「実測結果」参照）

したがって、条件7 は本 PR では**確定せず**、オーナーが専有環境で再実行したうえで
本ドキュメントを Accepted に更新する運用とする（TASK-144 の ADR と同じ流儀）。

## 専有環境での再実行手順

1. 他プロセスと CPU/IO を共有しない専有環境（ベアメタル、または他ジョブが同居しない
   専有 VM）を用意する。
2. spec（`docs/spec/04-behavior/sql-surface.md` SQL-1）の p95 上限・Recall 下限を
   コマンドラインの環境変数としてのみ渡す（値をファイル・コミット・PR 本文・本
   ドキュメントに書かない）:

   ```bash
   BENCH_SQL_C1_MAX_P95_MS=<spec 値> \
   BENCH_SQL_C1_MIN_RECALL=<spec 値> \
   BENCH_DEDICATED_ENV=1 \
   BENCH_SQL_C1_VERBOSE=1 \
   make bench-c1
   ```

   `BENCH_SQL_C1_VERBOSE=1` は実測値を標準出力へ含める opt-in（Issue #278）。
   **GitHub Actions 外の環境でのみ**設定すること。GitHub Actions 下（`GITHUB_ACTIONS`
   環境変数設定時）では verbose 要求を fail-closed で拒否し、データ投入前に非ゼロ
   終了する。
3. 実行前後に `cat /proc/loadavg`・`nproc`・`lscpu | grep 'Model name'` を控え、
   run-to-run 変動を確認するため 2 回以上実行する。
4. 標準出力の `p95_latency(sql_c1)`・`topk_consistency(sql_c1_vs_scalar_exhaustive)`・
   `diagnostic_ab(sql_surface_vs_core_search)`・`conditional_go_7` の各行の
   「実施済み」「pass/fail」を上表へ転記する（p95・`recall_min`・`median_ratio`・
   閾値の数値そのものは本ドキュメント〔public 資産〕へは転記せず、非公開記録先へ
   保存する。Issue #278）。
5. `pass=true` かつ `dedicated_env=attested` であれば、本ドキュメントのステータスを
   Accepted に更新し、条件7 を「充足」へ書き換える。基準未達の場合は「未充足
   （実測・専有環境）」として観察（A/B 比から読める切り分け所見）を追記する。

## 制約・スコープ外

- 専有環境での本実測はオーナー作業（本 PR には含まれない）
- wire 経由の end-to-end 再測定は TASK-165 の管轄
- SQL 経路への `PrefilterCache`（TASK-169）適用等の性能改善は本 PR のスコープ外
- `bench.yml` の `bench-c1` を週次 schedule へ含めるかの判断は、専有環境での実測後に
  オーナーが行う
- spec 側のステータス更新（Conditional Go 条件7 の Accepted 反映）・GitHub repo
  variables の設定はオーナー作業

## 参照

- `docs/spec/05-tasks.md` TASK-83（ポインタ）
- `docs/spec/04-behavior/sql-surface.md` SQL-1（ポインタ）
- Issue #278（実測値 public ログ出力の抑止）
- `crates/engine/benches/sql_c1_bench.rs`・`crates/engine/benches/harness/sql_c1.rs`・
  `crates/engine/benches/harness/env_report.rs`
- `crates/engine/tests/c1_bench_accept.rs`
- `docs/design/rrf-tie-break-determinism.md`（同型の再測定・判断記録 ADR の前例）
