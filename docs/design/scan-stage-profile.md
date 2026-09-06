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

`W0c`（e2e cold・`SqlArenaCache` を毎サンプル空の状態から測る）・`W0h`（e2e hot）・`W0n`（`WHERE` なしの同形 KNN。cache fast path）を e2e として測定する。`R_dot`（全可視行への逐次内積総和。Top-k なし）を参照区間（変更を含まない区間）として用い、複数ラウンド中央値の `(max-min)/min` をノイズ帯判定の実測帯とする（`docs/design/benchmark-judgement-policy.md` §4）。`W0h` と `W0n` は dense 探索の候補集合サイズが異なる（`W0n` は可視行全体、`W0h` は事前フィルタ後の一致行のみ）ため、`W0h − W0n` を「SQL 表層内の `WHERE` 上乗せ」として単純に報告することはできない（詳細は下記「W0-hot と W0-nowhere の候補集合差」節）。候補集合を揃えた `WHERE` 上乗せの内訳は W1〜W3（一致行のみを対象とする一貫した集合）で測る。

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

計測環境: 共有 QEMU 環境（`docs/design/benchmark-judgement-policy.md` §5 の区分に従い**参考値**。専有環境での再実測は運用者作業）。`lscpu` Model name: `QEMU Virtual CPU version 2.5+`・`nproc`=12・`isa=Avx2Fma`・計測時 `loadavg` ≈ 1.6〜2.9・`BENCH_DEDICATED_ENV` 未設定。ラウンド数: 既定 5。

**再測定の経緯**: 初回実測（本節旧版）は codex-review／Cursor Bugbot 指摘（PR #555・P1）により、`A4`／`A5` が `A3` と同じキー/ヘッダ tenant 整合検査（`verify_row_key_tenant_reimpl`）を省いており累積段契約（A3 ⊆ A4 ⊆ A5）が成立していなかったことが判明したため、`A4`／`A5` へ同検査を追加したうえで再測定した（初回実測で `A3→A4` の diff が `n/a（逆転）` になっていたのはこの欠落が原因——整合性検査を省くぶん `A4` が `A3` より速く見えていた）。下記は修正後の実測値。

### 25,000 行（`BENCH_SCAN_PROFILE_SCALE=1`。tenant-a 23,000・tenant-b 2,000）

per-round 生値（ms）:

| round | A1 | A2 | A3 | A4 | A5 | W1 | W2 | W3 | W4 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 0.971 | 1.098 | 1.235 | 1.377 | 1.449 | 0.316 | 0.410 | 0.486 | 0.095 |
| 1 | 0.962 | 1.098 | 1.238 | 1.374 | 1.443 | 0.317 | 0.408 | 0.488 | 0.095 |
| 2 | 0.970 | 1.163 | 1.333 | 1.453 | 1.517 | 0.322 | 0.419 | 0.526 | 0.095 |
| 3 | 1.002 | 1.097 | 1.233 | 1.374 | 1.446 | 0.317 | 0.411 | 0.491 | 0.093 |
| 4 | 1.002 | 1.115 | 1.235 | 1.381 | 1.448 | 0.318 | 0.409 | 0.490 | 0.093 |

median-of-R（ms・min-of-R）・ns/row・段差分:

| 段 | median | min-of-R | ns/row | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| A1 redb_scan | 0.971 | 0.962 | 38.9 | — | — | — | — |
| A2 header_decode | 1.098 | 1.097 | 43.9 | A1→A2 | 5.1 | 13.1% | within |
| A3 rls_visible | 1.235 | 1.233 | 49.4 | A2→A3 | 5.5 | 12.4% | within |
| A4 dim_meta_decode | 1.377 | 1.374 | 55.1 | A3→A4 | 5.7 | 11.5% | within |
| A5 scalar_validate | 1.448 | 1.443 | 57.9 | A4→A5 | 2.9 | 5.2% | within |

e2e: `A0a`（agg_count）median=1.580ms・`A0b`（rls_isolation）median=1.577ms（両者一致）。

| 段 | median | min-of-R | ns/row（分母=可視 23,000 行） | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| W1 scalar_scan | 0.317 | 0.316 | 13.8 | — | — | — | — |
| W2 predicate | 0.410 | 0.408 | 17.8 | W1→W2 | 4.0 | 29.2% | above |
| W3 arena_copy | 0.490 | 0.486 | 21.3 | W2→W3 | 3.5 | 19.4% | above |
| W4 provider_search（一致 4,600 行） | 0.095 | 0.093 | — | — | — | — | — |
| R_dot（参照区間） | 0.231 | 0.205 | — | — | — | reference_band=13.5% | — |

e2e: `W0-cold`=11.918ms・`W0-hot`=1.118ms・`W0-nowhere`=0.597ms。raw diff（`W0-hot − W0-nowhere`）= 0.521ms（下記「W0-hot と W0-nowhere の候補集合差」節の注意を参照——このままでは「SQL 表層内の `WHERE` 上乗せ」として単純には解釈できない）。

### 100,000 行（`BENCH_SCAN_PROFILE_SCALE=4`。tenant-a 92,000・tenant-b 8,000）

per-round 生値（ms）:

| round | A1 | A2 | A3 | A4 | A5 | W1 | W2 | W3 | W4 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 4.209 | 4.895 | 5.745 | 6.230 | 6.477 | 1.278 | 1.810 | 3.263 | 0.359 |
| 1 | 3.976 | 5.023 | 5.629 | 6.226 | 6.728 | 1.272 | 1.801 | 3.247 | 0.340 |
| 2 | 3.975 | 5.066 | 5.658 | 6.223 | 6.470 | 1.274 | 1.809 | 3.325 | 0.360 |
| 3 | 3.985 | 5.006 | 5.661 | 6.223 | 6.446 | 1.274 | 1.808 | 3.229 | 0.233 |
| 4 | 3.977 | 4.984 | 5.621 | 6.261 | 6.491 | 1.271 | 1.812 | 3.233 | 0.234 |

median-of-R（ms・min-of-R）・ns/row・段差分:

| 段 | median | min-of-R | ns/row | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| A1 redb_scan | 3.977 | 3.975 | 39.8 | — | — | — | — |
| A2 header_decode | 5.006 | 4.895 | 50.1 | A1→A2 | 10.3 | 25.9% | above |
| A3 rls_visible | 5.658 | 5.621 | 56.6 | A2→A3 | 6.5 | 13.0% | above |
| A4 dim_meta_decode | 6.226 | 6.223 | 62.3 | A3→A4 | 5.7 | 10.1% | above |
| A5 scalar_validate | 6.477 | 6.446 | 64.8 | A4→A5 | 2.5 | 4.0% | within |

e2e: `A0a`（agg_count）median=7.088ms・`A0b`（rls_isolation）median=7.184ms。

| 段 | median | min-of-R | ns/row（分母=可視 92,000 行） | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| W1 scalar_scan | 1.274 | 1.271 | 13.8 | — | — | — | — |
| W2 predicate | 1.809 | 1.801 | 19.7 | W1→W2 | 5.8 | 42.1% | above |
| W3 arena_copy | 3.247 | 3.229 | 35.3 | W2→W3 | 15.6 | 79.5% | above |
| W4 provider_search（一致 18,400 行） | 0.340 | 0.233 | — | — | — | — | — |
| R_dot（参照区間） | 1.729 | 1.716 | — | — | — | reference_band=3.98% | — |

e2e: `W0-cold`=75.157ms・`W0-hot`=7.208ms・`W0-nowhere`=3.043ms。raw diff（`W0-hot − W0-nowhere`）= 4.165ms（下記「W0-hot と W0-nowhere の候補集合差」節の注意を参照）。

### W0-hot と W0-nowhere の候補集合差（P2 指摘・codex-review）

`W0-nowhere`（`WHERE` なし KNN）は可視行全体（tenant-a の `tenant_a_rows` 件）を dense 探索の候補集合とするのに対し、`W0-hot`（`WHERE lang='ja'`）は事前フィルタ後の一致行（約 20%）のみを候補集合とする。したがって raw diff `W0-hot − W0-nowhere` には次の 2 つの効果が混入する。

1. **SQL 表層内の `WHERE` 上乗せ**（`scan_scalar_columns` による全可視行スキャン・述語判定・一致行の embedding 複製。W1〜W3 に相当）
2. **dense 探索（距離計算・Top-k 選出）の候補集合サイズが小さくなることによる処理量の減少**（`W4`〔一致行のみ〕対 `R_dot`／`W0-nowhere` 内部の全可視行探索の差。候補が少ないほど計算量は減る側に働く）

上記 2 つは符号が逆（1 は上乗せ・2 は削減）のため、raw diff を単純に「`WHERE` 上乗せ」として報告し、その値を `W3`（`W1`・`W2` を累積で含む値。`W1+W2+W3` のような加算は `W1` 分の `scalar_scan` コストを二重計上するため行わない）と比較して残差をパース・束縛・キャッシュ照会等へ帰属することはできない（候補集合が揃っていない）。候補集合を揃えた比較は W 系列（`W1`〜`W4`、いずれも一致行のみを対象とする一貫した集合）・`R_dot`（全可視行を対象とする参照区間）側で行っており、これらの段別内訳が主たる分析対象である。raw diff（25k: 0.521ms・100k: 4.165ms）は「候補集合差を含む e2e 全体の差」という参考値としてのみ扱う。

### 実測からの所見

- A 系列: `A1`（走査のみ）が総コスト（`A5` ns/row 比）の 6〜7 割を占める（25k: 38.9/57.9 ≈ 67.2%・100k: 39.8/64.8 ≈ 61.4%）。`A2`（ヘッダデコード）・`A3`（RLS 判定）が次点で、`A4`（dim/metadata デコード。A3 と同じキー/ヘッダ tenant 整合検査を含む）・`A5`（スカラー構造検証）の追加コストは A1〜A3 の合計より小さい。`A3→A4` の帯判定は規模で異なる——25k は ratio 11.5% が同ラウンド帯の `reference_band`（13.5%）を下回り `within_noise_band`、100k は ratio 10.1% が `reference_band`（3.98%）を上回り `above_noise_band` となる（100k で `above` になるのは `A4` 自体の増分が拡大したためではなく、100k 側で参照区間 `R_dot` のノイズ帯が 25k より大幅に狭いことによる。掲載表の帯判定列を参照）。`agg_count`／`rls_isolation` の e2e（A0a/A0b）は 25k では差 0.19%（1.580ms/1.577ms）とほぼ同値だが、100k では差 1.35%（7.088ms/7.184ms）へやや拡大する——いずれも同一走査であるという設計時の仮説（§背景）は実測でおおむね支持されるが、100k では 1% をわずかに超える差が残る。
- W 系列: `W3`（arena_copy）が規模とともに支配的になる（25k 点で W2 比 +19.4%、100k 点で W2 比 +79.5%）。一致行数（`lang='ja'` ≈ 20%）に比例して複製コストが伸びるため、規模が大きいほど `W3` の相対寄与が増す。`W0-hot` と `W0-nowhere` は候補集合が異なるため両者の raw diff を「`WHERE` 上乗せ」として単純に解釈することはできない（詳細は前節「W0-hot と W0-nowhere の候補集合差」）。
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

**所見**: `agg_count`／`rls_isolation` は `WHERE` を持たず `on_visible_row` を通らないため、#471 が `agg_count` を対象に挙げている前提は本経路には当てはまらない（`where_compound_count` には当てはまる）。両フェーズの改善は #477 側の段（A1〜A5、実測では特に A1〜A3）に帰属する。`vector_knn_where` は候補集合を揃えた `W2`（`W1` の `scan_scalar_columns` を累積で含んだうえで述語判定まで終えた値。dense 探索の候補集合サイズに依存しない）の比率で #471 の効果上限を、`W0-cold − W0-hot` で `SqlArenaCache` の寄与を示す（`W1` を別途加算すると `W1` 分の `scalar_scan` コストを二重計上するため `W2` を単独の累積値として用いる。述語判定のみの寄与を切り出す場合は `W2 − W1` を用いる。`W0-hot − W0-nowhere` の raw diff は前節の理由により #471／`SqlArenaCache` いずれの効果上限の根拠にも用いない）。

## `feature_bench` との差異

- `feature_bench`: tenant-a 20,000 行・tenant-b 5,000 行（10% Private）・`lang`/`topic`/`body` 3 スカラー列・`SELECT *` 系クエリ中心。
- 本ベンチ: tenant-a 23,000×scale 行 Public・tenant-b 2,000×scale 行 Private（crossdb モデルに合わせた比率）・`lang` 列のみ・`SELECT id` 系クエリ（crossdb ハーネスの投影と揃える）。

## スコープ外・申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測はオーナー作業。本 doc の数値は共有 QEMU 環境の参考値。
- 再実装デコーダ（`decode_dim_and_metadata_reimpl`）と正本のドリフトが将来問題化した場合の `bench-internals` 限定 src フック追加は別 Issue 判断（本 Issue は production 無変更を優先）。
- crossdb self（wire 経由・psycopg）との残差の定量帰属は #463 と同様に運用者作業。
- `feature_bench` の 13 フェーズ自体への段別計測点の埋め込みは行わない（`feature_bench` は 13 フェーズ横断の e2e 基線として不変に保つ）。
- `where_compound_count`・`group_by_having` の段別分解は本 Issue 対象外（#471 側で必要なら別途）。
