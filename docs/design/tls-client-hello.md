# ClientHello 拡張の解析と TLS 1.3 以外の拒否

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #954・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）は #953 で `ClientHello` の
**構造（シンタックス）**の parse を実装済みだが、拡張は不透明な
`(extension_type, extension_data)` 列のまま保持され、意味の解釈も受理
判定もまだ無かった。本 Issue はその上に、拡張の意味解釈・受理判定・
HelloRetryRequest（HRR）の組み立てを担う純粋関数層
（`crates/wire-server/src/tls/client_hello.rs`）を追加する。

## 責務境界

- 本 Issue: **状態を持たない**判定関数（`negotiate`）と HRR の構築
  （`build_hello_retry_request`）のみ
- #965（状態機械・alert 送出）: 「HRR を送ったか」の状態保持と alert の
  実送出・切断。「HRR は 1 回のみ」の判定は本モジュールが単一情報源として
  持ち、`after_hrr: bool` を引数で受け取るだけで #965 側は再実装しない
- #964（transcript hash）: HRR 時の `message_hash` 置換
- #955（X25519）: 共有秘密の計算・全ゼロ検出。本モジュールは client
  公開鍵 32 バイトを取り出して渡すだけ
- #959／#965（0-RTT）: `early_data` を無視した場合の早期
  application_data レコードの破棄。本モジュールは PSK／0-RTT を受理せず、
  `pre_shared_key`・`psk_key_exchange_modes` は RFC 8446 §4.2.11・§4.2.9
  の MUST（位置・併存）だけを検査し中身は解釈しない
- 通常の（HRR でない）`ServerHello` の組み立て（サーバー鍵を含む）は
  #965 が #955 の鍵生成と組み合わせて行う

## 判定順序（単一情報源）

`negotiate` 内の判定順序は以下で固定し、`sql::exec` 同様にこの関数以外に
同じ判定を再実装しない:

1. 拡張ブロック全体の走査（重複拡張の検出・`pre_shared_key` の位置検査・
   対象 5 拡張の構造解析。違反はそれぞれ `illegal_parameter`／
   `missing_extension`／`decode_error`）
2. バージョン（`supported_versions` に TLS 1.3 が無ければ
   `protocol_version`。暗号スイート・署名より先に判定し、TLS 1.2 以前の
   クライアントに `handshake_failure` を返さないため）
3. compression（`legacy_compression_methods != [0x00]` は
   `illegal_parameter`）
4. 暗号スイート（`TLS_AES_128_GCM_SHA256` が無ければ `handshake_failure`）
5. 署名アルゴリズム（`signature_algorithms` 欠落は `missing_extension`、
   ed25519 非提示は `handshake_failure`）
6. グループ／鍵共有（`supported_groups`・`key_share` の片方欠落は
   `missing_extension`、x25519 が無く `supported_groups` にはある場合は
   `after_hrr` に応じて HRR 要求／`handshake_failure` を切り替える）

## 重複拡張検出のデータ構造

`extension_type`（u16）ごとに 1 bit を持つ `[u64; 1024]`（65536 bit・
8 KiB）のビットマップで重複を検出する。拡張数が多くても O(n) で判定でき、
未知の type を大量に並べた入力に対する O(n²) 走査（DoS 耐性）を避ける。

## 無視する拡張・受理しないもの

- 未知の拡張（GREASE 値・`early_data` を含む）は中身を見ずに無視する
- PSK／0-RTT は受理しない（`pre_shared_key`・`psk_key_exchange_modes` の
  構造制約のみ検査し、値は解釈しない）
- `legacy_version` は RFC 8446 §4.2.1 に従い交渉に使わず検査もしない

## HelloRetryRequest の組み立て

`build_hello_retry_request` は RFC 8446 §4.1.3・§4.1.4・§4.2.8 に従い、
`random = HELLO_RETRY_REQUEST_RANDOM`（SHA-256("HelloRetryRequest")の
固定値）・`cipher_suite = TLS_AES_128_GCM_SHA256`・拡張
`[supported_versions: 03 04, key_share: 00 1d]` を持つ `ServerHello` を
返す。送出と transcript への反映は #965／#964 が担う。

## テストの注意（RFC 8448 §3 ベクタ）

RFC 8448 §3 の `ClientHello`（`handshake.rs` の既存テストで使用）の
`signature_algorithms` は ed25519(0x0807) を含まない。それ以外の条件
（TLS 1.3・`TLS_AES_128_GCM_SHA256`・x25519 の `key_share`／
`supported_groups`・compression `[0]`）はすべて満たすため、**無改変の
このベクタは負のテスト（`handshake_failure`）であり、正常系ではない**。
正常系のテストは、このベクタの `signature_algorithms` へ 0x0807 を
追加した派生を使う（`with_ed25519_sig_alg`）。

## セキュリティ考慮事項

- 受信データ（`ClientHello` は完全に untrusted）はすべて `Reader`
  （`handshake.rs` と共有する fail-closed カーソル）経由で読み、
  `unwrap`／`expect`／添字アクセスは使わない
- ClientHello の全フィールドは平文で流れる公開値であり、秘密値に依存する
  分岐・テーブル参照は無い。client x25519 公開鍵はコピーするだけ
- TLS 1.3（0x0304）以外は `protocol_version` で拒否し、TLS 1.2 以前への
  フォールバック経路を持たない。暗号スイート・グループ・署名方式は
  それぞれ 1 つに固定し交渉の幅を持たない
- エラー理由は英語の固定文字列（`&'static str`）のみで、受信バイト列
  （SNI のホスト名等）はエラーへ含めない。`server_name` は保持するだけで
  出力しない
