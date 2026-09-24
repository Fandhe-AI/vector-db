# HKDF と TLS 1.3 鍵スケジュール 実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #956・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）が対象とする唯一の暗号
スイート `TLS_AES_128_GCM_SHA256` は、鍵導出にハッシュ SHA-256 を
固定で用いる。本 Issue は X25519 の共有秘密（Issue #955）を入力にして、
TLS 1.3 鍵スケジュール（RFC 8446 §7.1）によりハンドシェイク／
アプリケーションの traffic secret と AEAD の key／iv を導出する層を
実装する（`crates/wire-server/src/tls/hkdf.rs`・`key_schedule.rs`）。
transcript hash の蓄積・Finished の verify_data 計算（#964）、
レコード保護（#959）、alert の実送出・状態機械への結線（#965・#966
以降）はいずれも対象外。

## SHA-256 の再利用

自作 SHA-256 は元々 `engine::recovery::content_hash` が台帳の内容
照合ハッシュ専用に非公開実装していた（TASK-101・RECOVER-10。
Issue #399 でストリーミング化）。本 Issue で `crates/engine/src/sha256.rs`
として切り出し `pub mod sha256;` で公開し、wire-server から
`engine::sha256::{Sha256, digest, DIGEST_LEN, BLOCK_LEN}` を利用する
（外部クレート追加なし・ハッシュ入力バイト列のレイアウト・既存
content_hash 値は不変）。wire-server 側に SHA-256 を再実装すると
「再利用」の要件に反しコードも重複するため、この切り出しを採る。

## HKDF 層（`tls/hkdf.rs`）

- `hmac_sha256(key, data: &[&[u8]]) -> [u8; 32]`: RFC 2104 の HMAC。
  `data` をマルチパートで受け取り連結用のヒープ確保をしない。鍵が
  ブロック長（64 バイト）を超える場合は先に SHA-256 で縮める（鍵長は
  呼び出し時点の公開値のため、この分岐は秘密値に依存しない）。
- `hkdf_extract` / `hkdf_expand`: RFC 5869 の Extract/Expand。
  `hkdf_expand` は出力長 `L > 255 * HashLen` を `Err(OutputTooLong)` で
  fail-closed に拒否し、`L = 0` は RFC が明示的に禁止していないため
  空書き込みとして受理する。
- `hkdf_expand_label` / `derive_secret`: RFC 8446 §7.1 の
  `HKDF-Expand-Label`／`Derive-Secret`。`HkdfLabel` は固定長スタック
  バッファ（最大 514 バイト）へ長さ検証後にのみ書き込む。`derive_secret`
  は `Messages` の蓄積（#964 の担当）を前提にせず、呼び出し側が既に
  計算済みの transcript hash（`&[u8; 32]`）を受け取る形に留める。
- `Secret32`: HKDF が受け渡す 32 バイト秘密値。`Clone`／`Copy` 非導出・
  `Debug` 秘匿・Drop で best-effort ゼロ化（`unsafe` 不使用のため
  最適化による消去省略までは保証しない。[`tls/x25519.rs`] の
  `SharedSecret` と同じ限界）。
- `HkdfError`: `OutputTooLong`／`InvalidLabel`／`ContextTooLong` の
  3 種。いずれも呼び出し契約違反（内部起因）のため
  `alert_description()` は `internal_error`（80）へ写す。alert の実
  送出は #965 の担当。

## 鍵スケジュール層（`tls/key_schedule.rs`）

RFC 8446 §7.1 の鍵スケジュール図のうち、親 Issue #941 の方針
（PSK・0-RTT・セッション再開なし）が対象とする経路のみを、`self` を
消費して次段へ遷移する型状態 API として実装する（誤用防止は
[`tls/x25519.rs`] の `EphemeralSecret::diffie_hellman(self)` と同じ
流儀）。

```text
EarlySecret::new_without_psk()
  └─ into_handshake(&SharedSecret) -> HandshakeSecret
       ├─ traffic_secrets(&th_ch_sh) -> { client, server: TrafficSecret }
       └─ into_master() -> MasterSecret
            └─ application_traffic_secrets(&th_ch_sf)
                 -> { client, server: TrafficSecret }

TrafficSecret::traffic_keys() -> TrafficKeys { key: [u8; 16], iv: [u8; 12] }
TrafficSecret::finished_key() -> Secret32
```

- `EarlySecret::new_without_psk`: `HKDF-Extract(salt = 0, IKM = 0)`
  （salt・IKM とも 32 バイトの 0 埋め。空 salt と 32 バイトの 0 salt は
  HMAC の鍵パディング規則により同一の PRK を生むため、RFC 8446 の
  図の "0" 表記どおりの値になる。`tls/hkdf.rs` のテストで機械検証済み）。
- `into_handshake`: `derived = Derive-Secret(early, "derived",
  Hash(""))` の後 `Extract(derived, ecdhe)`。
- `into_master`: `derived = Derive-Secret(hs, "derived", Hash(""))` の
  後 `Extract(derived, 0)`。
- `exp master`／`res master`（エクスポーター・再開用の secret）は
  親 Issue #941 の方針（セッション再開なし）により対象外。
- `TrafficKeys`: `TLS_AES_128_GCM_SHA256` の鍵長 16 バイト・iv 長
  12 バイト固定。`Debug` 秘匿・Drop で best-effort ゼロ化。

## 定数時間性

- HMAC・HKDF・SHA-256 のラウンド数・ブロック数・出力長は、鍵長・
  データ長・`L` といった呼び出し時点で確定する公開値にのみ依存し、
  秘密値のビットに依存する分岐・ループ回数は持たない。
- 秘密値を添字にしたテーブル参照は行わない（HMAC のラウンド定数
  参照は `engine::sha256` 側でラウンド番号 `t` という公開値のみを
  添字にする。`tls/x25519.rs` と同じ設計方針）。
- 秘密値どうしの比較（Finished 検証等）は本 Issue では実装しない。
  定数時間比較は #964 が別途用意する。

## 検証

RFC の公開テキストから正確に転記した値で機械検証した（記憶からの
再構成ではない）:

- RFC 5869 Appendix A.1〜A.3（HKDF-Extract/Expand・SHA-256。A.4〜A.7 は
  SHA-1 のため対象外）
- RFC 4231 §4.2・§4.3・§4.8（HMAC-SHA-256。§4.8 は鍵・データとも
  ブロック長超の Test Case 7）
- RFC 8448 §3（Simple 1-RTT Handshake）: ECDHE 共有秘密（IKM）・
  Early／Handshake／Master の各 secret・client/server の handshake
  traffic secret・application traffic secret・各 traffic key/iv・
  server finished key が記載値と一致することを `tls/key_schedule.rs`
  の単体テストで固定
- `crates/wire-server/tests/tls_key_schedule_rfc8448.rs`（公開 API の
  みを使った結合テスト）: X25519 鍵交換 → 鍵スケジュール一連の呼び出し
  列に加え、ClientHello‖ServerHello（RFC 8448 記載バイト列）の
  `engine::sha256::digest` が記載の transcript hash と一致すること
  （ブロック境界をまたぐ実データでの `engine::sha256` 公開 API 検証を
  兼ねる）

## 残余リスク・申し送り（#971 監査への申し送り）

- ゼロ化は best-effort（`unsafe`／`write_volatile` を使わないため、
  コンパイラ最適化により消去が省略されない保証はない）。
- X25519 の秘密型（`EphemeralSecret`・`SharedSecret`）自体には
  ゼロ化を後付けしていない（Issue #955 のスコープのまま。本 Issue の
  対象外）。
- `exp master`／`res master`（エクスポーター・再開）は未実装。
