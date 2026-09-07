# 可視ビットマップキャッシュの非漏えい・前後比較検証（Issue #479）

## 背景

Issue #478 で `SELECT COUNT(*)` 等（`GROUP BY` なし・`WHERE` なし・
`DecodeTier::Fast`）の集計経路へ可視行スナップショットの世代整合キャッシュ
（`sql::visible_cache::VisibleBitmapCache`。詳細は
`docs/design/visible-bitmap-cache.md` 参照）が入った。ヒット経路は
`user_rows/{table}` を一切開かず RLS 可視判定・TABLE-12 検査を省略して
`visible_ids` を反復するだけになるため、本ドキュメントは以下 2 点を検証専任で
固定し、実測値・実測できなかった範囲を記録する。

1. **テナント非漏えいの固定（対照 DB 方式）**: cold／hot いずれの経路でも、
   集計結果が「不可視行が物理的に存在しない DB」の結果と完全一致すること
2. **前後比較の記録**: 本キャッシュ導入による性能効果の実測

production コード（`crates/engine/src/`）は本 Issue の範囲では変更しない
（テスト・ベンチ・docs のみ）。

## 検証設計

### 対照 DB（オラクル）方式の非漏えいテスト

`crates/engine/tests/visible_cache_oracle.rs`。既存
`tests/sql_visible_cache.rs::cache_key_never_leaks_across_tenants` は
`COUNT(*)` の件数のみ・単一シナリオで非漏えいを確認しているのに対し、本
ファイルは以下を対照 DB（「その文脈で不可視な行が物理的に存在しない DB」）
との結果一致で機械検証する。

- `COUNT(*)`／`COUNT(id)`／`SUM(id)`／`AVG(id)`／`MIN(id)`／`MAX(id)` の単項
  6 種と、`SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs`
  という複合形（他 DB 横断ベンチの `agg_multi` と同形）
- **cold**（新規 `EngineCore`・真のミス経路）・**hot**（同一 `EngineCore` の
  2 回目・真のヒット経路）双方
- 単一 `EngineCore` を複数文脈（tenant-a Public のみ・tenant-a Public+Private・
  tenant-b Public のみ）で交互に実行し、他文脈のエントリが常駐した状態でも
  漏れないこと
- 対象テーブルへの書き込みによる失効（`stale_evictions` 増加）後も、無関係な
  tenant-a の新規 Private 行が tenant-b の視点に混入しないこと
- セッション経由（wire の入口。`EngineCore::execute_sql_in_session`）でも同様に
  一致すること。wire セッションは常に Public のみ可視で運用されるため、
  ここでは「Private を可視とする文脈」相当の経路は対象外（wire には存在しない）

**非 vacuous ガード**: cold 呼び出しは `visible_bitmap_cache_stats().misses`
が +1 することを、hot 呼び出しは `hits` が +1 することをそれぞれ確認して
から対照 DB と比較する（`tests/sql_visible_cache.rs::
id_aggregates_match_between_cold_and_hot_cache` の codex-review 指摘対応と
同方針）。キャッシュキーが `(table, PolicyContext)` のみで集計式を含まない
ため、同一 `EngineCore` で複数の SQL 文を検証するケース
（`session_entrypoint_matches_oracle_db`）は SQL ごとに新規 `EngineCore` を
使い、2 文目以降が「既存キャッシュへの意図しないヒット」を「真のミス」と
誤認しないようにしている。

**`AVG(id)` の比較が近似ではなく完全一致で成立する根拠**:
`sql::aggregate::Accumulator::IdAvg` は `u64` の厳密な総和を保持し、
`finish()` で 1 回だけ `sum as f64 / count as f64` を計算する（途中を `f64`
で逐次加算しない）ため、可視 `id` を観測する順序に関わらず得られる `f64` は
ビット一致する。対照 DB 側でも同じ可視 `id` 集合を観測するため、本ファイルの
比較は `assert_eq!` の完全一致で行う（順序依存の近似比較は不要）。

### 既存ゲートとの関係

`tests/sql_visible_cache.rs`・`tests/sql_aggregate.rs`・
`tests/rls_generalized.rs`（TASK-138）・`tests/sparse_cache_recall.rs`・
`tests/scan_stage_profile_accept.rs` は無変更のまま green を確認済み。

### e2e cold 変種 A0c（`crates/engine/benches/scan_stage_profile_bench.rs`）

既存の `A0a`（`agg_count`・ctx=tenant-a）・`A0b`（`rls_isolation`・
ctx=tenant-b）は同一 `EngineCore` を使い回すため、2 回目以降は必ず本
キャッシュのヒット経路を測る。`A0c` は `W0c`（既存の cold 変種）と同じ流儀で
毎サンプル新規 `Storage::open` + `EngineCore`（空キャッシュ）から
`COUNT(*)` を実行することで、ミス経路（走査に相乗りしたスナップショット
構築＋`Storage::open` を含む）の対照値を追加した。`make
bench-scan-stage-profile` で `A0a`／`A0c-cold` を出力・整合性検証すること
（COUNT 値が期待値と一致）を確認済み。

## 実測値

### `bench-scan-stage-profile`（engine 内 e2e、本開発環境の 1 回実測）

`BENCH_SCAN_PROFILE_ROUNDS=5 BENCH_SCAN_PROFILE_SCALE=1`（25,000 行・
dim=128・tenant_a=23,000 Public・tenant_b=2,000 Private）。

| 測定点 | median | 備考 |
| ------ | -----: | ---- |
| `agg_count`（A0a, ctx=tenant-a, ヒット経路） | 0.050ms | 同一 `EngineCore` 使い回し |
| `rls_isolation`（A0b, ctx=tenant-b, ヒット経路） | 0.050ms | 同上 |
| `agg_count`（A0c-cold, ctx=tenant-a, ミス経路） | 7.336ms（min-of-5=7.228ms） | 毎サンプル新規 `Storage::open`＋`EngineCore` |

`A0a`／`A0b` の値は `docs/design/visible-bitmap-cache.md`「前後比較実測」節の
導入後実測（0.050ms）と同一環境・同一設定での再実測であり整合する。
`A0c-cold` は本 Issue で新規追加した測定点で、ヒット経路（0.050ms）との比が
約 147 倍——同キャッシュのヒット時に A1〜A5 全段（redb 全行走査・ヘッダ
デコード・RLS 判定・dim/metadata デコード・スカラー検証）と `Storage::open`
そのものが省略されることを裏づける。

`docs/design/visible-bitmap-cache.md` の導入前実測（`agg_count`
1.598ms・`rls_isolation` 1.625ms。導入前は毎回全行走査だが `Storage::open`
は含まない）と `A0c-cold`（7.336ms・`Storage::open` を含む）は測定条件
（`Storage::open` の有無）が異なるため単純な比率換算はできない。`A0c` は
今回追加した新規測定点であり、導入前バイナリでの `A0c` 実測（下記「実測
できなかった範囲」参照）と揃えて初めて公平な前後比較になる。

### crossdb self（wire 経由・`agg_count`／`rls_isolation`）の before/after 前後比較

**実測できなかった範囲**: 本開発環境は複数の並列 worktree（他 Issue の
実装・レビューエージェント）が同時に稼働しており、`/tmp`（tmpfs 16GiB）の
空き容量不足（`Disk quota exceeded (os error 122)`）により、before
コミット（`2ca1536`）を別 `CARGO_TARGET_DIR` でビルドする過程で失敗した
（`git worktree add` 自体は成功、`cargo bench --no-run` が `syn`/`ash`/
`redb` のコンパイル中に失敗）。したがって以下は**未実測**のまま記録する。

- `bench-scan-stage-profile` の `A0c-cold` を before（`2ca1536`。
  `VisibleBitmapCache` 導入前）バイナリでも実行し、after（本ブランチ）と
  交互 min-of-N（N≥5）で比較する
- `scripts/crossdb_bench/run.py --db self --config exact`（`agg_count`／
  `rls_isolation`。dim=128・25,000 行）を `CROSSDB_SELF_BINARY` で
  before/after 2 バイナリを交互起動し、`docs/design/
  benchmark-judgement-policy.md` の規約（交互 N≥5 ペア・per-run 生データ・
  min-of-N＋median 併記・参照区間との比較）で判定する

再現手順（本コミットで追加した seam を使う。専有環境での実測を運用者へ
申し送る）:

```bash
# before/after を別 CARGO_TARGET_DIR でビルド（十分な空きディスクが必要）
git worktree add /path/to/wt-before 2ca1536
cp crates/engine/benches/scan_stage_profile_bench.rs \
   /path/to/wt-before/crates/engine/benches/scan_stage_profile_bench.rs
( cd /path/to/wt-before && \
  CARGO_TARGET_DIR=/path/to/target-before cargo build --release -p wire-server )
cargo build --release -p wire-server   # after（本ブランチ）

# 25,000 行・dim=128 の fixture を用意する（after 側ビルドを流用）
cargo run --release -p engine --example seed_docs -- \
  seed /path/to/docs25k.redb 25000 128
cargo run --release -p engine --example seed_docs -- \
  queries 128 200 /path/to/queries200.jsonl

# before/after を交互に N=5 ペア実行する（例。実際は自動化スクリプトで N 回ループする）
CROSSDB_SELF_BINARY=/path/to/wt-before/target-before/release/wire-server \
  python scripts/crossdb_bench/run.py --db self --config exact \
    --rows-file /path/to/docs25k.redb --queries-file /path/to/queries200.jsonl \
    --out-dir /path/to/results/pair1/before --expect-dim 128
python scripts/crossdb_bench/run.py --db self --config exact \
    --rows-file /path/to/docs25k.redb --queries-file /path/to/queries200.jsonl \
    --out-dir /path/to/results/pair1/after --expect-dim 128
```

## 環境記録（実測を行った範囲）

- CPU: 本開発環境（既存 docs と同一。`lscpu`/ISA 検出はベンチ出力の
  `env: os=linux arch=x86_64 logical_cpus=12 isa=Avx2Fma` を参照）
- `BENCH_DEDICATED_ENV` 未設定（共有環境）。ベンチ実行時の `loadavg` は
  `6.81 6.91 5.79`（他 worktree のビルド・テストと共存）
- `git rev-parse HEAD`（本ブランチ、A0c 実測時点）: 本コミット群の 2 番目
  （`test(engine): scan_stage_profile_bench へ e2e COUNT(*) の cold 変種
  A0c を追加`）
- before コミット: `2ca1536`（`929c027`＝Issue #478 マージコミットの親）

## 所見

- 対照 DB 方式のテストにより、cold／hot／複数文脈の交互ホット状態／失効後／
  wire セッション経由のいずれでも他テナントの可視性境界を跨いだ混入がない
  ことを機械的に固定した。
- `A0c-cold`（ミス経路）とヒット経路（`A0a`/`A0b`）の比（約 147 倍）は、
  `docs/design/visible-bitmap-cache.md` が既に報告した「約 32 倍高速化」
  （導入前 vs 導入後のヒット経路）を補強する新しい観測点だが、before
  バイナリでの `A0c` 実測が取れていないため、導入前後の直接比較値としては
  未確定のまま申し送る。
- crossdb self（wire 経由）の before/after 実測は、本開発環境の共有 `/tmp`
  容量制約により実施できなかった。上記の再現手順を用いた専有環境での実測は
  オーナー／運用者作業として引き続き申し送る。

## スコープ外（申し送り）

- 専有環境（`BENCH_DEDICATED_ENV=1`）での交互 min-of-N（N≥5）実測——
  `bench-scan-stage-profile` の `A0c` before/after 比較・crossdb self の
  `agg_count`／`rls_isolation` before/after 比較のいずれも未実施
  （`docs/design/visible-bitmap-cache.md` の既存の申し送りを維持）
- `DimAndScalar`/`Embedding` tier・`GROUP BY`・SELECT DISTANCE/hybrid 経路への
  拡張（Issue #478 のスコープ外判断を継承）
- `feature_bench`／`bench-*` への交互ペア自動集計の組み込み
  （`docs/design/benchmark-judgement-policy.md` §9 の別 Issue 候補）
