# 接続処理モデル（1 接続 1 スレッド vs 固定プール）の判断記録

- ステータス: **Accepted（現状維持: 1 接続 1 スレッド ＋ 接続数上限。固定
  スレッドプールは不採用）**
- 対応: Issue #482（親 #480・#457・ルート #455）
- 前提: TASK-69（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-5,
  WIRE-6）。依存: Issue #481（`docs/design/wire-response-buffering.md`）
- 計測規約: `docs/design/benchmark-judgement-policy.md`

## 背景

`wire-server` は接続を受理するたびに `std::thread::spawn` で専用 OS スレッドを
1 本割り当て、そのスレッドが接続の生存期間中ずっとブロッキング I/O で
読み書きする（`server.rs::accept_loop_inner`）。同時接続数に比例してスレッドが
増えるモデルは、DoS 面（無制限に接続されるとスレッド・スタックが尽きる）・
性能面（OS スケジューラのコンテキストスイッチコストが接続数に比例して増える
可能性がある）の両方で上限設計の妥当性を検討する必要がある、というのが本
Issue の出発点である。

## 現状の防御機構（TASK-69・WIRE-5／WIRE-6。既実装）

上限ガード自体は本 Issue に先行して実装済みであり、本 Issue で新規に追加した
production コードはない。

- `crates/wire-server/src/limits.rs::MAX_CONNECTIONS = 64`: 同時接続数の上限
  （WIRE-6）。`ConnectionLimiter`（`Arc<AtomicUsize>` ベースの CAS ループ・
  RAII `ConnectionPermit`）が枠を管理し、上限到達時は `try_acquire` が `None`
  を返す。
- `crates/wire-server/src/limits.rs::READ_TIMEOUT = 30s`: 認証前後を問わず
  一律に適用する読み取り・書き込みタイムアウト（WIRE-5）。アイドル接続が
  枠を無期限に占有できない。
- `server.rs::accept_loop_inner`: 枠確保に失敗した接続はハンドシェイクへ
  進めず、`limits::reject_too_many_connections`（`'E'`／SQLSTATE `53300`・
  S/C/M のみ・即時 close。他テナント情報を含まない）で拒否する。
- 拒否応答の書き込み自体は別枠有界のワーカー（`MAX_REJECT_WORKERS = 16`）へ
  委譲し、`MAX_CONNECTIONS` の枠管理外で無制限にスレッドが増える経路を作らない
  （拒否ワーカー枯渇時は応答なし close。fail-closed）。
- 受理接続本体は `std::thread::spawn`（1 接続 1 スレッド）。

この結果、プロセス全体のスレッド数は「接続スレッド ≤ 64」＋「拒否ワーカー
≤ 16」＋「accept ループ本体 1」で合計 ≤ 81 本に有界化されている。

## 検討した代替案

| 案 | 概要 | 評価 |
| --- | --- | --- |
| **A. 現状維持**（1 接続 1 スレッド ＋ 接続数上限） | 上記のとおり | **採用** |
| B. 自作固定サイズスレッドプール | 起動時に固定本数のワーカースレッドを立て、接続をキューへ積んで処理する | 不採用（下記参照） |
| C. 非同期 I/O（epoll/kqueue 相当のイベントループ） | 1 スレッドが多数の接続を多重化する | 不採用（下記参照） |

### B. 固定サイズスレッドプールを不採用とする理由

`MAX_CONNECTIONS` による接続数上限は、それ自体が「遅延充填される固定サイズの
資源プール」と同じ効果を持つ——スレッド数の絶対上限を有界化するという目的は
既に達成されている。固定プールを別途導入する動機として残るのは「スレッド数を
接続数より小さく保ちたい」というものだが、接続はブロッキング I/O で読み待ち
する（`READ_TIMEOUT` まで最大 30 秒）ため、接続数より小さいワーカープールに
載せ替えると次のいずれかが起きる。

- アイドルだが open な接続がワーカーを専有し、他の接続が新規クエリを送っても
  ワーカー空き待ちでキューに滞留する（`READ_TIMEOUT` の「認証前後で同一値」
  契約と衝突する新たな待ち時間が生まれる）。
- 上記を避けるには非同期 I/O（poll/epoll 相当）でワーカー 1 本が複数接続を
  多重化する必要があるが、これは案 C（非同期 I/O）そのものであり、案 B は
  実質的に案 C を内包しないと解決しない。

固定プール単体（同期ブロッキング I/O のまま）では、既存の接続数上限が持つ
資源有界化効果を上回る利点がなく、実装・維持コストだけが増える。

### C. 非同期 I/O を不採用とする理由

std のみでの非同期 I/O 実装は `libc` 直呼び出し（`unsafe`）か、`mio`／
`tokio` 等の非同期ランタイム依存が事実上必須になる。本リポの規約は次のとおり
これを禁じている。

- 依存最小・追加はユーザー承認制（`.claude/rules/dependency-policy.md`）。
  wire プロトコル層は特に外部ライブラリ非依存の自作実装を方針として掲げている。
- `unsafe` は原則禁止、必要な場合も理由・不変条件の明記とユーザー承認が必要
  （`.claude/rules/coding-rust.md`）。

非同期 I/O 化は接続数上限（DoS 対策）そのものの代替にはならず（非同期でも
同時ハンドリング数の上限は別途必要）、上記の制約に照らして採用しない。

## 判断

**現状維持（案 A）を Accepted とする。** 接続処理モデルは 1 接続 1 スレッド ＋
`MAX_CONNECTIONS`（64）の上限のまま変更しない。根拠は次のとおり構造的であり、
共有 QEMU 環境の実測（後述）を採否の根拠にはしていない
（`benchmark-judgement-policy.md` §5 のとおり、本判断は「production 変更を
行わない現状維持」であり実測を必須の根拠としない区分に該当する）。

1. 接続数上限が既にスレッド数を有界化しており、固定プールの導入で追加的に
   得られる資源保護効果がない。
2. 固定プールへの置き換えは、非同期 I/O 化を伴わない限り新たな待ち時間
   （ワーカー枯渇時のキュー滞留）を生み、`READ_TIMEOUT` 契約と整合しない。
3. 非同期 I/O 化は依存最小方針・`unsafe` 原則禁止という本リポの既定方針と
   衝突する。
4. 本モデルは PostgreSQL 本体（プロセス／接続方式。`max_connections` 超過で
   `53300`）と同型の契約であり、pg wire v3 互換を掲げる本実装が pg クライアント
   の期待する接続断・エラー応答の挙動と整合する。

他実装の接続処理モデル（手法名・採否・ライセンスのみの参照。
`hotpath-implementation-survey.md` と同じ方針で本文非転記）:

| 実装 | 接続処理モデル | ライセンス |
| --- | --- | --- |
| PostgreSQL 本体 | プロセスフォーク方式（接続 1 本 = 1 プロセス）＋ `max_connections` | PostgreSQL License |
| Qdrant | 非同期ランタイム（tokio）によるイベントループ多重化 | Apache-2.0 |
| pgvector | PostgreSQL の拡張として動作し、接続処理は PostgreSQL 本体へ委譲 | PostgreSQL License |
| sqlite-vec | インプロセス組み込み（ネットワーク接続処理を持たない） | MIT / Apache-2.0 |

## スケール阻害の候補（仮説・計測で確認していない）

`PrefilterCache`（TASK-169）・`SqlArenaCache`（Issue #363）・
`SparseIndexCache`（Issue #357）・`HnswIndexCache`（Issue #408）はいずれも
lookup 時に短時間の書き込みロック（LRU 更新）を取る。同時接続数 N が増えた
ときのスループット頭打ちが、接続スレッドモデル自体ではなくこれらのキャッシュ
のロック競合に起因する可能性がある。本 Issue では検証しておらず、後続の
再訪条件（下記）として記録するにとどめる。

## 参考実測（規約未充足・受け入れ条件としては不要だが記録する）

`crates/wire-server/tests/wire_concurrency_throughput.rs::
wire_concurrency_throughput_measurement`（`#[ignore]`・手動専用。
`make bench-wire-concurrency` から `WIRE_CONCURRENCY_N` を指定して実行）で、
既定検索エンジン（`engine::search_engine::default_engine()`。
`engine::parallel_search` のワーカー予算を経由する production 既定経路）・
25,000 行・dim 128 のコーパスに対する `SELECT id FROM docs ORDER BY embedding
<=> '<vec>' LIMIT 10` を、同時接続数 N=1／8／64 のクライアントスレッドが
それぞれ 100 往復（ウォームアップ 5 往復除く）ずつ発行したときの集計 QPS・
per-query レイテンシを 3 run ずつ実測した（`cargo test --release -p wire-server
--test wire_concurrency_throughput -- --ignored --nocapture`。commit
`895e6cd20234bfdbb5b4ac838b3c2b000998fbe4`）。

| N | run | QPS | per-query min (us) | p50 (us) | p95 (us) | max (us) |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | 1106.2 | 748 | 843 | 1218 | 1283 |
| 1 | 2 | 1202.5 | 705 | 770 | 1101 | 1200 |
| 1 | 3 | 1180.1 | 715 | 800 | 1142 | 1231 |
| 8 | 1 | 3046.4 | 878 | 2268 | 4767 | 7844 |
| 8 | 2 | 3080.1 | 831 | 2467 | 4829 | 7871 |
| 8 | 3 | 3141.5 | 731 | 2290 | 4839 | 8175 |
| 64 | 1 | 6946.4 | 757 | 4894 | 18102 | 212082 |
| 64 | 2 | 7402.0 | 789 | 4583 | 17474 | 190630 |
| 64 | 3 | 8155.8 | 731 | 4365 | 16294 | 177537 |

参照区間（`SET search_mode = 'recall'` の wire 往復。20 回）: N=1 で
min=22us・p50=22〜23us・max=40〜50us、N=64 で min=22us・p50=22us・max=43us
（いずれも本ハーネス実行時に併せて計測した参考値）。

計測環境: 共有 QEMU 開発環境（`QEMU Virtual CPU version 2.5+`・12 vCPU）。
`nproc=12`。専有環境（`BENCH_DEDICATED_ENV=1`）ではない。

**本節の計測は `benchmark-judgement-policy.md` §3（交互 N ≥ 5 ペア・per-run
生データ保持）を充足しない**（各 N 3 run・per-run 値列は集計後の min/p50/p95/
max のみを記録し、生の往復値列は保持していない）。また本判断は「production
変更を行わない現状維持」であり、上記のとおり採否の根拠としては用いていない
（`benchmark-judgement-policy.md` §5 の環境適格性区分に照らしても、本節の
数値は参考値にとどまる）。

QPS は N を増やすほど単調に増加するが、p50／p95／max は N=8 で顕著に、N=64
では p95 が N=1 の約 15〜20 倍・max が最大 200ms 超まで悪化する。この悪化が
（a）接続スレッド数自体の増加（コンテキストスイッチ）、（b）
`engine::parallel_search` のワーカー予算（`MAX_TOTAL_EXTRA_WORKER_THREADS =
64`）との相互作用によるオーバーサブスクリプション（12 vCPU に対し接続スレッド
64 ＋ 検索ワーカー予算 64 が競合しうる）、（c）上記キャッシュのロック競合の
いずれに主に起因するかは、本実測（1 種類の構成のみ）からは切り分けられない。
接続スレッドモデル自体の優劣を判定する根拠としては扱わない。

## 検証

- 上限ガード回帰テスト（受け入れ条件 (b)）: `crates/wire-server/tests/
  wire_limits.rs::wire6_production_max_connections_rejects_the_65th_connection`
  （新規）。production 定数 `MAX_CONNECTIONS`（64）そのもので accept ループを
  通し、64 本保持中に 65 本目が `'E'`／SQLSTATE `53300` で拒否され、
  `limiter.active() <= MAX_CONNECTIONS` を維持することを固定する。既存の
  `wire6_concurrent_burst_never_exceeds_max` 等はパラメータ化した小さい上限
  （1〜4）でのみ検証しており、production 定数そのものでの回帰は本テストが
  初めて固定する。
- 既存 wire テスト（`crates/wire-server/tests/*`）は無変更のまま green。
- スループット手動計測ハーネス（`wire_concurrency_throughput.rs`）の動作確認:
  `WIRE_CONCURRENCY_N` 未設定・範囲外（0・65）で fail-closed に失敗すること、
  N=1〜64 の指定でハーネスが完走し出力形式（QPS・per-query min/p50/p95/max・
  参照区間帯）が仕様どおりであることを確認済み。

## スコープ外・申し送り

- 受理経路 `std::thread::spawn`（生成失敗で panic → `recovery::fail_fast` に
  よりプロセス終了。TASK-99・RECOVER-8）と拒否経路 `Builder::spawn`（失敗時
  ログのみ）の非対称の是正。総スレッド数が `MAX_CONNECTIONS` ＋
  `MAX_REJECT_WORKERS` ＋ 1 で有界なため通常の ulimit 下では到達不能であり、
  再現テストが書けないため本 Issue では変更しない。
- 専有環境（`BENCH_DEDICATED_ENV=1`）での N 別再実測と、上記「スケール阻害の
  候補」（キャッシュのロック競合仮説）の切り分け（オーナー作業）。
- `crates/wire-server/tests/common/mod.rs::spawn_server_with_engine` の接続
  上限パラメータ化。他テストへの影響を避けるため、本 Issue の計測ハーネスは
  独自のローカルヘルパー（`wire_concurrency_throughput.rs::
  spawn_server_with_max_connections`）を使う。
- TLS（TASK-72・WIRE-9）導入時の接続あたりコスト再評価。

## 再訪条件

- 専有環境実測で N=8 以降のスループット頭打ちが `engine::parallel_search` の
  ワーカー予算ではなく wire 側の接続スレッドモデルに帰属すると切り分けられた
  場合。
- TLS（TASK-72・WIRE-9）導入で接続あたりコストが変わった場合。
