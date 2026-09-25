# PEM デコードと PKCS#8（Ed25519）秘密鍵の読み込み 実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #962・親 Issue #941

## 背景

自作 TLS 1.3 サーバー（親 Issue #941）を起動するには、運用者が用意した
サーバー秘密鍵・証明書のファイルを読み込み、後続（Ed25519 署名の #961、
X.509 と Certificate メッセージの #963、CLI の #967）が使える形に変換する
必要がある。本 Issue はこのファイル読み込み・PEM デコード・PKCS#8 の
最小パースだけを担う（`crates/wire-server/src/tls/pem.rs`・`pkcs8.rs`）。

X.509 DER のパース・公開鍵との整合チェック・validity 検査・Certificate
メッセージへの組み込みは #963、Ed25519 の鍵導出・署名は #961、CLI フラグ
と `main.rs` への結線は #967 の担当であり、いずれも本 Issue の対象外
（本 Issue は `tls::ed25519` に依存せず、Ed25519 の 32 バイト seed を返す
ところまでを担う）。

## lax／strict の線引き（PEM 構文。RFC 7468）

- 改行は LF・CRLF のいずれも受け付け、行の長さは強制しない
  （RFC 8410 §10.2 の証明書例が 66 文字行を使うため、64 文字固定にすると
  公開テストベクタ自体が通らなくなる）
- ブロックの外側に許すのは空白・改行のみとし、`Bag Attributes` のような
  説明文は受け付けない（RFC 7468 は許容するが、fail-closed を優先する
  意図的な選択）
- 本文中に `:` を含む行（RFC 1421 形式の `Proc-Type:`／`DEK-Info:` 等の
  暗号化ヘッダ）があれば `HeadersNotSupported` で拒否する
- ASCII 範囲外のバイトが 1 つでもあれば拒否する
- BEGIN と END のラベルが一致しない・入れ子になった BEGIN・BEGIN の無い
  END・BEGIN はあるが END が無い・空ファイルは、それぞれ専用の
  `PemError` variant で拒否する

## 定数時間 base64 デコードを別実装にした理由

秘密鍵（`PRIVATE KEY` ブロック）の base64 は、既存の `auth::base64_std`・
`http::query::base64_std`（いずれも文字ごとの `match` 分岐でデコードする）
とは意図的に別実装にした。これらは decode 対象が resumption データ・
`BYTEA` 列値であり秘密鍵バイトそのものではないため、文字分岐で構わない。
一方 `tls::pem` がデコードするのは秘密鍵の base64 表現であり、共通条件
「秘密値に依存する分岐・テーブル参照を作らない」を満たすため、sextet 値を
5 つのアルファベット区間（`A-Z`・`a-z`・`0-9`・`+`・`/`）の符号マスク
（`((lo - c) & (c - hi)) >> 8` という公知の branchless 手法）で求める。

実装時に「マスクを `-1`（全ビット 1）で初期化して OR で積み上げる」形を
最初に書いたが、OR は 0 のビットしか変化させられないため非マッチの区間で
値が変わらず、常に `-1`（invalid 扱い）を返す不具合になった（単体テストの
全 256 バイト網羅照合で検出）。正しくは「値の蓄積」と「いずれかの区間が
マッチしたか」を別のアキュムレータ（両方とも初期値 0）で独立に積み上げ、
後者が 0 のままなら invalid とする形にする必要があった。

正しさは以下の 2 系統のテストで保証する。

- 全 256 バイト値について、既存の strict デコーダ相当（`match` 分岐での
  期待値）と一致することを確認する網羅テスト
- 0〜64 バイトの全長について `http::query::base64_std::encode_base64_std`
  でエンコードした値を定数時間デコーダで復号し、ビット一致することを
  確認するラウンドトリップテスト

証明書（公開データ）のデコードにも同じ定数時間デコーダを使い、実装を
1 つに保つ（性能上の必要はないが、実装を分岐依存版と定数時間版の 2 つに
分けて保守するコストを避けた）。

## PKCS#8 の DER バイト列（RFC 8410 §7・§10.3。Ed25519 PKCS#8 v1・48 バイト）

```text
30 2e                 SEQUENCE (46)
   02 01 00           INTEGER version = 0 (v1)
   30 05              SEQUENCE AlgorithmIdentifier
      06 03 2b 65 70  OID 1.3.101.112 (Ed25519)。parameters は「無い」のが正
   04 22              OCTET STRING privateKey (34)
      04 20 <32B>     CurvePrivateKey ::= OCTET STRING (32) ← これが seed
```

外側の OCTET STRING（34 バイト）を seed だと誤解しないよう、seed は
内側の `04 20` の後ろにある 32 バイトである点をコード・コメント双方で
明示した。

### 拒否の判定順序（固定）

1. 外側の SEQUENCE（後続バイトなし）
2. version（`0x00`／`0x01` 以外は `Malformed`）
3. AlgorithmIdentifier の SEQUENCE・OID
4. OID による種別判定（Ed25519 以外は `UnsupportedAlgorithm(KeyAlgorithm)`。
   RSA・ECDSA・Ed448・X25519 は個別 variant、それ以外は `Other`）
5. Ed25519 の parameters 残存チェック（`UnexpectedParameters`。NULL でも
   不可）
6. version が `1`（v2・OneAsymmetricKey）なら `UnsupportedVersion`
7. privateKey の外側 OCTET STRING → 内側 OCTET STRING（seed）の長さが
   ちょうど 32 であることの検証（`InvalidSeedLength`）
8. privateKey の後ろ（attributes `[0]` 等）に残りバイトがあれば
   `Malformed`

種別判定（4）を version チェック（6）より先に行うのは、RSA・EC 鍵を渡した
運用者に対して「どの鍵形式が非対応か」を報告できるようにするため
（受入基準 3）。v2（OneAsymmetricKey）は publicKey フィールドの照合に
鍵導出（#961）が必要になるため、黙って無視せず明示的に `UnsupportedVersion`
として拒否する（fail-closed）。

### DER リーダー

`pkcs8.rs::DerReader` は固定形状の値だけを読む最小 TLV リーダーで、
以下を満たす:

- 長さは definite 形式のみ（`0x80` indefinite は拒否）
- long form は 1〜2 バイトまでで、最小符号化を強制する（1 バイトで
  表現できる値を long form で書いた・`0x82` の値が `0x100` 未満、はいずれも
  `Malformed`）
- 長さが残りバイト数を超えれば拒否する（`checked_shl` を用いた整数演算）
- 実装は `get()`／`split_first`／`split_at_checked` のみを使い、添字
  アクセス（`[]`）は使わない（受信データ経路。
  `.claude/rules/coding-rust.md`）

`#963` が X.509 向けに TLV 走査を一般化する際は、この最小実装（固定形状
専用）を出発点として拡張できるよう `pub(crate)` に留めた。

## 定数時間性の整理

- DER の構造部分（タグ・長さ・version・OID）は形式として公開された固定値
  であり秘密ではないため、これらでの分岐は問題ない。
- 秘密値は seed の 32 バイトとその base64 表現のみ。前者はコピーする
  だけで分岐せず、後者は上記の定数時間デコーダが処理する。

## ゼロ化の限界

`SecretBuf`（ファイルバッファ・base64 除去後テキスト・デコード後の DER）・
`Ed25519Seed`（32 バイト seed）はいずれも `Drop` 時に
`super::hkdf::zeroize`（全バイトを 0 埋め・`black_box` で最適化除去を
妨げる best-effort な処理）を呼ぶが、`unsafe`（`write_volatile` 等）を
使わないため、コンパイラの最適化によって消去そのものが省略されない保証は
ない。これは既存の `hkdf::Secret32`・`x25519::SharedSecret` と同じ限界
であり、本 Issue で新たに緩和も強化もしていない。

## 上限値（本リポ独自の実装既定値。spec 由来の数値ではない）

| 定数 | 値 | 用途 |
| ---- | -- | ---- |
| `MAX_PRIVATE_KEY_FILE_LEN` | 16 KiB | 秘密鍵 PEM ファイルの読み込み上限（Ed25519 PKCS#8 は 48 バイトの DER に収まるため十分な余裕） |
| `MAX_CERTIFICATE_FILE_LEN` | 1 MiB | 証明書チェーン PEM ファイルの読み込み上限 |
| `MAX_CERTIFICATE_CHAIN_LEN` | 8 | 証明書チェーンに含めてよい `CERTIFICATE` ブロック数の上限 |

ファイル読み込みは `main.rs` の `--scram-mock-key-file` 読み込みパターン
（`std::fs::metadata` で通常ファイルを確認 → `File::open` →
`Read::take(max + 1)` で上限 + 1 バイトまで読む二重防御）をそのまま踏襲した
（`pem::read_bounded_file`）。

## 公開 API

- `pem::decode_certificate_chain_pem` / `pem::load_certificate_chain_file`:
  1 個以上 `MAX_CERTIFICATE_CHAIN_LEN` 個以下の `CERTIFICATE` ブロックを
  ファイル内の出現順（先頭が葉証明書）で DER として返す
- `pkcs8::parse_ed25519_pkcs8_der` / `pkcs8::decode_ed25519_private_key_pem`
  / `pkcs8::load_ed25519_private_key_file`: Ed25519 の 32 バイト seed
  （`Ed25519Seed`）を返す。`Clone`／`Copy` は導出せず `Debug` は内容を伏せる
- `pem::decode_private_key_pem`（`pub(crate)`）: `PRIVATE KEY` ブロックが
  ちょうど 1 個であることを要求して生 DER を返す。`pkcs8` 以外の呼び出し元
  は現状想定していない

## テストで固定した公開ベクタ

- RFC 8410 §10.3 v1 の秘密鍵 PEM → seed
  `d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842`
  と一致すること
- RFC 8410 §10.3 の v2（OneAsymmetricKey）相当の最小 DER →
  `UnsupportedVersion`
- RFC 8032 §7.1 TEST 1 の秘密鍵（seed）を PKCS#8 DER へ埋めて往復
- 証明書チェーン: 決定的な合成 DER（SEQUENCE ヘッダ + 適当な中身。
  X.509 の意味論そのものの検証は対象外）を RFC 8410 §10.2 と同じ 66 文字
  幅で改行した PEM で、単一ブロック・複数ブロック連結・上限超過を検証

## 申し送り

- 鍵ファイルのモード検査（0600 強制等）は本 Issue の受入基準に含まれず
  未実装。#967（CLI 結線）・#971（監査）で扱うべき論点として記録する
- v2（OneAsymmetricKey）の publicKey 照合による受理は、鍵導出（#961）が
  実装された後に別途判断する
- `pkcs8::DerReader` は固定形状専用の最小実装であり、#963 が X.509 向けに
  TLV 走査を一般化する際の出発点として使える設計にしてある
