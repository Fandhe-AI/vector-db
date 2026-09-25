# SSLRequest への 'S' 応答と TLS 層の wire 接続結線 実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #966・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）の分解 15/20。#952〜#965 で
レコード層・ハンドシェイク codec・`ClientHello` 受理判定・X25519・
HKDF/鍵スケジュール・AES-GCM・レコード保護・Ed25519・PEM/PKCS#8・X.509・
transcript/Finished・サーバー側状態機械（`tls::server_handshake`）が
そろっていたが、実接続にはまだつながっていなかった。本 Issue で
`SSLRequest` へ `'S'` を返しハンドシェイクを実行、以後の pg wire
メッセージを TLS レコード上で送受信するところまでを結線した。

## ストリーム抽象 `WireStream`

認証後の接続ハンドラ（`handshake`・`simple_query`・`extended_query`・
`copy`・`protocol_dispatch`）は従来すべて `&mut TcpStream` を直接受け取る
関数群だった。これを平文・TLS の両方で動かすため、新設
`wire_stream::WireStream`（`Read + Write` + タイムアウト設定・shutdown・
緊急応答用複製・終了処理）を定義し、各関数を `<S: WireStream>` の
ジェネリクスへ一般化した（trait object ではなく静的ディスパッチ。呼び
出し側は常にどちらの型か静的に分かっているため、`dyn` と混在させる必要が
ない）。`TcpStream: WireStream` の実装は既存の OS 呼び出しへ 1:1 で委譲し、
`graceful_close` は no-op（平文接続のビット同一性を維持）。この一般化
自体はロジックを一切変えない（`refactor(wire)` コミットとして分離し、
既存テストが無変更のまま全件 green であることで検証した）。

`try_clone()` による緊急応答（RECOVER-6）用ソケット複製は
`WireStream::emergency_channel()` へ、`shutdown(Shutdown::Write)` は
`WireStream::shutdown_write()` へ置き換えた。

## `TlsStream`（`tls/stream.rs`）

### 読み取り方式（push 型）

`handshake::read_next_frame_header` は明示トランザクションが `Active` の
間、`WouldBlock`/`TimedOut` を受けても同じストリームで読み直す契約
（SQL-31・TASK-221）を持つ。`record::read_record`（`read_exact` を使う
pull 型）でこれを実装すると、レコード途中でタイムアウトした際に既に
読み込んだ部分バイト列が失われ、TLS ストリームの復号状態が壊れる。

そこで `record::RecordBuffer::feed`／`next_record` を使う push 型で実装
した。`inner.read` が返した生バイト列を `RecordBuffer` へ蓄積するだけ
なので、`WouldBlock`/`TimedOut`/`Interrupted` を挟んで呼び出しをまたいで
も取りこぼしがない。この契約は `tls::stream::tests::
read_recovers_after_would_block_mid_record`（極短い読み取りタイムアウト
を注入し、レコードが複数回の `read` に分かれて届く状況を再現）で固定
した。

### 書き込み・fail-closed

1 回の `write` は `TlsSession::seal_application_data` で 1 個以上の
レコードへ分割した後、1 回の `write_all` にまとめて送出する。復号失敗・
alert 解析失敗・書き込み失敗（タイムアウトを含む）はいずれもこの
`TlsStream` を `failed` 状態へ固定し、以後の read/write を即座にエラーに
する（fail-closed）。復号失敗等で `RecordError`/`TlsSessionError::
Protection` の `alert_description()` が `Some` を返す場合は、
`TlsSession::seal_fatal_alert`（本 Issue で追加。RFC 8446 §5.2 の
bad_record_mac 終了に対応）で fatal alert を 1 回だけ best-effort 送出
してから失敗を返す。

### 入力終端（EOF）の契約

`read` が正常終端の `Ok(0)` を返すのは、相手の `close_notify` を受信した
後に限る（RFC 8446 §6.1。PR #1056 レビュー指摘対応）。`close_notify`
なしで生ソケットが EOF になった場合は、受信バッファに部分レコードが
残っていれば `InvalidData`（truncation。`RecordBuffer::finish`）、
レコード境界なら `UnexpectedEof` を返し、いずれも `failed` へ固定する
（切断と正常終了・応答末尾の欠落を取り違えない）。上位の pg wire 層は
メッセージ境界での `UnexpectedEof` を平文 TCP の切断と同じく
`framing::read_typed_frame_header` で接続終了（`Ok(None)`）として静かに
扱い、メッセージ途中なら `FrameError::Truncated`（応答なし）となる。
失敗状態のため、切断後に `close_notify` や ErrorResponse は送らない。

### 緊急応答（RECOVER-6）との関係・既知の制約

`WireStream::emergency_channel` は `None` を返す。
`engine::recovery::panic_hook::EmergencyResponseRegistration` は生の
`TcpStream` へ平文の ErrorResponse を書く契約のため、TLS 接続でこれを
呼ぶと平文バイト列が TLS レコードへ混入してしまう。TLS 接続での
panic 発生時の緊急応答（RECOVER-6・観測性側）は「応答なしで切断」へ
縮退する。安全性側の abort ガード（RECOVER-5）には影響しない。engine 側
API（`TcpStream` 固定）を変更すれば解消できるが、本 Issue のスコープ外
とし、後続 Issue の候補として申し送る（起票はユーザー承認後）。

## `negotiate_startup` の二段化

- **フェーズ 1**（生の `TcpStream` 上）: `tls` 設定が `Some` の場合のみ
  `negotiate_startup_or_upgrade` を使い、`SSLRequest` を受けた時点で
  `'N'` を返さず `PreTlsOutcome::UpgradeTls` を返す。`tls` が `None` の
  場合は既存の `negotiate_startup`（`'N'` 応答）へそのまま委譲し、平文
  接続の挙動をビット単位で変えない（受入基準 2）。GSSENC は本 Issue の
  対象外のまま `'N'`。
- **TLS 昇格**（`handle_tls_upgrade`）: `'S'` を書いてから
  `perform_server_handshake` を実行する。ハンドシェイク driver
  （`perform_server_handshake_with`）は成功時もソケットの読み書き
  タイムアウトを `HANDSHAKE_READ_TIMEOUT` 定数のまま残す設計
  （`docs/design/tls-server-handshake.md` の既存設計を尊重し、driver 自体
  は変更しない）ため、呼び出し側で接続設定値（`server::accept_loop_*` が
  受理直後に設定した値。WIRE-5）を退避し、ハンドシェイク完了後に
  再適用する。これにより TLS 上でも `READ_TIMEOUT`（WIRE-4/5）が同じ値で
  働く（受入基準 4）。
- **フェーズ 2**（`TlsStream` 上）: `negotiate_after_tls` を使い、
  `SSLRequest`／`GSSENCRequest` はいずれも（初回であっても）`Protocol`
  エラーとして拒否する（受入基準 3）。PostgreSQL 本体も TLS 確立後の
  再ネゴシエーション要求を拒否する。

認証成功後の共通シーケンス（`AuthenticationOk` 以降・`post_auth_loop`）は
`run_authenticated_session<S: WireStream>` として切り出し、平文接続・TLS
接続の双方が共有する（ロジックは従来の `handle_connection_inner` から
一切変更しない）。

## pipelining 対策（CVE-2021-23214 型）の確認

`framing::read_startup_frame` は `TcpStream` を直接 `read_exact` で読み、
`BufReader` を使っていない。このため `SSLRequest` の直後に平文で送られた
バイト列は TLS レコードとして解釈され、ハンドシェイクは fail-closed で
失敗する。`tests/wire_tls_connection.rs::
pipelined_plaintext_after_ssl_request_is_not_processed_as_startup` で
固定した。

## 公開 API

- `wire_stream::WireStream`（新設）
- `tls::stream::TlsStream`（新設）
- `tls::server_handshake::TlsSession::seal_fatal_alert`（新設）
- `handshake::handle_connection_with_options`（新設。TLS opt-in を含む
  新しい公開入口。`tls: None` で既存 2 関数とビット同一）
- `server::accept_loop_with_tls`（新設。`tls: None` で
  `accept_loop_with_engine` とビット同一）
- 既存の `handle_connection_bounded`・`handle_connection_with_engine`・
  `handle_connection`（deprecated）・`accept_loop_with_limiter`・
  `accept_loop_with_engine` はシグネチャ・挙動とも無変更

## テスト

- `tls::stream::tests`（単体）: 平文往復・空バッファ書き込みの no-op・
  `WouldBlock` を挟んだレコード途中読み取りの復旧・書き込み失敗後の
  fail-closed 固定・部分レコードを残した EOF（truncation）と `close_notify`
  なしの EOF のエラー化・`close_notify` 受信後の正常終端。実ハンドシェイクを経由しない `TlsSession::
  new_for_tests`（`#[cfg(test)]` 限定）で鍵スケジュールから直接組み立てた
  `Sealer`/`Opener` を使う。
- `tests/common/tls_client.rs`（新設）: `wire_server::tls::*` の公開 API
  のみを使う最小 TLS 1.3 クライアント。`tests/tls_server_handshake.rs` の
  `TestClient`／`drive_client_handshake_over_socket` と同じ構成要素を
  独立に持つ意図的な重複（既存ファイルの大規模な相互依存を崩さずに
  新規結合テストを追加するため、本 Issue の範囲では共有モジュールへの
  一本化は行わない）。
- `tests/wire_tls_connection.rs`（新設）: TLS 未設定時の `'N'` 回帰・
  TLS opt-in 時の `'S'` → ハンドシェイク → StartupMessage → cleartext
  認証 → 簡易クエリ往復 → Terminate の完走・TLS 確立後の
  `SSLRequest`/`GSSENCRequest` 拒否・pipelining 耐性・`close_notify` なしの
  クライアント切断（Terminate の有無を問わず）が panic・エラー終了・応答送出
  なしに静かに終わることを固定。

`cargo fmt --all -- --check`・`cargo clippy -p fandhe-vector-db-wire-server
--all-targets -- -D warnings`・`cargo test -p fandhe-vector-db-wire-server`
（lib 1259 件・全結合テストファイル）はすべて green。

## 対象外（後続 sub-issue）

- CLI からの証明書・鍵読み込みと平文ポリシー（#967）。`bind_guard::
  TransportSecurity` にも触れていない
- HTTPS 表層（#968。`http/` 配下は無変更）
- 実クライアント（psql・openssl s_client 等）3 種での接続試験（#969）
- channel binding（#970）
- 緊急応答（RECOVER-6）の TLS 接続対応（engine 側 API の変更が必要。
  上記「既知の制約」参照）
