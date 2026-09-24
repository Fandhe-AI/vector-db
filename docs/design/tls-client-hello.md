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
  実送出・切断。本モジュールは 1 回目の `ClientHello`（`Option`。HRR
  未送出なら `None`）を引数で受け取るだけで状態は持たない。「HRR は
  1 回のみ」の判定・2 回目 `ClientHello` が 1 回目と（許可された差分を
  除き）同一であることの検証（RFC 8446 §4.1.2）はいずれも本モジュールが
  単一情報源として持ち、#965 側は再実装しない
- #964（transcript hash）: HRR 時の `message_hash` 置換
- #955（X25519）: 共有秘密の計算・全ゼロ検出。本モジュールは client
  公開鍵 32 バイトを取り出して渡すだけ
- #959／#965（0-RTT）: PSK を受理しない結果としての早期 application_data
  レコードの破棄。本モジュールは PSK／0-RTT を受理しないが、これは
  `pre_shared_key`・`psk_key_exchange_modes`・`early_data` の中身の
  「意味」を解釈しない（PSK を選択しない・ticket を検証しない）という
  意味であり、構造検証はすべて行う（「無視する拡張・受理しないもの」節
  参照）
- 通常の（HRR でない）`ServerHello` の組み立て（サーバー鍵を含む）は
  #965 が #955 の鍵生成と組み合わせて行う

## 判定順序（単一情報源）

`negotiate` 内の判定順序は以下で固定し、`sql::exec` 同様にこの関数以外に
同じ判定を再実装しない:

1. 拡張ブロック全体の走査（重複拡張の検出・`pre_shared_key` の位置検査・
   対象 5 拡張の構造解析。違反はそれぞれ `illegal_parameter`／
   `missing_extension`／`decode_error`）
2. HRR 後の 2 回目 `ClientHello` の同一性検証（`after_hrr` が `Some` の
   ときのみ。[`check_hrr_consistency`](../../crates/wire-server/src/tls/client_hello.rs)。
   違反は `illegal_parameter`）。他のどの意味検査よりも先に行う。後段の
   検査を先に走らせると、同一性違反があっても別の alert 種別（`missing_
   extension`・`handshake_failure` 等）で応答してしまい、RFC 8446 §4.1.2
   が要求する `illegal_parameter` を返せなくなるため
3. `legacy_version`（0x0303 固定でなければ `protocol_version`。RFC 8446
   §4.1.2 の MUST。TLS 1.0/1.1 の legacy_version（0x0301/0x0302）を送る
   旧クライアントもここで `protocol_version` になるが、そうした
   クライアントは `supported_versions` 自体も欠くため、この判定が無くても
   次のバージョン判定で同じ `protocol_version` に到達する（分類結果は
   変わらない））
4. バージョン（`supported_versions` に TLS 1.3 が無ければ
   `protocol_version`。暗号スイート・署名より先に判定し、TLS 1.2 以前の
   クライアントに `handshake_failure` を返さないため）
5. compression（`legacy_compression_methods != [0x00]` は
   `illegal_parameter`）
6. 暗号スイート（`TLS_AES_128_GCM_SHA256` が無ければ `handshake_failure`）
7. 署名アルゴリズム（`signature_algorithms` 欠落は `missing_extension`、
   ed25519 非提示は `handshake_failure`）
8. `server_name` の構造検証（HRR で戻る経路でも必ず実行する。Accept
   到達後まで遅延させると、不正な `server_name` を含む `ClientHello` に
   対して誤って `RetryRequestX25519` を返しうるため）
9. グループ／鍵共有（`supported_groups`・`key_share` の片方欠落は
   `missing_extension`）
10. x25519 が無く `supported_groups` にはある場合は `after_hrr` に応じて
    分岐する。1 回目（`after_hrr` が `None`）なら HRR 要求
    （`RetryRequestX25519`）。1 回目に x25519 自体を誰も提示していない
    （鍵交換グループの不合意）なら `handshake_failure`。HRR 後（`after_hrr`
    が `Some`）に要求したグループの key_share を含めなかった場合は、鍵交換
    の不合意ではなくクライアントのプロトコル違反であるため
    `illegal_parameter`（RFC 8446 §4.2.8）

## 重複拡張検出のデータ構造

`extension_type`（u16）ごとに 1 bit を持つ `[u64; 1024]`（65536 bit・
8 KiB）のビットマップ（`SeenU16Set`）で重複を検出する。拡張数が多くても
O(n) で判定でき、未知の type を大量に並べた入力に対する O(n²) 走査
（DoS 耐性）を避ける。`key_share` 内の `NamedGroup` の重複（x25519 に
限らず全グループ対象）・`server_name` 内の `name_type` の重複
（`host_name` に限らず全 name_type 対象）も同じデータ構造で検出する。

## HelloRetryRequest 後の 2 回目 ClientHello の検証

`negotiate` は `after_hrr: Option<&handshake::ClientHello>` で 1 回目の
`ClientHello` を受け取る（`None` は 1 回目・`Some` は 2 回目。呼び出し元
の #965 が「HRR を送ったか」の状態と共に 1 回目を保持し、2 回目の呼び出し
へそのまま渡す。本モジュール自体は状態を持たない）。`Some` のときは
RFC 8446 §4.1.2 に従い次を検証する:

- `legacy_version`・`random`・`legacy_session_id`・`cipher_suites`・
  `legacy_compression_methods` が 1 回目と完全一致
- `key_share`・`early_data`・`pre_shared_key`・`padding`（RFC 8446 §4.1.2
  が列挙する例外の 4 種）を除く拡張が、型・値ともに 1 回目と完全一致。
  `psk_key_exchange_modes` はこの例外一覧に含まれないため対象外（値の
  更新は許可されていない）。`cookie` も「HRR が提供していた場合にのみ
  追加してよい」対象だが、本サーバーの HRR は `cookie` を送出しないため
  実質的に到達しない
- `key_share` は HRR が要求したグループ（x25519）のみを含み、他のグループ
  のエントリが 1 件でもあれば `illegal_parameter`
- `early_data` は「比較除外＝自由に追加・維持してよい」対象ではない、
  RFC 8446 §4.1.2 の 4 例外中で唯一の非対称な拡張。0-RTT は HRR 後には
  許可されないため、1 回目の有無に関わらず 2 回目の `ClientHello` に
  存在すること自体が `illegal_parameter`（削除のみが許可された差分）

いずれの違反も `illegal_parameter` として拒否する。

## 無視する拡張・受理しないもの

- **未知の拡張**（対象 5 拡張・`pre_shared_key`・`psk_key_exchange_modes`・
  `early_data` のいずれでもない拡張。GREASE 値を含む）は
  `extension_type`／`extension_data` の中身を一切見ずに無視する
  （`parse_extensions` の `_ =>` 分岐）
- `early_data` は未知の拡張ではなく、`parse_extensions` が構造まで検証する
  **既知の拡張**である（下記）
- PSK／0-RTT そのものは受理しない（受理判定に使わない・0-RTT データを
  送出しない）。ただし「受理しない」は「構造を見ない」という意味ではない
  （codex-review PR #1022 P0・P2 指摘。以下は `parse_extensions` が
  `pre_shared_key`／`psk_key_exchange_modes`／`early_data` それぞれについて
  行う構造検証。値の意味解釈（PSK の選択・ticket の検証）はいずれも
  行わない）:
  - `pre_shared_key`（RFC 8446 §4.2.11）: `identities<7..2^16-1>`・
    `binders<33..2^16-1>` の 2 ベクタを最後まで読み進め、各エントリの
    境界（`identity<1..2^16-1>`・`obfuscated_ticket_age` 4 バイト・
    `binder<32..255>`）・`identities` と `binders` の件数一致・拡張全体の
    終端（余剰バイト無し）を検証する（`parse_pre_shared_key`）。加えて
    最後の拡張であること（§4.2.11 の MUST）・`psk_key_exchange_modes` の
    併存（§4.2.9 の MUST）も検査する
  - `psk_key_exchange_modes`（RFC 8446 §4.2.9）: `ke_modes<1..255>` の
    ベクタ境界・終端を検証する（`validate_psk_key_exchange_modes`）。
    個々の `PskKeyExchangeMode` 値は解釈しない（拡張可能な列挙のため）
  - `early_data`（RFC 8446 §4.2.10）: ClientHello 内の本体は `Empty` 型
    （0 バイト）でなければならないことを検証する
    （`validate_client_hello_early_data`）。加えて `pre_shared_key` との
    併存（§4.2.10 の MUST）も検査する
- `server_name` の `host_name`（name_type=0）は RFC 6066 §3 の DNS
  ホスト名構文（ASCII・全長 1..=253 バイト・末尾ドット禁止・IPv4/IPv6
  literal 禁止・各ラベル 1..=63 バイト・空ラベル禁止・英数字とハイフンの
  みで先頭/末尾ハイフン禁止）まで検証してから
  `NegotiatedClientHello::server_name` へ渡す（`validate_host_name`）

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
