# ADR: `agg_count`／`rls_isolation`／`vector_knn_where` の段別プロファイル

- ステータス: Accepted
- 対応 Issue: #464（親 #456・ルート #455）
- 関連ポインタ: TASK-83（SQL 表層性能受け入れ基準）・TASK-158（性能計測プロトコル基盤）・SQL-13（集計関数）・Issue #350（集計経路のデコードスキップ）・`docs/design/crossdb-bench.md`・`docs/design/knn-stage-profile.md`（Issue #362 の先行事例）・`docs/design/knn-wire-stage-profile.md`（Issue #463 の先行事例）・`docs/design/benchmark-judgement-policy.md`（計測規約）

## 背景

`docs/design/crossdb-bench.md`（25,000 行・dim 128・wire 経由・p50）で self が最も劣後する 3 フェーズ——`agg_count` 3,547µs（sqlite-vec 616µs）・`rls_isolation` 3,552µs（620µs）・`vector_knn_where` 2,819µs（Qdrant 615µs）——は、Issue #350 で集計経路が embedding デコードを既に省いているため、残るコストが「redb 全行走査」「ヘッダデコード」「RLS 可視判定（`PolicyContext::is_visible`＋TABLE-12 キー/ヘッダ整合検査）」「dim/metadata デコードと構造検証」「`WHERE` 述語評価（`scan_scalar_columns`＋`matches_all`）」「フィルタ後 arena 複製」「距離計算・Top-k」のどこに帰属するか実測内訳が無かった。本 Issue は 3 フェーズを段別に分解して実測し、Phase 2 の #477（可視ビットマップ世代整合キャッシュ）・#471（スカラー列二次索引）が対象にする段を数値で特定する。

**production コード（`crates/engine/src/`）は無変更**。テスト・ベンチ・docs 専任タスク。

## 測定対象の実行経路

### `agg_count`／`rls_isolation`（`SELECT COUNT(*) FROM docs`）

`core.rs::execute_sql` → `Statement::Aggregate` → `sql/aggregate.rs::execute_aggregate`。`SqlArenaCache` は経由せず毎クエリ redb を全行走査する。行ループの順序:

1. `table.iter()` の per-entry 走査
2. `storage::decode_row_header`（tenant/visibility/offset）
3. `ctx.is_visible(tenant_id, visibility)`（不可視なら continue）→ `storage::verify_row_key_tenant`（TABLE-12）
4. `DecodeTier::Fast`（`COUNT(*)` はスカラー列・VECTOR 列非参照）でも `storage::decode_row_dim_and_metadata_borrowed`（構造検証・ヒープ確保なし）
5. `row_codec::validate_scalar_columns(schema, metadata)`

`storage::decode_row_header`／`decode_row_dim_and_metadata_borrowed`／`verify_row_key_tenant` はいずれも `pub(crate)` でベンチから直接呼べないため、`benches/harness/scan_stage_profile.rs` へ行フォーマットの再実装を置く（ドリフト対策は後述）。

crossdb の可視性モデル（`scripts/crossdb_bench/`）: tenant-a 23,000 行 Public・tenant-b 2,000 行 Private。wire セッションは Public のみ可視のため、`agg_count`（tenant-a 接続）と `rls_isolation`（tenant-b 接続）は**同一の `COUNT(*)`＝23,000 を返す同一走査**になる（実測 3,547µs/3,552µs の近さと整合）。

### `vector_knn_where`（`SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '<vec>' LIMIT 10`）

`sql/exec.rs::execute_statement_with_cache`。`WHERE` があるため `cache_fast_path_eligible = false`。`SqlArenaCache` ヒット時は全可視行に対し `on_visible_row`（`row_codec::scan_scalar_columns` で全列を借用スキャン → `matches_all`。`SELECT id` のため投影列の複製は発生しない）を呼び、一致行の embedding のみを新規 arena へ複製したうえで `provider.search`（距離＋Top-k）を呼ぶ。crossdb で特定済みの「k 非依存の約 10ms 固定コスト」（Issue #453）と同じ `scan_scalar_columns` 全行走査がここに乗る。

## 段の定義

### A 系列（`agg_count`／`rls_isolation`。入れ子: A1 ⊆ A2 ⊆ A3 ⊆ A4 ⊆ A5。分母は総物理行数）

| 段 | 内容 |
| --- | --- |
| A1 `redb_scan` | 生 `redb::Database` 再オープンで `user_rows/docs` を per-entry 走査するのみ |
| A2 `header_decode` | A1 ＋ ヘッダデコード（`harness::knn_profile::decode_header_reimpl` を再利用） |
| A3 `rls_visible` | A2 ＋ `PolicyContext::is_visible`（不可視行は以降スキップ）＋ TABLE-12 キー/ヘッダ tenant 整合検査（`verify_row_key_tenant_reimpl`） |
| A4 `dim_meta_decode` | A3 ＋ dim・metadata 借用デコード（`decode_dim_and_metadata_reimpl`。可視行のみ） |
| A5 `scalar_validate` | A4 ＋ `row_codec::validate_scalar_columns`（可視行のみ） |

`A0a`（e2e `COUNT(*)`・ctx=tenant-a）・`A0b`（同・ctx=tenant-b）を e2e 上限として別途測定し、両者の可視行数が毎ラウンド一致することを fail-closed で検証する。

### W 系列（`vector_knn_where`。入れ子: W1 ⊆ W2 ⊆ W3。分母は可視（tenant-a Public）行数）

| 段 | 内容 |
| --- | --- |
| W1 `scalar_scan` | 可視行の metadata へ `row_codec::scan_scalar_columns` |
| W2 `predicate` | W1 ＋ `lang = 'ja'` 判定（`declarative_filter::matches_all`） |
| W3 `arena_copy` | W2 一致行の embedding を連続 `Vec<f32>` へ複製 |
| W4 `provider_search` | 一致行のみへ `ParallelSearchProvider::search`（k=10） |

`W0c`（e2e cold・`SqlArenaCache` を毎サンプル空の状態から測る）・`W0h`（e2e hot）・`W0n`（`WHERE` なしの同形 KNN。cache fast path）を e2e として測定し、`W0h − W0n` を「SQL 表層内の `WHERE` 上乗せ」として報告する。`R_dot`（全可視行への逐次内積総和。Top-k なし）を参照区間（変更を含まない区間）として用い、複数ラウンド中央値の `(max-min)/min` をノイズ帯判定の実測帯とする（`docs/design/benchmark-judgement-policy.md` §4）。

## 測定設計

- **コーパス**: crossdb モデル（tenant-a 23,000×scale 行 Public・tenant-b 2,000×scale 行 Private・dim 128・`lang` 列 5 値輪番〔`ja` ≒ 20%〕）。`feature_bench.rs` とは列構成が異なる（`topic`／`body` を持たない簡略版）ことを明記する。`BENCH_SCAN_PROFILE_SCALE`（既定 1＝25,000 行、4＝100,000 行。`bench_engine::parse_scale` で fail-closed）で規模を切替え、1 プロセス = 1 規模点とする（`docs/design/benchmark-judgement-policy.md` §5・Issue #313 の教訓）。
- **ラウンド輪番**: `BENCH_SCAN_PROFILE_ROUNDS`（既定 5・5〜50・fail-closed パース）。各ラウンドで A1〜A5・W1〜W4・R_dot を輪番で 1 回ずつ計測し（各段内部は `harness::protocol::run` の warmup 20 回・計測 20 回で中央値を得る）、ラウンド横断で min-of-R と median-of-R を併記する。per-round 生値も出力する。
- **再実装によるドリフト対策**: `pub(crate)` の `decode_row_header`／`decode_row_dim_and_metadata_borrowed`／`verify_row_key_tenant` はベンチから直接呼べないため、`benches/harness/scan_stage_profile.rs::decode_dim_and_metadata_reimpl`（f32 変換を行わず dim・metadata の構造検証のみ）を追加した。`tests/scan_stage_profile_accept.rs` が `Storage::put`/`Storage::scan()`（pub API・正本）との突き合わせでドリフトを検出する（`knn_profile_bench.rs::decode_row_reimpl` と同方式）。
- **CI 配線**: しない。spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線せず、`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する。`make bench-scan-stage-profile` から手動実行する。

## fail-closed 整合性検証（不成立なら非ゼロ終了・測定値非出力）

- A3 の可視行数が全ラウンドで `tenant_a_rows` と一致し、かつ ctx_a／ctx_b で同一であること（`agg_count`／`rls_isolation` が同一走査であることの直接確認）
- W2 の一致件数が、計測外で独立に求めた `lang = 'ja'` の可視行数と一致すること
- W0-cold／W0-hot の返却行数が `TOP_K`（10）であり、返却 id がすべて tenant-a・`lang='ja'` の可視集合に含まれること（テナント境界・フィルタ境界の非漏えい確認）
- `COUNT(*)` の値が ctx_a／ctx_b いずれも `tenant_a_rows` と一致すること
- `GITHUB_ACTIONS` 設定時は起動直後に拒否

## 実測結果

計測環境: 共有 QEMU 環境（`docs/design/benchmark-judgement-policy.md` §5 の区分に従い**参考値**。専有環境での再実測は運用者作業）。`lscpu` Model name: `QEMU Virtual CPU version 2.5+`・`nproc`=12・`isa=Avx2Fma`・計測時 `loadavg` ≈ 1.8〜2.5・`BENCH_DEDICATED_ENV` 未設定・計測コミット: `b827a2f09842c914667511df700d4deb031cfc96`（`origin/main`）。ラウンド数: 既定 5。

### 25,000 行（`BENCH_SCAN_PROFILE_SCALE=1`。tenant-a 23,000・tenant-b 2,000）

per-round 生値（ms）:

| round | A1 | A2 | A3 | A4 | A5 | W1 | W2 | W3 | W4 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 0.962 | 1.098 | 1.235 | 1.223 | 1.298 | 0.317 | 0.414 | 0.489 | 0.127 |
| 1 | 0.964 | 1.093 | 1.232 | 1.222 | 1.301 | 0.320 | 0.415 | 0.491 | 0.113 |
| 2 | 0.963 | 1.094 | 1.236 | 1.237 | 1.307 | 0.319 | 0.417 | 0.489 | 0.121 |
| 3 | 0.961 | 1.116 | 1.239 | 1.224 | 1.311 | 0.320 | 0.415 | 0.491 | 0.092 |
| 4 | 0.963 | 1.097 | 1.239 | 1.235 | 1.296 | 0.320 | 0.415 | 0.489 | 0.124 |

median-of-R（ms・min-of-R）・ns/row・段差分:

| 段 | median | min-of-R | ns/row | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| A1 redb_scan | 0.963 | 0.961 | 38.5 | — | — | — | — |
| A2 header_decode | 1.097 | 1.093 | 43.9 | A1→A2 | 5.4 | 14.0% | within |
| A3 rls_visible | 1.236 | 1.232 | 49.4 | A2→A3 | 5.6 | 12.7% | within |
| A4 dim_meta_decode | 1.224 | 1.222 | 49.0 | A3→A4 | n/a（逆転） | — | — |
| A5 scalar_validate | 1.301 | 1.296 | 52.0 | A4→A5 | 3.1 | 6.2% | within |

e2e: `A0a`（agg_count）median=1.581ms・`A0b`（rls_isolation）median=1.581ms（両者一致）。

| 段 | median | min-of-R | ns/row（分母=可視 23,000 行） | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| W1 scalar_scan | 0.320 | 0.317 | 13.9 | — | — | — | — |
| W2 predicate | 0.415 | 0.414 | 18.1 | W1→W2 | 4.2 | 30.0% | above |
| W3 arena_copy | 0.489 | 0.489 | 21.3 | W2→W3 | 3.2 | 17.8% | above |
| W4 provider_search（一致 4,600 行） | 0.121 | 0.092 | — | — | — | — | — |
| R_dot（参照区間） | 0.198 | 0.195 | — | — | — | reference_band=17.0% | — |

e2e: `W0-cold`=11.709ms・`W0-hot`=1.226ms・`W0-nowhere`=0.628ms。`W0-hot − W0-nowhere`（SQL 表層内の `WHERE` 上乗せ）= 0.598ms。

### 100,000 行（`BENCH_SCAN_PROFILE_SCALE=4`。tenant-a 92,000・tenant-b 8,000）

per-round 生値（ms）:

| round | A1 | A2 | A3 | A4 | A5 | W1 | W2 | W3 | W4 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 3.995 | 5.259 | 5.568 | 5.713 | 5.842 | 1.293 | 1.689 | 2.908 | 0.378 |
| 1 | 3.991 | 4.999 | 5.603 | 5.667 | 5.863 | 1.290 | 1.695 | 2.891 | 0.245 |
| 2 | 4.002 | 5.009 | 5.616 | 5.729 | 5.960 | 1.293 | 1.688 | 2.893 | 0.243 |
| 3 | 4.026 | 5.002 | 5.633 | 5.689 | 5.905 | 1.290 | 1.690 | 2.922 | 0.250 |
| 4 | 3.996 | 5.005 | 5.580 | 5.715 | 5.890 | 1.292 | 1.689 | 2.911 | 0.235 |

median-of-R（ms・min-of-R）・ns/row・段差分:

| 段 | median | min-of-R | ns/row | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| A1 redb_scan | 3.996 | 3.991 | 40.0 | — | — | — | — |
| A2 header_decode | 5.005 | 4.999 | 50.0 | A1→A2 | 10.1 | 25.2% | above |
| A3 rls_visible | 5.603 | 5.568 | 56.0 | A2→A3 | 6.0 | 12.0% | above |
| A4 dim_meta_decode | 5.713 | 5.667 | 57.1 | A3→A4 | 1.1 | 2.0% | within |
| A5 scalar_validate | 5.890 | 5.842 | 58.9 | A4→A5 | 1.8 | 3.1% | within |

e2e: `A0a`（agg_count）median=7.265ms・`A0b`（rls_isolation）median=7.169ms。

| 段 | median | min-of-R | ns/row（分母=可視 92,000 行） | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| W1 scalar_scan | 1.292 | 1.290 | 14.0 | — | — | — | — |
| W2 predicate | 1.689 | 1.688 | 18.4 | W1→W2 | 4.3 | 30.8% | above |
| W3 arena_copy | 2.908 | 2.891 | 31.6 | W2→W3 | 13.3 | 72.2% | above |
| W4 provider_search（一致 18,400 行） | 0.245 | 0.235 | — | — | — | — | — |
| R_dot（参照区間） | 1.698 | 1.681 | — | — | — | reference_band=1.11% | — |

e2e: `W0-cold`=74.308ms・`W0-hot`=6.508ms・`W0-nowhere`=3.256ms。`W0-hot − W0-nowhere` = 3.253ms。

### 実測からの所見

- A 系列: `A1`（走査のみ）が総コストの 3〜4 割を占め、`A2`（ヘッダデコード）・`A3`（RLS 判定）が次点。`A4`（dim/metadata デコード）・`A5`（スカラー構造検証）の追加コストは相対的に小さい（100k 点で A3→A5 合計は A1→A3 合計の半分未満）。`agg_count`／`rls_isolation` の e2e（A0a/A0b）は両 ctx でほぼ同値（差 1%未満）——同一走査であるという設計時の仮説（§背景）を実測で確認した。
- W 系列: `W3`（arena_copy）が規模とともに支配的になる（25k 点で W2 比 +17.8%、100k 点で W2 比 +72.2%）。一致行数（`lang='ja'` ≈ 20%）に比例して複製コストが伸びるため、規模が大きいほど `W3` の相対寄与が増す。`W0-hot − W0-nowhere`（SQL 表層内の `WHERE` 上乗せ全体）は 25k で 0.598ms・100k で 3.253ms であり、W1+W2+W3 の合計（25k: 0.489ms・100k: 2.908ms〔累積値としては W3 の median と同じ〕）に近い値で、残差はパース・束縛・キャッシュ照会等の付随コストと解釈できる。
- `W0-cold`（`SqlArenaCache` を毎回空の状態から測る）は `W0-hot` の約 9〜11 倍——`SqlArenaCache`（Issue #363）のヒット有無がクエリ毎の redb 再デコードコストを大きく左右することを示す（`vector_knn_where` の crossdb 実測がどちらの状態に近いかは、crossdb ハーネスの接続再利用方針に依存するため本 Issue の対象外）。

## #477（可視ビットマップ世代整合キャッシュ）・#471（スカラー列二次索引）への帰属

| 段 | #477 が省き得るか | #471 が省き得るか |
| --- | --- | --- |
| A1 redb 走査 | 可（同一世代内は走査自体を省略し得る） | 不可（`COUNT(*)` 無 `WHERE` は索引対象外） |
| A2 ヘッダデコード／A3 RLS 判定・TABLE-12 検査 | 可（構築時に全件実施し以降省略。fail-closed 維持が条件） | 不可 |
| A4 dim/meta 借用デコード／A5 構造検証 | 可（同上） | 不可 |
| W1 `scan_scalar_columns` 全行／W2 述語 | 不可 | 可（等値述語を索引経路へ） |
| W3 一致行 arena 複製 | 不可 | 一部（候補集合縮小で複製量は減るが複製自体は残る） |
| W4 距離＋Top-k／R_dot | 不可（参照区間） | 不可（参照区間） |

**所見**: `agg_count`／`rls_isolation` は `WHERE` を持たず `on_visible_row` を通らないため、#471 が `agg_count` を対象に挙げている前提は本経路には当てはまらない（`where_compound_count` には当てはまる）。両フェーズの改善は #477 側の段（A1〜A5、実測では特に A1〜A3）に帰属する。`vector_knn_where` は `W0-hot − W0-nowhere` の上乗せのうち W1＋W2 の比率で #471 の効果上限を、`W0-cold − W0-hot` で `SqlArenaCache` の寄与を示す。

## `feature_bench` との差異

- `feature_bench`: tenant-a 20,000 行・tenant-b 5,000 行（10% Private）・`lang`/`topic`/`body` 3 スカラー列・`SELECT *` 系クエリ中心。
- 本ベンチ: tenant-a 23,000×scale 行 Public・tenant-b 2,000×scale 行 Private（crossdb モデルに合わせた比率）・`lang` 列のみ・`SELECT id` 系クエリ（crossdb ハーネスの投影と揃える）。

## スコープ外・申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測はオーナー作業。本 doc の数値は共有 QEMU 環境の参考値。
- 再実装デコーダ（`decode_dim_and_metadata_reimpl`）と正本のドリフトが将来問題化した場合の `bench-internals` 限定 src フック追加は別 Issue 判断（本 Issue は production 無変更を優先）。
- crossdb self（wire 経由・psycopg）との残差の定量帰属は #463 と同様に運用者作業。
- `feature_bench` の 13 フェーズ自体への段別計測点の埋め込みは行わない（`feature_bench` は 13 フェーズ横断の e2e 基線として不変に保つ）。
- `where_compound_count`・`group_by_having` の段別分解は本 Issue 対象外（#471 側で必要なら別途）。
