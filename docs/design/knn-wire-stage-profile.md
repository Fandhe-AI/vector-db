# `vector_knn` 786µs の wire／SQL 表層／距離カーネル・Top-k 内訳プロファイル

- ステータス: Accepted（本コミットで計測ベンチ・ADR を追加。内訳は本ベンチの実測値
  であり、production コード〔`crates/engine/src/`・`crates/wire-server/src/`〕は
  無変更）
- 対応: Issue #463（`test(engine): vector_knn 786µs の wire／SQL 表層／距離カーネル
  内訳を切り分ける計測点を追加する`）
- 関連ポインタ: TASK-83・TASK-127・TASK-158・TASK-73/WIRE-1・SQL-1〜4・RLS-6/RLS-7
- 先行: `docs/design/crossdb-bench.md`（`vector_knn` self 786/1147µs の測定元）・
  `docs/design/knn-stage-profile.md`（Issue #362。SQL 表層内部〔走査・デコード・
  arena 構築・距離計算〕の内訳）・`docs/design/benchmark-judgement-policy.md`
  （Issue #462。交互 min-of-N・ノイズ帯併記の計測規約）

## 背景

`docs/design/crossdb-bench.md`（25,000 行・dim 128・wire 経由・psycopg・p50）で
`vector_knn`（`SELECT id FROM docs ORDER BY embedding <=> '<vec>' LIMIT 10`）は
self 786µs（p95 1147µs）。`docs/design/knn-stage-profile.md`（Issue #362）で SQL
表層内部（redb 走査・デコード・arena 構築・距離計算）の内訳は既に実測済みだが、
「wire 往復・応答組み立て」対「SQL 表層（パース・束縛・`SqlArenaCache` 参照・
投影）」対「距離計算・Top-k」という 3 層の切り分けは未計測だった。本 Issue は
この 3 層（4 区分）の内訳を実測し、Phase 4（チップ最適カーネル）の期待効果を
見積もる材料とする。

## 計測段（tier）の定義

同一プロセス内で、同一コーパス（tenant-a 23,000 行 Public ＋ tenant-b 2,000 行
Private・dim 128。crossdb の可視性モデル——`scripts/crossdb_bench/self_db.py` の
23,000/2,000 分割——と同一）・同一 200 クエリベクトル（決定的 RNG）・同一
`Arc<EngineCore>`（既定エンジン `search_engine::default_engine()` =
`ParallelSearchProvider`）を共有し、6 段を交互計測する。

| 段 | 内容 | 対応する区分 |
| --- | --- | --- |
| T1′ `kernel_distance_only` | 事前構築 `VectorArena` の全可視行へ `isa::current().dot` を適用し総和（Top-k なし） | 距離カーネル |
| T1s `provider_scalar` | `CpuScalarProvider::search`（単線・逐次） | 距離＋Top-k（単線条件） |
| T1p `provider_parallel` | `ParallelSearchProvider::search`（production 既定） | 検索カーネル段の実効値 |
| T2 `sql_surface_hot` | `EngineCore::execute_sql_in_session`（wire と同じ入口。`SqlArenaCache` ウォーム） | SQL 表層 |
| T3e `wire_encode_only` | `wire_server::result_encoder` による応答エンコードのみ（計測外で得た `QueryResult` を対象） | wire の部分区間（informational） |
| T3 `wire_roundtrip` | in-process ループバックサーバーへの簡易クエリ 1 往復 | wire e2e |

4 区分への帰属（min-of-R の中央値どうし）:

- **距離カーネル** = T1′
- **Top-k（単線条件）** = T1s − T1′（負なら測定ノイズによる逆転として n/a）
- **SQL 表層** = T2 − T1p（production 実効値である並列 provider 基準）
- **wire** = T3 − T2

Top-k は並列 provider 下では分離できない（`knn_profile_bench.rs` の
`S5_scalar − S5'` と同じ理由: `ParallelSearchProvider::search` はワーカー生成・
行範囲分割・部分 Top-k・マージが混在する）ため、単線条件（T1s − T1′）に限定し、
production 実効値は T1p として別掲する。

## 計測条件

- コーパス: `docs`（`embedding VECTOR(128)` のみ）に tenant-a 23,000 行
  （`Visibility::Public`）・tenant-b 2,000 行（`Visibility::Private`）を投入。
  crossdb の可視性モデル（tenant-a Public 23,000 + tenant-b Private 2,000）を
  再現し、`PolicyContext::new("tenant-a")`（wire 認証の既定と同じく Public のみ
  可視）で 23,000 行が可視集合になる。
- 200 クエリベクトル（決定的 RNG）を輪番し、ラウンドごとに全 6 段を 1 回ずつ
  交互計測する（`docs/design/benchmark-judgement-policy.md` §3 の交互実行
  min-of-N〔N≥5〕方式）。
- 計測入口: `crates/wire-server/benches/knn_wire_profile_bench.rs`（時間依存の
  実測本体）・`crates/wire-server/benches/harness/knn_wire.rs`（rounds パース・
  帰属計算・ノイズ帯判定・出力整形の時間非依存ロジック）・`crates/wire-server/
  tests/knn_wire_profile_accept.rs`（`make ci` 対象の回帰テスト）。`engine` 側の
  性能計測プロトコル基盤（`harness::protocol::run`・`harness::stats`・
  `harness::rng`・`harness::env_report`・`harness::sql_c1`）は `#[path]` で共有し、
  新規クレート・コピーは作らない。
- wire 側は `wire_server::server::accept_loop_with_engine` で起動した
  in-process ループバックサーバーへ、`crates/wire-server/tests/common/mod.rs`
  の生 TCP クライアントヘルパー（`send_simple_query`・`read_row_description`・
  `read_data_row`・`read_command_complete`・`read_ready_for_query`）で接続する
  （`tests/wire_nodelay_latency.rs` と同一パターン）。クライアントソケットにも
  `set_nodelay(true)` を設定する。

## 計測器の差異

crossdb（`scripts/crossdb_bench/self_db.py`）は別プロセスの release
`wire-server` バイナリへ psycopg（Python・簡易クエリ・`prepare_threshold=None`）
で接続する。本ベンチは in-process ループバックの生 TCP クライアントで、
プロセス間・言語間のオーバーヘッド（Python 側のオブジェクト生成・OS スケジューラの
プロセス切替）を含まない。そのため T3（`wire_roundtrip`）の実測値は crossdb の
786µs を下回る見込みで、その残差は「クライアント側（psycopg・プロセス間
スケジューリング）」として本ベンチの対象外（下記「実測結果」参照）。

## fail-closed 検証

出力前に以下をすべて検証する（`.claude/rules/security.md`「テナント境界」
「fail-closed を維持する」）。いずれか 1 つでも満たさない場合はベンチが
非ゼロ終了し、測定値は一切出力されない。

- arena（T1 系専用）の行数が可視集合（tenant-a の 23,000 行）と一致すること。
- T1p・T2・T3 の各ラウンドで返却行数が `TOP_K`（10）件であること。
- T1p・T2・T3 の返却 id が、いずれもテナント境界（`id < 23,000`）の範囲内で
  あること（tenant-b の不可視行が一切混入しないことの検査）。
- T3 が `ErrorResponse`（`'E'`）を返さないこと（`common::read_row_description`
  等が `'T'`/`'D'`/`'C'`/`'Z'` 以外を受け取ると即座に fail-closed で打ち切る）。

## 内訳比率の統計量・ノイズ判定の算出方法

4 区分内訳の各行は 2 種類の比率を独立に算出する（レビュー指摘対応。旧版は
両者を混同していた）。

- **`ratio`（全体構成比）**: 分子は `bucket_diff(from, to)`（min-of-R どうしの
  差分）、分母も同じ統計量（min-of-R）である `T3 wire_roundtrip` の
  `min_of_r`（`t3_min`）。分子・分母の統計量を min-of-R に統一している
  （旧版は分母に median-of-R の `t3_med` を使っており分子と統計量が
  不一致だった）。表示専用の informational な値で合否判定には使わない。
- **`band`（ノイズ判定）**: `docs/design/benchmark-judgement-policy.md` §4 が
  要求する「対象区間自身の相対増分をノイズ帯と比較する」契約に従い、
  `ratio` とは別に対象区間の `from`/`to`（いずれも min-of-R）から
  `step_ratio = |to/from − 1| × 100` を算出し、これを固定 ±5% 帯・
  参照区間帯（いずれか広い方）と比較する（`harness::knn_wire::
  step_ratio_pct`／`classify_against_bands`）。距離カーネル区分は
  `from = 0`（基準点なし）のため `step_ratio` が定義できず、`band` は
  「n/a（no baseline for step ratio）」として分類対象外にする。

各ラウンドは全 6 段（T1′/T1s/T1p/T2/T3e/T3）へ同一クエリ（200 クエリ
プールから輪番）を渡す（レビュー指摘対応。旧版は段ごとに `next_query()` を
進めており、既定 5 ラウンドでは各段が異なる 5 クエリしか測っておらず、
モジュール冒頭コメントが宣言する「同一 200 クエリベクトルを共有」という
契約と食い違っていた）。これにより T1p・T2・T3 の返却 id 集合が同一クエリに
対して完全一致することを毎ラウンド検証できる（fail-closed 検証節参照）。

また T3e（応答エンコードのみ）が使う `QueryResult` サンプルは、T2
（`sql_surface_hot`）の計測区間の外で別途 1 回だけ取得する（レビュー指摘
対応。旧版は T2 の計測クロージャ内で毎反復 `result.clone()` していたため、
エンコード用複製コストが SQL 表層区分〔T2〕に混入し、SQL 表層を過大・wire
区分を過小に見積もる懸念があった）。

## 実測結果（共有 QEMU 開発環境・参考値）

`BENCH_DEDICATED_ENV` 未設定（専有環境の自己申告なし）での 1 回の実測。
`docs/design/benchmark-judgement-policy.md` §5 の方針により、共有環境の数値は
**採否根拠にしない**参考値として扱う。以下は、統計量統一・T2 計測区間からの
クローン除去に加え、各段の全反復（warmup・計測を通じて）でクエリプールを
輪番する構成（codex-review 指摘対応。ラウンド境界のみで 1 クエリを進める
旧構成では測定クエリ数が rounds 件〔既定 5〕に留まっていた）に更新した実装で
再実測した値であり、それ以前の版の数値（30.1%/55.2%/12.2% 等・52.10%/28.20%/
13.72%/2.31% 等）とは前提が異なるため単純比較しない。

環境: `os=linux arch=x86_64 logical_cpus=12 isa=Avx2Fma`（開発コンテナ内。
`crossdb-bench.md` の RTX 3060 実機とは別セッションの CPU 実測）。

`make bench-knn-wire-profile`（`BENCH_KNN_WIRE_ROUNDS=5`、既定）。各ラウンドの
median／p95（µs）:

| round | T1′ median/p95 | T1s median/p95 | T1p median/p95 | T2 median/p95 | T3 median/p95 |
| --- | --- | --- | --- | --- | --- |
| 1 | 204.291 / 213.802 | 221.377 / 228.901 | 230.446 / 271.118 | 628.544 / 948.092 | 694.594 / 748.609 |
| 2 | 206.161 / 220.442 | 218.773 / 228.094 | 331.344 / 406.704 | 608.461 / 662.400 | 692.779 / 1060.935 |
| 3 | 206.011 / 452.764 | 216.813 / 275.381 | 254.310 / 320.165 | 611.310 / 649.428 | 715.462 / 780.982 |
| 4 | 239.498 / 250.560 | 222.464 / 230.382 | 324.086 / 356.683 | 613.010 / 927.227 | 690.255 / 734.721 |
| 5 | 206.941 / 216.330 | 219.473 / 230.167 | 226.185 / 267.433 | 629.177 / 698.806 | 706.190 / 1015.032 |

min-of-5／median-of-5（µs）:

| 段 | min-of-5 | median-of-5 |
| --- | --- | --- |
| T1′ `kernel_distance_only` | 204.291 | 206.161 |
| T1s `provider_scalar` | 216.813 | 219.473 |
| T1p `provider_parallel` | 226.185 | 254.310 |
| T2 `sql_surface_hot` | 608.461 | 613.010 |
| T3e `wire_encode_only`（informational） | 0.470 | 0.476 |
| T3 `wire_roundtrip` | 690.255 | 694.594 |

参照区間帯（T1′ の 5 ラウンド中央値の `(max-min)/min`）: 17.23%。

4 区分内訳（`diff` は min-of-5 どうしの差分。`ratio` は `T3` の min-of-5
`690.255µs` に対する構成比、`band` は各区分自身の `step_ratio_pct` による
ノイズ判定。上記「内訳比率の統計量・ノイズ判定の算出方法」節参照）:

| 区分 | diff | ratio（構成比） | band（ノイズ判定） |
| --- | --- | --- | --- |
| 距離カーネル（T1′） | 204.291µs | 29.60% | n/a（no baseline for step ratio） |
| Top-k（単線条件。T1s − T1′） | 12.522µs | 1.81% | within_noise_band |
| SQL 表層（T2 − T1p） | 382.276µs | 55.38% | above_noise_band |
| wire（T3 − T2） | 81.794µs | 11.85% | within_noise_band |

**所見**:

- **SQL 表層（T2 − T1p）が最大の区分**（55.38%）であり、距離カーネル自体
  （29.60%）より支配的。`docs/design/knn-stage-profile.md`（Issue #362）の
  内訳（`SqlArenaCache` ヒット時オーバーヘッド・投影・境界チェック等）が
  crossdb 786µs の主要因の一つであることを裏付ける。Phase 4（チップ最適
  カーネル）は距離カーネル自体を高速化しても、全体の 3 割弱にしか効かない
  ことを示唆する。
- **wire 区分（T3 − T2）は 81.794µs（11.85%）**で、`wire_encode_only`
  （0.476µs・T3 のほぼ 0%）と比べ極めて小さい。応答エンコード自体
  （`RowDescription`/`DataRow`×10/`CommandComplete`）のコストは無視できるほど
  小さく、この区分の実体は TCP 送受信・フレーミング読み取り・スレッド切替
  （`crates/wire-server/src/simple_query.rs` の複数回 `write_all`）である
  （モジュール冒頭コメント参照）。参照区間帯（17.23%・固定 5% のいずれか
  広い方）の範囲内にとどまり、統計的な有意性は今後の複数回実測で補強する。
- 本ベンチの T3（694.594µs・median-of-5）は crossdb の self 786µs と近い
  オーダーだが、計測環境（本ベンチは開発コンテナ内 CPU 実測、crossdb は
  RTX 3060 実機の別セッション）・コーパス生成・統計量（本ベンチは
  min-of-R／median-of-R、crossdb 側 786µs は p50）がいずれも揃っていない
  ため、両者の差（約 91〜96µs）を「クライアント側（psycopg・プロセス間
  スケジューリング）」の残差と断定はしない。定量的な残差の帰属は、同一
  コミット・同一環境で crossdb self を再実行する運用者作業に委ねる
  （「スコープ外・申し送り」節参照）。
- Top-k（単線条件。T1s − T1′）の diff（12.522µs・全体構成比 1.81%）は
  参照区間帯（17.23%）の範囲内であり、ノイズと区別できない。統計的な
  有意性の解釈は今後の複数回実測（`benchmark-judgement-policy.md` §3 の
  交互 N≥5 ペア）で補強する。

## 同一コミット `bench-knn-profile`（Issue #362）との対照

同一コミットで `make bench-knn-profile`（S0-hot: `execute_sql` 経由の SQL 表層
e2e。`SqlArenaCache` ヒット状態）と `make bench-knn-wire-profile`（T2:
`execute_sql_in_session` 経由の SQL 表層。同じく `SqlArenaCache` ヒット状態）は
同じ実行エントリポイント系統（`EngineCore` の SQL 表層）を対象とするため、
両者の中央値はオーダーが一致する（T2 の実測 608.461µs〔min-of-5〕は
`knn-stage-profile.md` 記載の S0-hot の実測レンジ——測定環境・実行時刻が
異なるため厳密な再現値ではないが同オーダー——と整合する）。厳密な数値比較は
測定環境・実行時刻が異なるため行わない（`benchmark-judgement-policy.md` §5 の
「共有環境の数値は採否根拠にしない」方針どおり、ここでは整合性の定性確認に
留める）。

## スコープ外・申し送り

- psycopg／別プロセス実行での残差（「計測器の差異」節）の定量帰属は、crossdb
  self を同一コミットで再実行する運用者作業。
- 応答送出が複数回の `write_all` に分かれている点（バッファリング統合）は
  本ベンチの所見として記録するのみで、production 変更は別 Issue（Phase 4 系）
  で判断する。→ Issue #481 で実装。`docs/design/wire-response-buffering.md` 参照。
- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測はオーナー作業。本 doc の数値は
  共有開発環境の参考値。
- `docs/design/hotpath-implementation-survey.md`・`docs/design/
  chip-kernel-guidelines.md`（Issue #470。本コミット時点で未作成）との
  相互リンクは、それらのマージ後に別途追加する。
