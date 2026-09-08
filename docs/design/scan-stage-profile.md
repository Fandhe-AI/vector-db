# ADR: `agg_count`／`rls_isolation`／`vector_knn_where` の段別プロファイル

- ステータス: Accepted
- 対応 Issue: #464（親 #456・ルート #455）。A 系列バイナリ配置アーティファクトの是正・基線再取得は Issue #635（親 #631・ルート #629）
- 関連ポインタ: TASK-83（SQL 表層性能受け入れ基準）・TASK-158（性能計測プロトコル基盤）・SQL-13（集計関数）・Issue #350（集計経路のデコードスキップ）・Issue #478（`VisibleBitmapCache`。A0a/A0b の e2e 値が本 doc 初版から大きく変わった要因）・`docs/design/crossdb-bench.md`・`docs/design/knn-stage-profile.md`（Issue #362 の先行事例）・`docs/design/knn-wire-stage-profile.md`（Issue #463 の先行事例）・`docs/design/benchmark-judgement-policy.md`（計測規約）・`docs/design/visible-bitmap-cache.md`

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

A1〜A5 の計測対象本体（`run()` へ渡すクロージャの中身）は `scan_stage_profile_bench.rs` 内のトップレベル関数（`stage_a1_scan`／`stage_a2_header_decode`／`stage_a3_rls_visible`／`stage_a4_dim_meta_decode`／`stage_a5_scalar_validate`）へ `#[inline(never)]` 付きで分離している（Issue #635。`dot_kernel_bench.rs::dot_wrapper` と同じ設計）。詳細・採用根拠は下記「#586 後の A 系列バイナリ配置アーティファクト（Issue #635）」節を参照。

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

## #586 後の A 系列バイナリ配置アーティファクト（Issue #635）

2026-09-08 の Phase 系一括計測切り分け（Issue #629）で、下記「実測結果」節旧版に記載していた `A1`（生 `redb` per-entry 走査のみ）が 38.9 ns/row（25k）→ 48.7 ns/row へ、比率にして約 +25% 悪化しているように見える所見が得られた。しかし追加の切り分けにより、これは **production コード（`kernel.rs`／`storage.rs` 等）の退行ではなく、本ベンチのハーネス変更に伴うバイナリ配置（コード配置・アライメント）アーティファクト**であると確定した。

### 切り分けの経緯

- 同日の A/B 5 ペア実測: 旧ハーネス時点のコミット `ee99db3` で min-of-R ≈ 0.95ms、`HEAD` で min-of-R ≈ 1.20ms（`agg_count`/`rls_isolation` 側の scale=1 min-of-R 合算値。W 系列・`R_dot` は不変）
- `git bisect --first-parent`（判定閾値: A1 min-of-R ≤ 1.08ms を good とする）→ first bad = `5076c5d`（PR #586）
- PR #586（Issue #479。`VisibleBitmapCache`（Issue #478）の非漏えい・前後比較検証タスク）は、計測ラウンドループ（`measure_a_series` の呼び出しを含むラウンド輪番）の**後方**に「A0c」ブロック（毎サンプル新規 `Storage::open`＋`EngineCore` 構築を含む cold `COUNT(*)` 対照計測。上記「段の定義」節参照）を追加していた。これ自体は A1〜A5 の計測区間を一切含まないコードだが、追加によりバイナリ全体のコード配置が変わり、`A1` の計測対象コードの配置（インライン化・アライメント・命令キャッシュ挙動）が変化した
- `HEAD` の production コードのまま、ベンチファイルだけ #586 以前（かつ #478 実装前）のものへ戻すと min-of-R は 0.93〜0.97ms に復帰する（production 側は無罪であることの直接確認）

### 摂動テストと構造変更の採用（Step 1・gate）

配置感度そのものを下げる構造変更として、A1〜A5 の計測対象本体（`run()` へ渡すクロージャの中身）を `#[inline(never)]` 付きのトップレベル関数（`stage_a1_scan` 等。既存の `dot_kernel_bench.rs::dot_wrapper` と同じ設計）へ分離した（上記「段の定義」節参照）。この構造変更が実際に配置感度を下げるかを、以下の摂動テストで確認した。

- **variant A**: 構造変更適用後の現行ハーネス（A0c ブロックは production 相当のまま）
- **variant B**: 同じ構造変更を適用した上で、A0c ブロックの本体（`Storage::open`／`EngineCore` 構築を含む実装）を、同程度のコード量を持つ無害な `black_box` ダミー計算へ置換したもの（バイナリサイズ差は 3,863,456 bytes 対 3,862,224 bytes・約 0.03%）
- 両 variant を `BENCH_SCAN_PROFILE_SCALE=1`・`BENCH_SCAN_PROFILE_ROUNDS=5` で交互に N=5 ペア実測（`docs/design/benchmark-judgement-policy.md` §3 準拠）

| pair | variant A・A1 min-of-R (ms) | variant B・A1 min-of-R (ms) |
| --- | --- | --- |
| 1 | 1.217 | 1.198 |
| 2 | 1.203 | 1.175 |
| 3 | 1.206 | 1.163 |
| 4 | 1.165 | 1.162 |
| 5 | 1.197 | 1.189 |

min-of-N: A=1.165ms・B=1.162ms（比 0.997・-0.3%）。median-of-N: A=1.203ms・B=1.175ms（比 0.977・-2.3%）。プール（A・B 計 10 run）での `(max-min)/min` スプレッド: A1 系列 4.73%（min=1.162・max=1.217）、参照区間 `R_dot` min-of-R 系列 9.04%（min=0.166・max=0.181）。A1 のプールスプレッド 4.73% は固定 ±5% 帯・参照区間実測帯（9.04%）のいずれも超えないため、「構造変更（`#[inline(never)]` 分離）により A1 の配置感度が低下した」と判定し、この構造変更を採用した（`crates/engine/benches/scan_stage_profile_bench.rs` に反映済み）。

摂動テストは A0c ブロックの**コードサイズ**（実行時コストではない）が A1 の配置へ影響していたかを切り分ける目的のみに使い、variant B のダミー実装は本ベンチには含まれない（production 相当の variant A のみを採用・以降のコミットに反映）。

### 結論

- A 系列の絶対値（ns/row）はバイナリのビルドレイアウトに依存し得るため、**同一バイナリ内での相対比較（段間の diff／ratio／帯判定）を主たる分析根拠とし、絶対値が異なるハーネス版どうしを跨いだ比較（例: 本節旧版の 38.9 ns/row と #586 後の 48.7 ns/row の単純比較）には用いない**（`docs/design/benchmark-judgement-policy.md` の精神と整合）。
- `#[inline(never)]` 分離後も A1 の絶対値は 38.9 ns/row 付近へは戻らない（後述「実測結果」の 49.9 ns/row 前後が現行ハーネスでの正しい基線）。これは A0c（#478 実装のための対照計測）自体が撤去されたわけではなく、あくまで A1 計測の配置感度が今後の変更に対して安定した、という意味であることに注意（過去の 38.9 ns/row との差は「退行」ではなく「異なるバイナリでの測定値」）。
- 以降のこの doc の実測値は、この構造変更を適用した現行ハーネス（`crates/engine/benches/scan_stage_profile_bench.rs`）での再取得値へ全面的に置き換える。

## 実測結果

計測環境: 共有 QEMU 環境（`docs/design/benchmark-judgement-policy.md` §5 の区分に従い**参考値**。専有環境での再実測は運用者作業）。`lscpu` Model name: `QEMU Virtual CPU version 2.5+`・`nproc`=12・`isa=Avx2Fma`・計測時 `loadavg` ≈ 2.0〜2.3（scale=1・scale=4 とも 1 プロセス = 1 規模点で逐次実行。並列実行中の他 Issue のビルドジョブが同時に高負荷を出す時間帯があったため、A/B 摂動テスト実測時（`loadavg` ≈ 6〜9）とは別のタイミングで低負荷を確認して実施）・`BENCH_DEDICATED_ENV` 未設定。ラウンド数: 既定 5。実測日: 2026-09-08（Issue #635）。

**再測定の経緯**: 本節の実測値は 2 段階の再測定を経ている。(1) 初回実測は codex-review／Cursor Bugbot 指摘（PR #555・P1）により、`A4`／`A5` が `A3` と同じキー/ヘッダ tenant 整合検査（`verify_row_key_tenant_reimpl`）を省いており累積段契約（A3 ⊆ A4 ⊆ A5）が成立していなかったことが判明したため、`A4`／`A5` へ同検査を追加したうえで再測定した。(2) その後 PR #586 適用後は A1 が上記「#586 後の A 系列バイナリ配置アーティファクト」節のとおりバイナリ配置アーティファクトの影響を受けていたため、`#[inline(never)]` 分離（Issue #635）を適用したうえで再測定した。下記は Issue #635 適用後の実測値（A0a／A0b の e2e 値は #478 `VisibleBitmapCache` 実装後の高速経路値へも同時に置き換わっている）。

### 25,000 行（`BENCH_SCAN_PROFILE_SCALE=1`。tenant-a 23,000・tenant-b 2,000）

per-round 生値（ms）:

| round | A1 | A2 | A3 | A4 | A5 | W1 | W2 | W3 | W4 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 1.226 | 1.406 | 1.552 | 1.715 | 1.788 | 0.311 | 0.402 | 0.483 | 0.097 |
| 1 | 1.248 | 1.397 | 1.591 | 1.725 | 1.788 | 0.313 | 0.404 | 0.472 | 0.108 |
| 2 | 1.246 | 1.390 | 1.550 | 1.715 | 1.800 | 0.319 | 0.405 | 0.470 | 0.096 |
| 3 | 1.248 | 1.396 | 1.551 | 1.717 | 1.788 | 0.319 | 0.404 | 0.469 | 0.093 |
| 4 | 1.249 | 1.413 | 1.556 | 1.714 | 1.784 | 0.315 | 0.405 | 0.471 | 0.097 |

median-of-R（ms・min-of-R）・ns/row・段差分:

| 段 | median | min-of-R | ns/row | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| A1 redb_scan | 1.248 | 1.226 | 49.9 | — | — | — | — |
| A2 header_decode | 1.397 | 1.390 | 55.9 | A1→A2 | 6.0 | 11.93% | within |
| A3 rls_visible | 1.552 | 1.550 | 62.1 | A2→A3 | 6.2 | 11.13% | within |
| A4 dim_meta_decode | 1.715 | 1.714 | 68.6 | A3→A4 | 6.5 | 10.52% | within |
| A5 scalar_validate | 1.788 | 1.784 | 71.5 | A4→A5 | 2.9 | 4.22% | within |

e2e: `A0a`（agg_count）median=0.050ms・`A0b`（rls_isolation）median=0.050ms（両者一致。#478 `VisibleBitmapCache` のヒット高速経路値——`docs/design/visible-bitmap-cache.md` 参照）。`A0c-cold`（毎サンプル新規 `Storage::open` を含む cold `COUNT(*)`。`VisibleBitmapCache` のミス経路対照値）median=6.561ms（sample minimum, N=20: 6.385ms）。

| 段 | median | min-of-R | ns/row（分母=可視 23,000 行） | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| W1 scalar_scan | 0.315 | 0.311 | 13.7 | — | — | — | — |
| W2 predicate | 0.404 | 0.402 | 17.6 | W1→W2 | 3.9 | 28.35% | above |
| W3 arena_copy | 0.471 | 0.469 | 20.5 | W2→W3 | 2.9 | 16.52% | within |
| W4 provider_search（一致 4,600 行） | 0.097 | 0.093 | — | — | — | — | — |
| R_dot（参照区間） | 0.188 | 0.188 | — | — | — | reference_band=20.82% | — |

e2e: `W0-cold`=14.540ms・`W0-hot`=0.660ms・`W0-nowhere`=0.599ms。raw diff（`W0-hot − W0-nowhere`）= 0.061ms（下記「W0-hot と W0-nowhere の候補集合差」節の注意を参照——このままでは「SQL 表層内の `WHERE` 上乗せ」として単純には解釈できない）。

### 100,000 行（`BENCH_SCAN_PROFILE_SCALE=4`。tenant-a 92,000・tenant-b 8,000）

per-round 生値（ms）:

| round | A1 | A2 | A3 | A4 | A5 | W1 | W2 | W3 | W4 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 4.791 | 6.179 | 7.501 | 7.450 | 7.903 | 1.257 | 1.835 | 3.099 | 0.219 |
| 1 | 4.764 | 6.219 | 6.812 | 7.514 | 7.831 | 1.250 | 1.837 | 3.144 | 0.203 |
| 2 | 4.844 | 6.191 | 6.857 | 7.482 | 7.861 | 1.258 | 1.797 | 3.121 | 0.221 |
| 3 | 4.778 | 6.175 | 6.954 | 7.488 | 7.754 | 1.255 | 1.801 | 3.126 | 0.249 |
| 4 | 4.763 | 6.307 | 6.814 | 7.514 | 7.836 | 1.253 | 1.795 | 3.082 | 0.330 |

median-of-R（ms・min-of-R）・ns/row・段差分:

| 段 | median | min-of-R | ns/row | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| A1 redb_scan | 4.778 | 4.763 | 47.8 | — | — | — | — |
| A2 header_decode | 6.191 | 6.175 | 61.9 | A1→A2 | 14.1 | 29.58% | above |
| A3 rls_visible | 6.857 | 6.812 | 68.6 | A2→A3 | 6.7 | 10.77% | above |
| A4 dim_meta_decode | 7.488 | 7.450 | 74.9 | A3→A4 | 6.3 | 9.19% | above |
| A5 scalar_validate | 7.836 | 7.754 | 78.4 | A4→A5 | 3.5 | 4.66% | within |

e2e: `A0a`（agg_count）median=0.195ms・`A0b`（rls_isolation）median=0.195ms（両者一致。#478 `VisibleBitmapCache` のヒット高速経路値）。`A0c-cold`median=29.147ms（sample minimum, N=20: 28.834ms）。

| 段 | median | min-of-R | ns/row（分母=可視 92,000 行） | 差分元 | diff ns/row | ratio | 帯判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| W1 scalar_scan | 1.255 | 1.250 | 13.6 | — | — | — | — |
| W2 predicate | 1.801 | 1.795 | 19.6 | W1→W2 | 5.9 | 43.50% | above |
| W3 arena_copy | 3.121 | 3.082 | 33.9 | W2→W3 | 14.3 | 73.31% | above |
| W4 provider_search（一致 18,400 行） | 0.221 | 0.203 | — | — | — | — | — |
| R_dot（参照区間） | 1.445 | 1.430 | — | — | — | reference_band=3.12% | — |

e2e: `W0-cold`=86.993ms・`W0-hot`=4.972ms・`W0-nowhere`=3.041ms。raw diff（`W0-hot − W0-nowhere`）= 1.931ms（下記「W0-hot と W0-nowhere の候補集合差」節の注意を参照）。

### W0-hot と W0-nowhere の候補集合差（P2 指摘・codex-review）

`W0-nowhere`（`WHERE` なし KNN）は可視行全体（tenant-a の `tenant_a_rows` 件）を dense 探索の候補集合とするのに対し、`W0-hot`（`WHERE lang='ja'`）は事前フィルタ後の一致行（約 20%）のみを候補集合とする。したがって raw diff `W0-hot − W0-nowhere` には次の 2 つの効果が混入する。

1. **SQL 表層内の `WHERE` 上乗せ**（`scan_scalar_columns` による全可視行スキャン・述語判定・一致行の embedding 複製。W1〜W3 に相当）
2. **dense 探索（距離計算・Top-k 選出）の候補集合サイズが小さくなることによる処理量の減少**（`W4`〔一致行のみ〕対 `R_dot`／`W0-nowhere` 内部の全可視行探索の差。候補が少ないほど計算量は減る側に働く）

上記 2 つは符号が逆（1 は上乗せ・2 は削減）のため、raw diff を単純に「`WHERE` 上乗せ」として報告し、その値を `W3`（`W1`・`W2` を累積で含む値。`W1+W2+W3` のような加算は `W1` 分の `scalar_scan` コストを二重計上するため行わない）と比較して残差をパース・束縛・キャッシュ照会等へ帰属することはできない（候補集合が揃っていない）。候補集合を揃えた比較は W 系列（`W1`〜`W4`、いずれも一致行のみを対象とする一貫した集合）・`R_dot`（全可視行を対象とする参照区間）側で行っており、これらの段別内訳が主たる分析対象である。raw diff（25k: 0.061ms・100k: 1.931ms）は「候補集合差を含む e2e 全体の差」という参考値としてのみ扱う。

### 実測からの所見

- A 系列: `A1`（走査のみ）が総コスト（`A5` ns/row 比）の 6〜7 割を占める（25k: 49.9/71.5 ≈ 69.8%・100k: 47.8/78.4 ≈ 61.0%）。`A2`（ヘッダデコード）・`A3`（RLS 判定）が次点で、`A4`（dim/metadata デコード。A3 と同じキー/ヘッダ tenant 整合検査を含む）・`A5`（スカラー構造検証）の追加コストは A1〜A3 の合計より小さい。`A2〜A4` の帯判定は規模で明確に異なる——25k は参照区間 `R_dot` のノイズ帯が 20.82% と広く `A1→A2`（11.93%）〜`A4→A5`（4.22%）まで全段が `within_noise_band` になる一方、100k は `R_dot` のノイズ帯が 3.12% と狭いため `A1→A2`（29.58%）〜`A3→A4`（9.19%）が `above_noise_band` へ転じ、`A4→A5`（4.66%）のみ固定 ±5% 帯の内側にとどまり `within` と判定される（帯判定は「固定 ±5% 帯・参照区間実測帯のいずれか一方を超えなければ within」という OR 判定であり、100k で `above` になる段が増えるのは `A2`〜`A4` 自体の増分が拡大したためではなく、100k 側の参照区間ノイズ帯が 25k より大幅に狭いことによる。掲載表の帯判定列を参照）。`agg_count`／`rls_isolation` の e2e（A0a/A0b）は #478 `VisibleBitmapCache` のヒット高速経路を通るようになり、25k（0.050ms/0.050ms）・100k（0.195ms/0.195ms）いずれも完全一致——同一走査であるという設計時の仮説（§背景）は、キャッシュ導入後もそのまま成立する。cold 側の対照値 `A0c-cold`（毎サンプル新規 `Storage::open`＋キャッシュミス経由の全行走査を含む）は 25k で 6.561ms・100k で 29.147ms（sample minimum 込み。前節「#586 後の…アーティファクト」参照）——キャッシュ非ヒット時のコストは e2e 全行走査の水準に戻ることを示す。
- W 系列: `W3`（arena_copy）が規模とともに支配的になる（25k 点で W2 比 +16.52%、100k 点で W2 比 +73.31%）。一致行数（`lang='ja'` ≈ 20%）に比例して複製コストが伸びるため、規模が大きいほど `W3` の相対寄与が増す。`W0-hot` と `W0-nowhere` は候補集合が異なるため両者の raw diff を「`WHERE` 上乗せ」として単純に解釈することはできない（詳細は前節「W0-hot と W0-nowhere の候補集合差」）。
- `W0-cold`（`SqlArenaCache` を毎回空の状態から測る）は `W0-hot` の約 17〜22 倍（25k: 14.540ms/0.660ms・100k: 86.993ms/4.972ms）——`SqlArenaCache`（Issue #363）のヒット有無がクエリ毎の redb 再デコードコストを大きく左右することを示す（`vector_knn_where` の crossdb 実測がどちらの状態に近いかは、crossdb ハーネスの接続再利用方針に依存するため本 Issue の対象外）。W0-cold／W0-hot の絶対値は本 doc 初版時点（#477・#471 の前段・#357／#363／#478 等の各種キャッシュ導入前）から変化しているが、Issue #635 の対象は A 系列のバイナリ配置アーティファクト是正・基線再取得であり、W 系列絶対値の変化は intervening な性能改善作業（`CLAUDE.md` の Issue #357・#363・#478 等）の帰結として記録するのみに留める。

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
- Issue #635: `#[inline(never)]` 分離の摂動テストは本開発環境（共有 QEMU）での 1 回の N=5 ペア実測に基づく。専有環境での再検証・より長期的な配置感度の安定性確認はオーナー作業として申し送り。W 系列の絶対値変化（本節初版比）は Issue #635 のスコープ外だが、実測値の記録として本 doc の「実測結果」節へ反映済み。
