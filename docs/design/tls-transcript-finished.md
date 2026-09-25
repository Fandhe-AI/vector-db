# TLS 1.3 transcript hash と Finished 実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #964・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）の分解 13/20。既存の部品
（`tls::handshake`・`tls::client_hello`・`tls::key_schedule`・
`tls::hkdf`）の間に欠けていた次の 2 点を実装した。

1. **transcript hash**（RFC 8446 §4.4.1）: ハンドシェイクメッセージ列の
   累積 SHA-256。HelloRetryRequest を経た場合は 1 回目 ClientHello を
   合成メッセージ `message_hash` へ置き換える。
2. **Finished**（RFC 8446 §4.4.4）: `verify_data = HMAC(finished_key,
   transcript_hash)` の生成（送信側）・定数時間検証（受信側）。

`key_schedule.rs` の `finished_key` は既に導出まで実装済みで
「verify_data の計算・検証は #964 の担当」と明記されていた。

## モジュール配置

新規ファイルを 2 つ追加した（`record_protection.rs` と同じ、既存モジュールの
責務境界を崩さない流儀）。

- `tls/transcript.rs`: `Transcript`（順序検証付き累積ハッシュ・HRR 置換・
  名前付きチェックポイント）・`TranscriptStep`（内部型）・
  `ExpectedNext`・`TranscriptError`
- `tls/finished.rs`: `compute_verify_data`・`build_server_finished`・
  `verify_client_finished`・`FinishedError`（秘密値を扱うコードを
  `transcript` と視覚的に分離するため独立モジュールにした）

`mod.rs` へ `pub mod finished;`・`pub mod transcript;` の 2 行を追加した。

## `Transcript`: 更新順序の単一情報源

サーバー側で受理し得るハンドシェイクメッセージの並び

```text
ClientHello → [HelloRetryRequest → ClientHello] → ServerHello →
EncryptedExtensions → Certificate → CertificateVerify →
server Finished → client Finished
```

を、内部の `Step` 列挙体と `hrr_done: bool` フラグで表現する。
`expected_next()` が現在の状態から「次に受理できるメッセージ種別」
（`ExpectedNext`。ClientHello 直後だけ HelloRetryRequest／ServerHello の
2 択）を返し、状態機械（#965）はこれと各 `append_*` の `Result` のみを
頼りに遷移を判断する契約とする。**#965 は独自の順序表を持たない。**
`expected_next()` は `Result<ExpectedNext, TranscriptError>` を返し、
poison 済み（append・チェックポイントのいずれかが一度でも `Err` を返した後）
は `Err(OutOfOrder)` を返す。失敗済みの transcript に対して正常な次メッセージ
を返し、状態機械に継続可能と誤認させないため（PR #1033 レビュー指摘）。

各 `append_*`（`append_client_hello`／`append_hello_retry_request`／
`append_server_hello`／`append_encrypted_extensions`／
`append_certificate`／`append_certificate_verify`／
`append_server_finished`／`append_client_finished`）は入力
`&handshake::RawHandshake` の `msg_type` と現在の `step` の両方を検査し、
一致しなければ `Err(TranscriptError)` を返して以後 poison する
（`HandshakeBuffer` と同じ fail-closed 流儀。一度エラーを返したら以後の
呼び出しも `Err(OutOfOrder)` を返し続ける）。

### メッセージ本体をバッファしない設計

内部状態は `engine::crypto::sha256::Sha256`（ストリーミング・`Clone`
可）＋現在の `step` のみ。投入のたびに `RawHandshake::header` で
4 バイトヘッダだけを組み立て、ヘッダ・本文（`RawHandshake::body`）の順に
`Sha256::update` へ直接流し込む。本文をヘッダ込みの別バッファへコピー
せず、メッセージ本体そのものも保持しない。Certificate チェーンの長さに
依存しない O(1) メモリで動作する（PR #1033 レビュー指摘で全量コピーを
撤去）。

### 名前付きチェックポイント

任意時点の `current_hash()` のような汎用 getter は公開せず、
該当ステップを通過した直後にのみ取得できる 4 つのチェックポイントを
提供する（誤った時点のハッシュ取得を型・状態で防ぐ設計）。

| チェックポイント | 対応する transcript 範囲 | 用途 |
| ---------------- | ------------------------ | ---- |
| `hash_through_server_hello()` | ClientHello..ServerHello | `HandshakeSecret::traffic_secrets` |
| `hash_through_certificate()` | ClientHello..Certificate | CertificateVerify の署名対象（#961 が利用） |
| `hash_through_certificate_verify()` | ClientHello..CertificateVerify | server Finished の verify_data 算出対象 |
| `hash_through_server_finished()` | ClientHello..server Finished | `MasterSecret::application_traffic_secrets`・client Finished 検証対象 |

いずれも `step` が該当ステップと**一致する場合のみ** `Ok`（通過済みだが
先へ進んでしまった場合も含め、それ以外は `Err(TranscriptError::
OutOfOrder)`）。呼び出し順序が単純（各チェックポイントは 1 回だけ、
直後に取得する）であることを踏まえ、実装を単純にする側を選んだ。

### HelloRetryRequest 時の `message_hash` 置換（RFC 8446 §4.4.1）

`append_hello_retry_request` は次の 2 点を検証してから置換する。

1. `raw.msg_type == HandshakeType::ServerHello`（HRR は ServerHello と
   同じメッセージ型。RFC 8446 §4.1.4 の注記）
2. 本文の `random` フィールド（オフセット 2..34。legacy_version 2 バイト
   の直後）が `client_hello::HELLO_RETRY_REQUEST_RANDOM` と一致する

2 つ目の検証により、真の ServerHello を HRR と誤認して transcript を
破壊してしまう取り違えを防ぐ（`client_hello.rs` は `254`
（`message_hash`）を受信側の閉じた語彙に追加しない方針のため、この
判別は `random` フィールドでしか行えない）。

置換の実体は、現在のハッシュ状態を `clone().finalize()` して
`Hash(ClientHello1)` を取り出し、新しい `Sha256` を
`[0xFE, 0x00, 0x00, 0x20] ‖ Hash(ClientHello1)`（`msg_type=254`・
3 バイト長 `0x000020`＝32）から開始してから HRR 自体を投入する、という
2 段階。`254` は `HandshakeType` の閉じた語彙に存在しないため、
`Transcript` が生バイトとして直接書く。

HelloRetryRequest は 1 接続 1 回のみ許可し（`hrr_done` フラグ）、
2 回目の HRR・HRR 後に ServerHello 以外のメッセージが来た場合は
いずれも `Err(OutOfOrder)`（poison）とする。

## `finished.rs`: `verify_data` の計算・検証

- `compute_verify_data(finished_key: &Secret32, transcript_hash: &[u8; 32])
  -> Secret32`: `HMAC-SHA-256(finished_key, transcript_hash)`
  （`hkdf::hmac_sha256` を単一引数で呼ぶだけ）。結果は比較が終わるまで
  秘密扱いのため `Secret32`（Drop で best-effort ゼロ化）で返す。
- `build_server_finished(server_hs_traffic: &TrafficSecret, th_ch_cv: &[u8;
  32]) -> Result<handshake::Finished, FinishedError>`:
  `TrafficSecret::finished_key()` → `compute_verify_data` → `Finished`。
- `verify_client_finished(client_hs_traffic: &TrafficSecret, th_ch_sf: &[u8;
  32], received: &handshake::Finished) -> Result<(), FinishedError>`:
  期待値を計算し `hkdf::ct_eq`（GCM タグ検証と共有する唯一の定数時間
  比較）でのみ判定する。長さ不一致も `ct_eq` が `false` を返すため、
  最終判定以外に受信値のビットへ依存する分岐は無い。

`FinishedError` は受信値・期待値・鍵のバイト列を一切保持しない
（`VerifyDataMismatch`・`Hkdf(HkdfError)` の 2 バリアントのみ）。
`alert_description()` は `VerifyDataMismatch` を
`AlertDescription::DecryptError`（51）へ写す——RFC 8446 §4.4.4 の
実装注記どおり、TLS 1.3 では歴史的な `bad_record_mac` ではなく
`decrypt_error` を用いる。`record.rs` の `AlertDescription`
（`#[non_exhaustive]`）へ `DecryptError` を追加した（既存 9 値は無変更）。

`Hkdf(HkdfError)` は `AlertDescription::InternalError`（80）——呼び出し
契約違反（内部起因）を表す既存の方針（`hkdf.rs`・`record_protection.rs`
と同じ）をそのまま踏襲する。

`wire_code`（ERR-1／ERR-2／ERR-4）への写像は追加しない。TLS 層の失敗は
alert と切断で表す方針を他の TLS モジュールから引き継ぐ。alert の
実送出・状態機械への結線は `#965` の担当。

## テストの出典と非 vacuous 性

### 単体テスト（`transcript.rs`・`finished.rs` 内）

- RFC 8448 §3（Simple 1-RTT Handshake。IETF の公開文書由来。`docs/spec`
  の内容ではない）の ClientHello・ServerHello から
  `hash_through_server_hello()` を計算し、トレース記載値
  （`860c06ed…`）と一致することを固定（`transcript.rs`）。
- 順序外 append（ClientHello の前の ServerHello・ServerHello 前の
  Certificate チェックポイント要求）・期待外 `msg_type`（ClientHello
  位置への Finished）・完了後の append・HRR の 2 回目・random 不一致の
  ServerHello を HRR として渡した場合が、いずれも `Err` かつ以後
  poison され続けることを固定（`transcript.rs`）。
- RFC 8448 §3 の finished key（`008d3b66…`／`b80ad010…`。
  `key_schedule.rs` のテストが `TrafficSecret::finished_key` 経由で
  導出済みと確認している値）と th_ch_cv／th_ch_sf から
  `compute_verify_data` がトレースの server／client Finished
  （`9b9b141d…`／`a8ec436d…`）と一致することを固定（`finished.rs`）。
  th_ch_cv（`edb7725f…`）は RFC 8448 に直接のラベルが無いため、
  Certificate/CertificateVerify までのバイト列から独立に SHA-256 で
  算出した値（`Transcript` のインクリメンタル計算との一致は結合
  テストが検証する）。
- 1 ビット反転・全ゼロ・長さ違いの verify_data がいずれも不一致になる
  こと、`Debug`／`Display` に秘密バイトが含まれないことを固定
  （`finished.rs`）。

### 結合テスト `tests/tls_transcript_finished_rfc8448.rs`（公開 API のみ）

- `rfc8448_simple_1rtt_transcript_and_finished_match_trace`: RFC 8448
  §3 の X25519 秘密鍵から実際に鍵交換を行い、
  X25519 → Early secret → Handshake secret → `Transcript`
  （ClientHello→ServerHello→EncryptedExtensions→Certificate→
  CertificateVerify→server Finished→client Finished）の各チェックポイント
  → `finished::build_server_finished`／`verify_client_finished` →
  Master secret → application traffic secret、という一連の流れを
  1 本の呼び出し列として固定する（受け入れ条件 A1）。server Finished の
  送信バイト列（ヘッダ込み）・client Finished の検証結果・改ざんした
  client Finished が `DecryptError` へ写像されることまで確認する。
- `rfc8448_hello_retry_request_transcript_message_hash_matches_trace`:
  RFC 8448 §5（HelloRetryRequest。IETF の公開文書由来）の
  ClientHello1・HelloRetryRequest・ClientHello2・ServerHello の実バイト
  列から `Transcript` を構成し、`hash_through_server_hello()` が
  トレース記載の「derive secret "tls13 c hs traffic"」の `hash`
  フィールド値（`8aa8e828…`）と一致することを固定する（受け入れ条件
  A2）。この値は `message_hash` 置換が正しく行われた場合にのみ一致する
  ため、置換ロジックの非 vacuous な検証になる。

  RFC 8448 §5 は ECDHE に P-256 を使い、本リポの X25519 実装では鍵交換
  そのものは再現できないため、finished_key を用いた計算の検証
  （HRR 版 server／client verify_data）は `finished.rs` の crate 内
  単体テストへ役割分担せず——`TrafficSecret` はコンストラクタを公開
  していないため——`compute_verify_data_matches_rfc8448_*`
  （1-RTT ケース）で `compute_verify_data` 自体の正しさを固定するに
  留め、本結合テストは `Transcript` の message_hash 置換の正しさに
  範囲を絞った。

## 対象外・申し送り

- alert の実送出・切断・状態機械本体（#965）: `Transcript::
  expected_next()`／`append_*` と `FinishedError::alert_description()`
  を呼ぶ側
- CertificateVerify の署名（#961）: `hash_through_certificate()` を利用
- 接続への TLS 層挿入（#966・#968）、`tls-server-end-point`（#970）
- クライアント証明書・PSK／セッション再開（`res master`）・0-RTT・
  KeyUpdate（親 Issue #941 の方針により対象外）
