# ADR: SCRAM チャネルバインディング用 `tls-server-end-point` の実装既定値

- ステータス: Proposed（署名アルゴリズム別ハッシュ選択・TLS 未確立時 `None`
  の 2 点は実装・テストで確定済み。`PLUS` 提示既定 `false` は下記実測に
  基づく実装判断であり、オーナー確認待ち）
- 対応: TASK-228・WIRE-9・WIRE-18・HTTP-10 ポインタ（Issue #970・親 #941）
- 関連: `docs/design/tls-server-handshake.md`・`docs/design/tls-scram-design.md`

## 背景

SCRAM-SHA-256-PLUS（RFC 5802 の `-PLUS` 機構・[WIRE-18] ポインタ）は
`gs2-cbind-flag` に `p=<cb-name>` を伴い、クライアント・サーバー双方が
同一のチャネルバインディング値を算出できることを要求する。本サーバーは
`tls-server-end-point`（RFC 5929 §4）のみを対象とし、算出は
[`crates/wire-server/src/tls/channel_binding.rs`] の
`tls_server_end_point` が担う。

## 決定

### 1. ハッシュ関数の選択（署名アルゴリズム別）

RFC 5929 §4.1 は「証明書の署名に使われたハッシュ関数（MD5・SHA-1 は
SHA-256 へ格上げ）で証明書 DER 全体をハッシュする」ことを要求する。
本実装は `signatureAlgorithm`（`Certificate` 外側 `SEQUENCE` の 2 番目
フィールド）の OID から表引きし、表に無い OID は `None` へ縮退させる
（推測で別のハッシュを当てない fail-closed 判断）。

Ed25519（OID `1.3.101.112`。RFC 8410 §3）は RFC 5929 が定義した当時には
存在せず単一の「署名ハッシュ」を持たない署名方式だが、本サーバーが受理
する唯一の葉鍵種別であるため、SHA-256 を割り当てる決定を本リポの
実装既定値として行った（`crates/wire-server/tests/tls_channel_binding.rs`
の RFC 8410 §10.2 公開テストベクタで、OpenSSL `x509 -fingerprint -sha256`
の独立算出値との一致を固定）。

### 2. TLS 未確立時は `None`

`crate::wire_stream::WireStream::tls_server_end_point` は TLS 未確立の
平文接続では `None` を返す。チャネルバインディングを要求しない
SCRAM-SHA-256（無印）はこの値を一切参照しない。

### 3. SCRAM-SHA-256-PLUS の機構提示は既定で無効

`TlsServerConfig::with_scram_channel_binding`（既定 `false`）を新設し、
`TlsServerConfig::new` はチャネルバインディング値の算出のみ起動時に
1 回行い、機構リストへの `PLUS` 提示可否は別フラグとして分離した。

**根拠（実測）**: `crates/wire-server/tests/wire_scram_plus_psql_interop.rs`
（手動専用・`#[ignore]`。CI 非配線）で psql 18.6・OpenSSL 3.5.5 を用い、
`sslmode=require` かつ `channel_binding` を `disable`／`prefer`／`require`
と変えた 3 通りを、`PLUS` 提示（`advertise_scram_channel_binding`）の
有効・無効それぞれについて本サーバー（Ed25519 葉証明書）に対して実測
した。いずれの組み合わせでも TLS ハンドシェイク自体は成立する
（本フラグは TLS 確立後の SASL 機構リストのみを変えるため）。差は
その後の SCRAM 交換（tls-server-end-point の算出）に現れる:

| `PLUS` 提示 | `disable` | `prefer` | `require` |
| ----------- | --------- | -------- | --------- |
| 無効（既定） | 成功 | 成功 | libpq がクライアント側で拒否（"channel binding is required, but server did not offer ..."）。サーバーが `PLUS` を提示しないため |
| 有効（opt-in） | 成功 | 失敗（`could not find digest for NID UNDEF`） | 失敗（`could not find digest for NID UNDEF`） |

`PLUS` 提示が有効な場合の `prefer`／`require` 失敗は、libpq が
SCRAM-SHA-256-PLUS を選び tls-server-end-point 用のダイジェストを
算出しようとした際に、本サーバーが受理する唯一の葉鍵種別である
Ed25519 の署名アルゴリズムに対応するダイジェストを libpq 側が解決
できないために起きる（TLS ハンドシェイクの失敗ではない）。libpq
側の具体的な実装箇所の特定は本 Issue の対象外（下記「スコープ外」）
とする。

この実測結果に基づき、`PLUS` 機構を既定で提示しないことで、libpq の
既定設定（`channel_binding=prefer`）を使う一般的なクライアントが
（`disable`・`prefer` いずれでも）認証成功する状態を維持する。`PLUS`
を使いたい運用（`channel_binding=require` かつクライアント側が本
サーバーの制約を把握している場合）は
`TlsServerConfig::with_scram_channel_binding(true)` の opt-in で有効化
できる。CLI からの結線は #967 の担当範囲。

## 受け入れ基準への対応

- 署名アルゴリズム別のハッシュ選択（SHA-256／SHA-512。非対応は `None`）:
  `channel_binding.rs::classify_signature_oid`・単体テストで固定
- TLS 未確立なら `None`（`p=tls-server-end-point` 非提示・無印 SCRAM の
  みへ縮退）: `wire_stream.rs`・`tls/stream.rs` 経由で固定
- 既知の固定値に対する単体テスト: `tls_channel_binding.rs`
  （合成証明書・RFC 8410 §10.2 公開テストベクタ双方）

## スコープ外

- CLI からの `--scram-channel-binding` 相当のフラグ結線（#967）
- HTTPS 表層（NoSQL・HTTP-10）へのチャネルバインディング適用
- libpq 以外のクライアント（psycopg・node-postgres 等）での相互運用実測
- `PLUS` 提示 opt-in 時に libpq 側の制約を回避する追加実装（証明書の
  署名アルゴリズムを libpq が解決可能な形へ変更する等）
- libpq 側の失敗箇所の特定（`fe-secure-openssl.c` 等の追跡）そのもの
- `PLUS` 提示既定 `false` の最終確定（オーナー承認）

## 参照

- RFC 5929 §4（`tls-server-end-point`）
- RFC 5802（SCRAM）・RFC 8410 §3（Ed25519 OID）
- `crates/wire-server/src/tls/channel_binding.rs`
- `crates/wire-server/src/tls/server_handshake.rs`
  （`TlsServerConfig::with_scram_channel_binding`）
- `crates/wire-server/tests/tls_channel_binding.rs`
- `crates/wire-server/tests/wire_scram_plus_tls.rs`
- `crates/wire-server/tests/wire_scram_plus_psql_interop.rs`（手動専用）
