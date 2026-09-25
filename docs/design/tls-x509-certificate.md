# X.509 DER の最小パースと Certificate メッセージ 実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #963・親 Issue #941

## 背景

自作 TLS 1.3 サーバー（親 Issue #941）は、運用者が用意した証明書チェーン
（[`docs/design/tls-pem-pkcs8.md`](./tls-pem-pkcs8.md)・Issue #962 が返す DER
列）を起動時に最小限パースして自己整合性を検査し、`Certificate` ハンドシェイク
メッセージ（RFC 8446 §4.4.2。`crates/wire-server/src/tls/handshake.rs`・
Issue #953 で不透明バイト列として実装済み）へ組み立てる必要がある。本 Issue は
この X.509 パース・validity 検査・葉 SPKI 公開鍵の照合・メッセージ組み立てを
`crates/wire-server/src/tls/der.rs`（`pub(crate)`）・`tls/x509.rs`（`pub`）として
担う。

証明書署名そのものの検証・SAN 等 extensions の意味解釈・中間証明書どうしの
連結検査・Ed25519 の鍵導出（秘密鍵 seed → 公開鍵）・CLI 結線・ハンドシェイク
状態機械への結線はいずれも本 Issue の対象外である（後述「スコープ外」参照）。

## #961（Ed25519 鍵導出）との接続点（seam）

受入基準の「証明書の SPKI 公開鍵と、秘密鍵から導出した公開鍵が一致しなければ
起動失敗」には、秘密鍵 seed から公開鍵を導出する処理（SHA-512・Edwards
スカラー倍算）が要るが、これは Issue #961 の担当である。本モジュールは
「導出済みの期待公開鍵」を `&[u8; 32]` として引数で受け取り、葉証明書の SPKI
公開鍵と照合するところまでを担う。実際の起動時失敗化（鍵ファイル読み込み →
公開鍵導出 → 本モジュールでの照合 → 不一致ならプロセスを起動失敗させる、
という一連の組み立て）は #967（CLI opt-in）の担当である。

## `tls/der.rs`: 一般化した DER TLV リーダー

`tls/pkcs8.rs::DerReader`（Issue #962）は固定形状の値だけを読む最小実装
だったのに対し、X.509 は任意深さの入れ子（`SEQUENCE`／`SET`／コンテキスト
依存タグ）を持つため、これを一般化した独立実装を `der.rs` に新設した
（`pkcs8::DerReader` は変更せず、固定形状専用のまま残置）。

- 単一バイトのタグのみ受理する（high-tag-number 形式・BER の EOC `0x00`
  （DER では意味を持たない）はいずれも `read_tag` で `UnsupportedTag` として
  拒否する）
- 長さは definite 形式のみ。indefinite（`0x80`）は拒否
- long form は 1〜4 オクテットまでとし、最小符号化を要求する（1 バイトで
  表現できる値を long form で書いた・先頭オクテットが `0x00` の long form は
  いずれも `NonMinimalLength`）
- 長さの蓄積は `u32` で行ってから `usize::try_from` で変換する。
  `checked_shl` は「シフト量」しか検査せず値のビット落ちは検出しないため、
  4 オクテットの long form（`u32` にちょうど収まる）を経由することで
  オーバーフローを構造的に避ける
- `validate_structure(der, max_depth)` が構造全体（トップレベルがちょうど
  1 個の TLV であること・constructed な TLV の値部分が入れ子の TLV 列として
  整形式であること）を深さ上限 `MAX_DER_NESTING_DEPTH`（16。本リポの実装
  既定値。X.509 の `Name` が `SEQUENCE → SET → SEQUENCE` で 5 階層程度に
  なるため十分な余裕を見込む）付きで検証する。あわせて、フィールドの意味を
  問わず入れ子の全階層で次の DER 制約を検査する
  - universal クラスのタグは `SEQUENCE`／`SET` を除き primitive でなければ
    ならない（constructed 化した文字列型等は `ConstructedUniversalType`）
  - `SEQUENCE`／`SET` は常に constructed でなければならない（primitive の
    `0x10`／`0x11` は `PrimitiveSequenceOrSet`）
  - `NULL` は値が空、`BOOLEAN` はちょうど 1 バイトで `0x00`／`0xFF` のいずれか
    （それ以外は `InvalidPrimitiveEncoding`）
- 上記以外の primitive な値（`OCTET STRING`・`BIT STRING`・`INTEGER` 等）の
  中身には潜らない（フィールドとしての検査は `x509.rs` が担う）
- `Tlv` は `tag`・`value` に加えて `raw`（タグ+長さ+値の生バイト列）を持つ。
  `read_any` は消費前の残り入力（`start`）と消費後の残り入力（`remaining`）の
  長さの差分から `raw` を切り出す。`start`・`remaining` は常に同一バッファの
  先頭を削っていくだけの関係にあるため、この差分計算は安全である
- 添字アクセス（`[]`）は使わず `split_first`／`split_at_checked` のみで
  進める（受信データ経路。`.claude/rules/coding-rust.md`）

`raw` フィールドは `x509.rs` が `tbsCertificate.signature` と外側
`signatureAlgorithm` の DER バイト列一致（RFC 5280 §4.1.1.2）を検査する際に
使う。

## `tls/x509.rs`: 証明書のパース手順（拒否理由が決定的に決まる順序）

1. 証明書 DER 長の上限検査（`MAX_CERTIFICATE_DER_LEN` = 64 KiB。本リポの
   実装既定値）。確保より前に判定する
2. `der::validate_structure` による TLV 全体の整形式・入れ子深さ上限の検証
3. `Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
   signatureValue BIT STRING }`（後続バイトは拒否）
4. `tbsCertificate` の各フィールドを順に読む:
   - `version [0] EXPLICIT INTEGER 2`（v3）のみ受理。タグ不一致（欠落＝v1
     を含む）・値不一致（v2 の `1` 等）はいずれも `UnsupportedVersion`
   - `serialNumber INTEGER`: 正の整数（最上位ビットが立つ負数・全オクテット
     0 のゼロは拒否）・20 オクテット以下・最小符号化（符号ビット確保以外の
     不要な先頭 `0x00` を拒否）であること（RFC 5280 §4.1.2.2）
   - `signature AlgorithmIdentifier`（生バイト列 `raw` を保持し、手順 5 で
     構造検査と外側 `signatureAlgorithm` との比較を行う）
   - `issuer Name`: 属性値の意味は解釈しないが、`RDNSequence ::= SEQUENCE OF
     RelativeDistinguishedName`・`RelativeDistinguishedName ::= SET SIZE
     (1..MAX) OF AttributeTypeAndValue`・`AttributeTypeAndValue ::= SEQUENCE
     { type OBJECT IDENTIFIER, value ANY }` の構文（RFC 5280 §4.1.2.4）を
     検査する。RDN が `SET` でない・空の `SET`・`type` の OID 欠落／不正・
     `value` 欠落・余剰要素はいずれも `Malformed`。0 個の RDN から成る空の
     `Name` は受理する。さらに `SET OF` の DER 正規順序（X.690 §11.6。各
     `AttributeTypeAndValue` の符号化バイト列が非減少の昇順）を検査し、
     隣接要素が降順の非正規 BER は `Malformed` とする（同一符号化の重複は
     昇順の定義上許容する）
   - `validity SEQUENCE { notBefore Time, notAfter Time }`（後続バイト拒否・
     `notBefore > notAfter` は `InvalidValidityRange`）
   - `subject Name`（`issuer` と同じ構文・順序検査）
   - `subjectPublicKeyInfo SEQUENCE { AlgorithmIdentifier, BIT STRING }`:
     AlgorithmIdentifier は葉・中間を問わず構造検査（先頭に整形式の OID が
     1 個・任意の `parameters` は高々 1 個の TLV・余剰要素なし）を通し、OID・
     parameters 有無・鍵ビット列を保持する
   - 任意の `issuerUniqueID [1] IMPLICIT`・`subjectUniqueID [2] IMPLICIT`:
     存在すれば `BIT STRING` の形状（未使用ビット数 0〜7・非 0 なら最終
     オクテットの未使用ビットが 0）を検査する
   - 任意の `extensions [3] EXPLICIT`: wrapper の値部分がちょうど 1 個の
     `Extensions`（`SEQUENCE`）で後続データが無く、`Extensions` が 1 個以上の
     `Extension` から成ることを検査する。各 `Extension` は `SEQUENCE {
     extnID OBJECT IDENTIFIER, critical BOOLEAN DEFAULT FALSE, extnValue
     OCTET STRING }`（RFC 5280 §4.1）の順序どおりに読み、空の Extension・
     `extnID` 欠落／不正な OID・`critical` の型違いや `extnValue` 後への配置・
     `extnValue` 欠落／型違い・余剰要素はいずれも `Malformed`。`critical` の
     明示的な `FALSE`（`01 01 00`）は、DER（X.690 §11.5）が DEFAULT 値の省略を
     要求するものの、公開テストベクタである RFC 8410 §10.2 の証明書自身が
     この形を使うため受理する。`extnValue` の中身は解釈しない
   - `tbsCertificate` の後続バイトは拒否
5. `tbsCertificate.signature` の AlgorithmIdentifier 構造検査（SPKI と同じ
   基準）の後、外側 `signatureAlgorithm` との DER バイト列一致（RFC 5280
   §4.1.1.2。不一致は `SignatureAlgorithmMismatch`）
6. `signatureValue BIT STRING`: 先頭の未使用ビット数オクテットが 0〜7・その
   後に署名データが 1 バイト以上存在すること・未使用ビット数が非 0 なら最終
   オクテットの下位未使用ビットがすべて 0 であること（中身は検証しない。
   署名検証はスコープ外）

OID の値部分は、空・サブ識別子先頭の `0x80`（非最小符号化）・継続ビット付きの
まま終端（切り詰め）をいずれも `Malformed` として拒否する（AlgorithmIdentifier・
`AttributeTypeAndValue.type`・`extnID` に共通）。

validity はチェーン内の**全証明書**に対して現在時刻（呼び出し元が注入する
`now_unix_secs`）で検査する。葉の SPKI 公開鍵照合は**チェーンの先頭のみ**に
適用する。

## 時刻パース（RFC 5280 §4.1.2.5）

- UTCTime（tag `0x17`）: `YYMMDDHHMMSSZ` の 13 バイト固定。`YY >= 50` は
  19YY、`YY < 50` は 20YY と解釈する
- GeneralizedTime（tag `0x18`）: `YYYYMMDDHHMMSSZ` の 15 バイト固定。RFC 5280
  は 2050 年以降にのみ GeneralizedTime を使うことを要求するため、2050 年
  未満は fail-closed で拒否する
- 共通: 数字以外・`Z` 以外の末尾（タイムゾーンオフセット・小数秒を含む）は
  拒否。月 1〜12・日は月と閏年（グレゴリオ暦。400 年ルール込み）に応じた
  上限・時 0〜23・分 0〜59・秒 0〜59（うるう秒 60 は非受理）を検査する
- エポック秒（`i64`）への変換は Howard Hinnant の `days_from_civil`
  アルゴリズム（外部クレートなし・`div_euclid` で負の年にも対応）を自作し、
  秒への合成は `checked_mul`／`checked_add` でオーバーフローを検出する。
  RFC 8410 §10.2 の証明書 validity（notBefore=1470053964・
  notAfter=2240611199）・UNIX epoch（0）・1950 年（負値）の 3 値で
  単体テスト固定した

## SPKI（葉証明書のみ）

- AlgorithmIdentifier の OID が Ed25519（`2b 65 70`）で `parameters` が
  「無い」こと（RFC 8410 §3。NULL であっても `InvalidPublicKey`）
- `BIT STRING` が未使用ビット 0 で、鍵がちょうど 32 バイトであること
  （それ以外は `InvalidPublicKey`）
- 期待公開鍵との照合は `super::hkdf::ct_eq`（定数時間比較）で行う。公開鍵は
  公開データそのものだが、導出元が秘密鍵であるため保守的に定数時間で
  比較する
- 中間証明書の SPKI アルゴリズムは制限しない（AlgorithmIdentifier の構造
  だけ検査する）

## `Certificate` メッセージの組み立て

`ServerCertificateChain::from_der_chain` はチェーン全体の検査後、入力順
（先頭が葉）どおりに `CertificateEntry { cert_data, extensions: vec![] }` を
並べた `super::handshake::Certificate`（`certificate_request_context` は空）
を組み立て、`serialize_body_into` で u24 長さフィールドの上限
（`0xFF_FFFF`）に収まることを起動時に確認する（ハンドシェイク中の送出
失敗を避けるため）。収まらない場合は `MessageTooLarge` を返す。

## スコープ外（明記して実装しなかった事項）

- 証明書署名そのものの検証（発行者鍵の検証はクライアントの責務。本サーバー
  側が検証する鍵一致は `CertificateVerify` 用の Ed25519 鍵一致のみで
  #961 の担当）
- SAN・ホスト名・keyUsage・basicConstraints 等 extensions の意味解釈
  （各 `Extension` の `extnID`／`critical`／`extnValue` の構文までは検査するが、
  `extnValue` の中身は解釈しない。同一 `extnID` の重複検出も行わない）
- issuer/subject `Name` の属性値の意味解釈（文字列型の妥当性・属性種別の
  制約）
- 中間証明書どうしの issuer/subject 連結検査・パス構築（RFC 8446 §4.4.2 は
  後続証明書の順序を SHOULD とするに留まるため、チェーン先頭が葉であることの
  みを前提にする）
- `pkcs8::DerReader` を `der.rs` へ統合する整理（`pkcs8` 側のテストが拒否
  順序を固定しているため今回は触らない。将来のフォローアップ候補）
- CLI 結線（#967）・ハンドシェイク状態機械（#965）・接続組み込み
  （#966・#968）・`CertificateVerify`（#961）

## テストで固定した公開ベクタ・拒否ケース

- RFC 8410 §10.1 の Ed25519 公開鍵（`19bf4409...`）・§10.2 の X25519
  自己発行証明書（304 バイト・v3・notBefore/notAfter が UTCTime）・§10.3 の
  秘密鍵 seed（`d4ee72db...`。§10.1 の公開鍵と対応）・RFC 8032 §7.1 TEST 1
  の公開鍵（不一致の対照値）
- 単体テスト（`tls/x509.rs`）: UTCTime／GeneralizedTime の境界（49/50・
  2050 年）・閏年（2024／2023／2100／2000）・不正な暦フィールド（月 0/13・
  日 32・時 24・分/秒 60）・`Z` 欠落／オフセット付き／小数秒／長さ違いの
  拒否・validity 境界（`now == notBefore`／`notAfter` は受理）・SPKI 照合
  （一致・不一致・非 Ed25519・parameters 付き・鍵長 31/33・未使用ビット
  非 0）・空チェーン／上限超過チェーンの拒否・RDN の `SET OF` 順序判定
  （昇順・等値は受理、降順は拒否）・`Extension` 構文検査（`critical` 省略／
  TRUE／明示 FALSE は受理、空・`extnID` 欠落・`extnValue` 欠落・`critical`
  後置・`extnValue` 型違い・余剰要素・不正 OID は拒否）
- 結合テスト（`tests/tls_x509.rs`。公開 API のみ）: RFC 8410 §10.2 証明書を
  葉に置くと X25519 として拒否されること、手組みした Ed25519 葉（RFC 8410
  §10.1 の SPKI を埋め込み）が期待公開鍵で受理されること、葉＋中間
  （RFC 8410 §10.2）の 2 段チェーンで `certificate_message()` が入力順・
  空 extensions・`Certificate::parse` との往復一致を保つこと、公開鍵不一致
  （全く別の鍵・1 ビット反転）・中間証明書の `NotYetValid`／`Expired`
  （`index: 1`）・空チェーン／上限超過チェーン・切り詰め／末尾 1 バイト
  追加／PKCS#8 DER を証明書として渡す／version 欠落（v1）・v2（`1`）／
  `tbsCertificate.signature` と外側の不一致／両側とも空の AlgorithmIdentifier・
  余剰要素付き・不正な OID の AlgorithmIdentifier／中間証明書 SPKI の
  AlgorithmIdentifier の余剰要素・切り詰め OID／serialNumber の負数・ゼロ・
  21 オクテット・非最小符号化（20 オクテット・符号ビット確保の先頭 `0x00` は
  受理）／`signatureValue` の署名データ欠落・非 0 パディング（0 パディングは
  受理）／SPKI parameters 付き／BIT STRING 未使用ビット非 0／鍵長 31・33／
  issuer の primitive `SET`・subject の RDN が `SEQUENCE`／複数属性 RDN の
  降順（issuer・subject・長さ違いの要素を含む。昇順・同一要素の重複は受理）／
  extensions wrapper の余剰データ／Extension の内部構文違反（空・`extnID`
  欠落・`extnValue` 欠落・`critical` の後置・`extnValue` が BIT STRING・
  余剰要素・`critical` が INTEGER・切り詰め `extnID`。単独でも正当な
  Extension の後続でも拒否。`critical` 省略・TRUE・明示 FALSE は受理）／
  DER 長上限超過の拒否、エラー `Display` が証明書内容（CN 文字列・鍵の 16 進
  表現）を含まないこと、ファイル入口（一時ファイル経由の成功・存在しない
  ファイル）を固定した
- 単体テスト（`tls/der.rs`）: long form 長さの境界・非最小符号化・indefinite・
  high-tag-number／EOC・入れ子深さ上限・constructed 化した universal 型・
  primitive の `SEQUENCE`／`SET`・`NULL`／`BOOLEAN` の非正規形の拒否

手組み DER エンコーダ（`tlv`／`sequence` 等）はテスト専用であり、本番コード
には存在しない（`pkcs8` の結合テストと同方針）。

## 定数時間性の整理

- DER の構造部分（タグ・長さ・入れ子構造）・時刻値・アルゴリズム OID は
  いずれも公開された形式情報であり秘密ではないため、これらに基づく分岐は
  問題ない
- 唯一の秘密由来の値は期待公開鍵（導出元は秘密鍵だが公開鍵自体は公開値）
  であり、その比較のみ `super::hkdf::ct_eq`（定数時間比較。GCM タグ検証
  （#958）と共有）を使う

## 申し送り

- `pkcs8::DerReader` と `der::DerReader` の統合整理は本 Issue の対象外
  （フォローアップ候補として記録のみ）
- 鍵ファイル・証明書ファイルのモード検査（0600 強制等）は
  [`tls-pem-pkcs8.md`](./tls-pem-pkcs8.md) が既に申し送り済みの論点であり、
  本 Issue でも扱わない
