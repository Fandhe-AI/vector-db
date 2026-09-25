# TLS 1.3 サーバー側ハンドシェイク状態機械と alert 処理 実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-5・WIRE-9・HTTP-10 ポインタ）・Issue #965・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）の分解 14/20。既存の部品
（レコード層・ハンドシェイクメッセージ層・`ClientHello` 受理判定・
X25519・鍵スケジュール・レコード保護・transcript hash・Finished・
`CertificateVerify`・X.509 証明書チェーン）をつなぎ、ClientHello 受信から
client Finished 検証までを進めるサーバー側の状態機械と、alert（RFC 8446
§6）の送出・受信処理、ハンドシェイク中の読み取りタイムアウトを実装した。

## モジュール配置の意図的な逸脱

Issue の記述は「`tls/handshake.rs` の状態機械」だが、`handshake.rs`
（Issue #953）は「鍵・状態を持たない純粋な codec」であることをモジュール
doc で確定済みのため、状態を持つ本体は新規ファイル
`tls/server_handshake.rs` として追加した。alert メッセージの
parse/serialize・受信分類も新規ファイル `tls/alert.rs` に切り出した。
`mod.rs` へ `pub mod alert;`・`pub mod server_handshake;` の 2 行を追加した。

## 層の分離

- **`ServerHandshake<E: HandshakeEntropy>`**: ソケットを持たない、レコード
  単位の push 型状態機械。`handle_record(&Record) -> Result<Step,
  ServerHandshakeError>` のみを公開 API とする
- **`perform_server_handshake`／`perform_server_handshake_with_timeout`**:
  `HandshakeTransport`（`Read + Write` + タイムアウト設定 + shutdown）を
  介した blocking driver
- **`TlsSession`**: ハンドシェイク完了後のアプリケーションデータ往復
  （`seal_application_data`／`open_record`／`close_notify`）

## 状態遷移

```text
ExpectClientHello{after_hrr:false}
  → (RetryRequestX25519) → ExpectClientHello{after_hrr:true}
  → (Accept) → ExpectClientFinished
  → (client Finished 検証成功) → Complete（TlsSession を払い出す）
```

`Failed(ServerHandshakeError)`・`Closed` は終端状態。`ServerHandshakeError`
は全 variant が `Copy` であり、`HandshakeBuffer`・`RecordBuffer` と同じ
fail-closed な poison 契約（一度 `Err` を返したら以後同じ理由の `Err` を
返し続ける）を満たす。

## HelloRetryRequest は 1 回まで

`client_hello::negotiate` 自体が「`after_hrr` が `Some` のときは
`RetryRequestX25519` を返さない」契約を持つため、本状態機械は HRR を
2 回送る経路を構造的に持たない。防御用の分岐
（`decide_after_client_hello`。HRR 送出済みで `RetryRequestX25519` を
受けたら `handshake_failure`）を直接テストできる純粋関数として切り出した。

外部から観測できる alert 種別を、真の 2 回目 HRR 要求時（構造的に到達
しない防御分岐）と、HRR 後に x25519 の `key_share` を含まない CH2
（`negotiate` が `illegal_parameter` で拒否）とで揃えるかどうかは、
`client_hello.rs`（#954）の既存設計判断の変更にあたるため据え置いた
（変更する場合は `negotiate` 側の判定・既存テスト・
`docs/design/tls-client-hello.md` の記述を別途更新する）。

## alert の方針

- `record::AlertDescription` に `CloseNotify`(0)・`UserCanceled`(90) を
  追加した（`#[non_exhaustive]` のため非破壊）
- `alert::Alert::parse` は fragment がちょうど 2 バイト・level が 1/2 の
  いずれかでなければ `decode_error`
- 受信 alert の分類（`alert::classify_received`）: `close_notify`／
  `user_canceled` は正常終了、それ以外（未知の値を含む）は fatal として
  `Err(ReceivedFatalAlert)`（応答は送らない）
- fatal alert の送出は `fatal_alert_of(&ServerHandshakeError) ->
  Option<AlertDescription>` に集約し、各サブモジュールの既存
  `alert_description()` を再利用する（写像を再実装しない）
- 送出できるのは 1 回のみ。`handle_record` は `Err` を返す直前に現在の
  `Sealer` の epoch でちょうど 1 レコードの alert を seal しようと試み、
  `Result::Err` は出力バイト列を運べないため一時フィールド
  `pending_alert_output` へ保存する。呼び出し元は `Err` を受けた直後に
  `take_pending_alert_output()` で取り出す 2 段構え
- alert を送らずに閉じるケース: `ProtectionError::SequenceExhausted`（鍵を
  使い切った後は保護された alert を安全に送れない）、レコード層の
  `Truncated`／`Io`／EOF／読み取りタイムアウト（`fail_on_record_error`・
  driver 側で判定）、受信済み fatal alert への応答（RFC 8446 §6 は
  alert への応答で新たな alert を送ることを求めない）

## middlebox 互換ダミー CCS（RFC 8446 付録 D.4）

`ClientHello` 受信後（`after_hrr:true` または `ExpectClientFinished`）に
限り、外側 type が `ChangeCipherSpec` かつ fragment がちょうど `[0x01]`
のレコードを `Opener::open` の**前**に読み捨てる。受理数の上限
`MAX_DUMMY_CCS_RECORDS`（既定 1。RFC 8446 付録 D.4 に従うクライアントは
1 回しか送らない）を超えた分・時期外・値違反はいずれも
`unexpected_message`。

## client Finished の検証順序

`verify_client_finished` を `Transcript::append_client_finished` より
**先**に呼ぶ（transcript は「受理できた」メッセージのみを蓄積する契約の
ため）。検証成功後に transcript へ反映し、鍵変更境界の整列検査
（バッファに部分メッセージ・追加の完全メッセージが残っていないか）を行う。

## ハンドシェイク中の読み取りタイムアウト

`HANDSHAKE_READ_TIMEOUT = crate::limits::READ_TIMEOUT`（WIRE-5 の簡易
クエリ応答と同値）を単一情報源とし、`tests/tls_server_handshake.rs::
handshake_read_timeout_matches_wire_read_timeout` で固定した。

## 対象外（後続 sub-issue の担当）

| 対象 | 担当 |
| ---- | ---- |
| `SSLRequest` への `'S'` 応答・`server.rs`／pg 側 `handshake.rs` への接続結線 | #966 |
| CLI からの証明書・鍵の読み込み | #967 |
| HTTPS 表層 | #968 |
| 3 クライアント接続テスト | #969 |
| channel binding | #970 |
| 監査・ADR の Accepted 化 | #971 |

KeyUpdate・NewSessionTicket・0-RTT・クライアント証明書は親 Issue の方針で
対象外のまま。

## 検証

- `crates/wire-server/src/tls/server_handshake.rs`（単体テスト）:
  `decide_after_client_hello` の 3 分岐・`fatal_alert_of` の網羅的写像
- `crates/wire-server/tests/tls_server_handshake.rs`（結合テスト。公開
  API のみ使用）:
  - RFC 8448 §3 の `ServerHello` バイト一致（server random・鍵交換秘密鍵を
    注入。`ServerHello` 組み立て・key_share 公開鍵導出・拡張順序に対する
    独立した正しさの根拠）
  - 公開 API のみで組んだ最小クライアントによる完全なハンドシェイクの
    往復（server Finished の verify_data 独立再計算による検証を含む）と、
    application data・`close_notify` の往復
  - HelloRetryRequest の 1 回限りの往復（`key_share` 無し CH1 → HRR →
    `key_share` 有り CH2 → Accept）
  - ダミー CCS の受理・上限超過拒否・時期外拒否
  - 状態外メッセージ（`ApplicationData` 早期受信）・解析失敗
    （壊れた `ClientHello`）の fail-closed 拒否
  - fatal alert の 1 回限り送出・以後の poison
  - 改ざんした暗号文の `bad_record_mac` 拒否
  - loopback `TcpStream` での読み取りタイムアウト（alert を送らずに閉じる）
  - `TlsServerConfig::new` の公開鍵不一致拒否

`cargo fmt --all -- --check`・`cargo clippy --workspace --all-targets --
-D warnings`（wire-server）・既存 TLS 関連テスト（`tls_x509.rs`・
`tls_transcript_finished_rfc8448.rs`・`tls_ed25519_rfc8032.rs`・
`tls_record_protection_rfc8448.rs` 等）はすべて green のまま。
