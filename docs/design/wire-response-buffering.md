# 簡易クエリ応答の単一バッファ組み立て・1 回 write 送出

- ステータス: Proposed（実装済み・共有環境参考値あり。受け入れ条件 (a) は
  `docs/design/benchmark-judgement-policy.md` §3・§4 の必須計測プロトコルを
  充足しておらず評価保留。専有環境再実測はオーナー作業。同 §5 の環境適格性
  区分に従う）
- 対応: Issue #481（`perf(wire): DataRow 群を単一バッファへ組み立てて 1 回の write
  で送出する`）。依存: Issue #463（`docs/design/knn-wire-stage-profile.md`）
- 前提: `docs/spec/04-behavior/wire-protocol.md` WIRE-1（ポインタ）

## 背景

簡易クエリ（`'Q'`）の成功応答は `RowDescription` → `DataRow` × N →
`CommandComplete` → `ReadyForQuery` の 4 種フレームからなるが、
Issue #481 以前の `simple_query.rs::respond_query_result` はこれをフレームごとに
個別の `write_all`（行数 + 3 回のシステムコール）で送出していた。`encode_data_row`
も行ごとに新規 `Vec<u8>` を確保していた。

`docs/design/knn-wire-stage-profile.md`（Issue #463）の「スコープ外・申し送り」で
この点は明示的に先送りされており、`docs/design/crossdb-bench.md` の広域取得
フェーズ `bulk_knn_k1000`（`SELECT id, body FROM docs ORDER BY embedding <=> '<vec>'
LIMIT 1000`・25,000 行・dim 128・wire 経由・psycopg）では行数（1,000）に比例した
`write_all` 回数がレイテンシへ乗っていた。

## 設計

### `ResponseBuffer`（`crates/wire-server/src/response_buffer.rs`。crate 内限定）

- `push_frame(w, frame)`: 完成済みフレームを未送出バッファへ積む。積んだ結果が
  上限（`limits::MAX_RESPONSE_BUFFER_BYTES` = 1 MiB）を超え、かつバッファが
  非空なら先に `flush` する（**「拒否」ではなくフラッシュ閾値**——応答全体が
  上限を超えても、超過分を分割送出するだけでエラーにはしない）。
- フレームは常にフレーム境界で扱う（分割してバッファへまたがせない）。1 フレーム
  自体が上限を超える場合はバッファを経由せず直接 `write_all` する（コピー回避）。
- `frame_start`/`as_mut_vec`/`truncate_to`: 呼び出し元が `DataRow` を in-place
  エンコードし、失敗時に書きかけを巻き戻せるようにする（下記参照）。
- `flush`: 非空なら 1 回の `write_all`。

接続あたりの追加常駐メモリは上限値 + フレーム 1 個ぶんに有界化され、
`limits::MAX_CONNECTIONS`（64）を掛けても全体常駐メモリは有界のまま
（DoS 対策・OWASP A05）。

### `result_encoder::encode_data_row_into`

行ごとに新規 `Vec<u8>` を確保していた旧 `encode_data_row` を、呼び出し元が持つ
1 個のバッファへ直接追記する形へ変更した。長さフィールドはプレースホルダを
push してから `out.get_mut` で backpatch する（`unwrap`/`[]` を使わない —
`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。**エンコードに失敗した
場合は必ず呼び出し前の長さへ `truncate` してから返す**契約とし、部分フレームを
絶対に残さない。旧 `encode_data_row` は `encode_data_row_into` を呼ぶ薄い
ラッパーとして残し、生成バイト列は完全に同一（既存単体テスト・bench はそのまま
green）。

`ReadyForQuery`（'Z'）のバイトレイアウトも `result_encoder::
encode_ready_for_query` へ一元化した（以前は `handshake.rs::
write_ready_for_query` にのみ存在し、`ResponseBuffer` へ他フレームと同じ形で
積める関数が無かった）。`handshake::write_ready_for_query` はこれを呼ぶだけの
薄いラッパーへ変更（`write_ready_for_query_io` の公開契約は不変）。

### `simple_query.rs::respond_query_result`

1. `RowDescription` を `push_frame`。
2. 各行を `encode_data_row_into` でバッファへ直接エンコードし、バッファ長が
   上限に達したらその時点で `flush`（フレーム境界での分割送出）。
   - エンコード失敗時は書きかけを `truncate_to` で巻き戻し、**完成済み
     フレームを `flush` してから** `ErrorResponse`（`XX000`）+ `ReadyForQuery`
     へ切り替える（pg プロトコル上、`DataRow` 後の `ErrorResponse` は正当。
     部分フレームは出さない）。
3. `CommandComplete`・`ReadyForQuery` を `push_frame` してから最後に 1 回
   `flush`（応答一式が上限以下なら **write は 1 回**）。

`SET`/`CREATE FUNCTION`/`INSERT` 応答（`CommandComplete` + `ReadyForQuery` の
2 write）はスコープ外として現状維持（「スコープ外・申し送り」参照）。

### RECOVER-5/6 との関係

バッファ組み立て・送出は `execute_and_respond` が「outcome を決定する区間」
（`_emergency_registration` の生存区間）を抜けた後、`_response_boundary`
（RECOVER-5 (3)）の生存区間内で完結する。バッファ組み立て自体はメモリ上の
操作でしかなく、commit 成功境界・応答一意性の契約を変えない。

### `TCP_NODELAY`（`server.rs`）

応答は原則 1 回の `write_all` へ束ねられるが、上限超過時の分割送出・
`SET`/`INSERT` 等の 2 write では引き続き複数 `write_all` に分かれるため、
`TCP_NODELAY`（Issue #451 で追加）は維持する。コメントのみ更新（挙動不変）。

## 検証

- 既存 wire テスト（`crates/wire-server/tests/*`。`wire_nodelay_latency.rs`
  含む）は**無変更のまま** green（受信バイト列が同一のため。`common` の
  クライアントヘルパーは `read_exact` でフレーム単位に読み、write 境界に
  依存しない）。
- 新規単体テスト（`response_buffer.rs`）: cap 未満→1 write／cap 超過→
  フレーム境界で分割／単一フレーム > cap → 直送／`truncate_to` の巻き戻し。
- 新規単体テスト（`result_encoder.rs`）: `encode_data_row_into` が
  `encode_data_row` とバイト同一・既存内容への追記・32,768 セル行での失敗時
  巻き戻し・`encode_ready_for_query` のレイアウト固定。
- 新規単体テスト（`simple_query.rs`）: 実ループバックソケット経由で
  `respond_query_result` の送出バイト列が個別エンコードの連結と完全一致する
  こと、および行の途中でエンコード不能な行が混在する場合に完成済みフレーム
  送出 → `ErrorResponse` → `ReadyForQuery` となり部分フレームが混入しない
  ことを確認。
- 新規結合テスト（`tests/wire_bulk_response.rs`）: 実サーバー経由で
  1,000 行（`id, body`）の `SELECT ... LIMIT 1000` が全行欠落なく届くこと、
  および応答合計が `MAX_RESPONSE_BUFFER_BYTES`（1 MiB）を跨ぐケース
  （約 2 MiB）でも分割送出を経て全行が届くことを確認。

## 参考実測（規約未充足・受け入れ条件 (a) の評価は保留）

`crates/wire-server/tests/wire_bulk_response.rs::
wire_bulk_select_latency_measurement`（`#[ignore]`・手動専用）で、
`bulk_knn_k1000` 相当（`id, body`・本文約 200B・k=1,000・全件同一近傍方向の
25,000 行規模を模した 1,000 行コーパス）の wire 往復レイテンシを before/after
3 ペア実測した（`cargo test --release -p fandhe-vector-db-wire-server --test
wire_bulk_response -- --ignored --nocapture wire_bulk_select_latency_measurement`。
各ペア 20 往復・中央値採用。before は本 PR の `crates/wire-server/src/` 変更のみ
`git stash` で除去した状態＝依存 Issue #463 時点の `origin/main`
`c636a81`。after は本 PR の作業ブランチの未コミット作業ツリー）。

| ペア | before median (us) | after median (us) | before min (us) | after min (us) |
| --- | --- | --- | --- | --- |
| 1 | 3164 | 2164 | 3123 | 1950 |
| 2 | 3161 | 2170 | 2889 | 1978 |
| 3 | 3164 | 2171 | 3123 | 2043 |

median の ratio（after/median ÷ before/median）はいずれのペアも約 0.685
（3 ペアとも 2164〜2171us で安定、before も 3161〜3164us で安定）で、
`docs/design/benchmark-judgement-policy.md` §4 の固定相対帯 ±5% は明確に
超える（`classify_change` 上は `Improved`、約 -31%）。

**ただし本節の計測は `benchmark-judgement-policy.md` の必須プロトコルを
充足しておらず、受け入れ条件 (a) の評価は保留とする**（規約は「規約を満たす
計測を整備するか、未充足を明示して評価を保留する」ことを求めており、後者を
選択した）。未充足の項目は次のとおり。

- **§3（計測プロトコル）**:
  - ペア数が 3 で、必須の `N ≥ 5` を満たさない。
  - 各ペアの生データは 20 往復の中央値・最小値のみを保持し、20 回の
    per-run 値列そのものは記録・保存していない（事後の再判定ができない）。
  - after 側の計測対象は本 PR の作業ブランチの**未コミット作業ツリー**であり、
    再現可能な commit hash を記録していない（before の `c636a81` も
    `git stash` による差分除去であって、クリーンな checkout ではない。
    「ビルド条件の統一」の観点でも申し送り事項とする）。
  - `lscpu` の命令セットフラグ・`nproc`・各 run 時点の `loadavg`・同時実行
    プロセスの有無・`BENCH_DEDICATED_ENV` の設定有無を記録していない。
- **§4（ノイズ帯の定義）**:
  - 追加ハーネスはクエリ送出〜`ReadyForQuery`（＝本 PR の変更を含む対象区間
    そのもの）のみを計時しており、**本 PR で変更しない区間（参照区間）を
    独立に計測していない**。上記「median の ratio」段落より前の版では
    対象区間自身の中央値の安定度（3164〜3164us 等）を参照区間の代用として
    ノイズ帯を推定していたが、これは §4 が定義する「変更を含まない参照区間の
    run-to-run 幅」ではないため撤回する。
  - 実測帯（`reference_band`）が未算出のため、§4 が要求する「固定相対帯・
    実測帯の両方を超えること」を判定できない。固定相対帯（約 -31%）のみが
    判明している状態であり、実測帯側は「未計測」として扱う。

計測環境: 共有 QEMU 開発環境（`QEMU Virtual CPU version 2.5+`・12 vCPU）。
`docs/design/benchmark-judgement-policy.md` §5 の環境適格性区分に従い、本節の
数値は**参考値**として記録する（同区分の表のとおり、共有 QEMU 環境は
「perf 動機の production 変更の採用（Accepted）」の採否根拠には**不可**であり、
上記の規約未充足を解消しても本節単独では Accepted 判定の根拠にならない）。

行数比例のシステムコール削減という構造的な改善（k=1,000 なら 1,003 回の
`write_all` → 応答合計が 1 MiB 以下なら 1 回）は環境に依存しない設計上の効果
であり、共有環境の参考実測もこれと整合する方向（約 31% 短縮）を示しているが、
上記のとおり規約上の実測帯判定・N≥5 ペアでの裏付けは行えていない。

再実測（オーナー作業）は次を満たすこと:

- before/after ともに `git stash` ではなくクリーンな commit hash から
  ビルドし、両者の hash を記録する。
- 交互 5 ペア以上・各ペアの 20 往復 per-run 値列を保持する。
- 参照区間として、本 PR で変更しない区間（例: 同一セッションでの `SET`
  応答〔`CommandComplete` + `ReadyForQuery` の 2 write。本 PR ではスコープ外
  として無変更〕の wire 往復、または SQL 表層・engine 側処理のみを計時する
  区間）を独立に計測し、`reference_band` を算出する。
- `lscpu` 命令セットフラグ・`nproc`・各 run の `loadavg`・同時実行プロセスの
  有無・`BENCH_DEDICATED_ENV` の設定有無を記録する。

## スコープ外・申し送り

- `SET`／`CREATE FUNCTION`／`INSERT` 応答（`CommandComplete` + `ReadyForQuery`
  の 2 write）・`ErrorResponse` + `ReadyForQuery` の 1 write 統合。
- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測と本 ADR の Accepted 判定
  （オーナー作業）。
- `bulk_knn_k1000` の主因である Top-k 確定前の全行 `scan_scalar_columns`
  （Issue #453）は engine 側の別 Issue（本 Issue の対象外）。
- crossdb `bulk_knn_k1000` の self 再実行（`scripts/crossdb_bench/`。25,000 行
  規模での再計測）は fixture・venv が揃う環境での運用者作業。
