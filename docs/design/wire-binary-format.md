# バイナリ形式の結果エンコーディング（WIRE-14）の設計判断

- ステータス: **Accepted（Phase A: エンコーダ層のみ。wire 経由の結線は Phase B）**
- 対応: Issue #936（TASK-218・WIRE-14。ポインタ: `docs/spec/05-tasks.md`
  TASK-218・`docs/spec/04-behavior/wire-protocol.md` WIRE-14）
- 実装: `crates/wire-server/src/result_encoder.rs`
- テスト: `crates/wire-server/src/result_encoder.rs`（単体）・
  `crates/wire-server/tests/wire14_binary_format.rs`（結合）

## 背景

wire-server の `RowDescription`／`DataRow` は format code フィールドが 0
（テキスト）固定だった。PostgreSQL wire プロトコル v3 は結果列ごとにテキスト
／バイナリ形式を要求できるが、その要求は拡張クエリプロトコルの **Bind**
メッセージでのみ行われる。

## スコープを 2 フェーズへ分けた理由

Bind／Execute（Issue #934）・Parse／Describe（Issue #933）はいずれも本 Issue
時点で未実装であり、`protocol_dispatch::classify` は `'B'` を拡張クエリ
プロトコルとして分類し `0A000` で接続を閉じる（WIRE-8）。型 OID の拡張
（`id` → `int8` 等。Issue #895・WIRE-13）も別 Issue の担当。

このため、実際に wire 経由でバイナリ形式を要求して値を受け取るところまでは
本 Issue 単独では到達できない。範囲を次の 2 つに分けた。

- **Phase A（本 Issue）**: `result_encoder.rs` に置く純関数のエンコーダ層
  （形式コードの解決・対応型の事前検査・型ごとのバイナリレイアウト・
  形式コード付きの `RowDescription`／`DataRow` 組み立て）。wire の送受信
  経路（`simple_query.rs`・`protocol_dispatch.rs`・`handshake.rs`）は変更
  しない。
- **Phase B（#934 以降）**: Bind の結果形式コードを本 API へ渡す結線、
  `0A000` のあと当該文だけを拒否して接続を維持する同期回復（WIRE-11）、
  3 クライアント（psycopg 3 `binary=True`・node pg `binary: true`・生バイト
  Rust クライアント）でのバイナリ受信 e2e。psql は拡張クエリプロトコル
  未対応かつバイナリ結果を要求できないため層 B の対象クライアントから外れる。

受け入れ条件 1（「バイナリを要求された列で返せる」）は、本 Issue では
「エンコーダ層として返せる」までを達成範囲とする。

## 形式コードの列ごとの解決（`ResultFormats::resolve`）

PostgreSQL の Bind 規則に従う。

| 指定個数 | 解決 |
| --- | --- |
| 0 個 | 全列テキスト |
| 1 個 | 全列に同じ値を適用 |
| 列数と同数 | 列ごとに適用 |
| それ以外 | `BinaryFormatError::FormatCountMismatch`（`08P01`） |
| 値が {0, 1} 以外 | `BinaryFormatError::InvalidFormatCode`（`08P01`） |

PostgreSQL 本体は不正な format code 値を `22023` で返すが、本リポの
`ErrorClass::OperationIdContentMismatch` が `22023` を専有しており意味が
異なるため、`ErrorClass` を増やさず本モジュールの fail-closed 方針（構文
違反は `08P01`）に寄せた。

解決結果 `Vec<FormatCode>` の長さは列数（`i16` で有界）にのみ比例する。
`codes` スライス自体の長さ上限検証は、untrusted な Bind 本文を解析する側
（#934）の責務であり、`ResultFormats::resolve` は受け取ったスライス長に
比例した確保を行わない。

## 対応型の事前検査（`0A000`）: 1 バイトも送る前に判定する

`validate_binary_formats` は `RowDescription` を送る前に呼ぶ前提の純関数。
応答列の一部を送ってからエラーにするとフレームが崩壊するため、この順序は
必須。

判定は公告型（`ColumnMeta`／`WireType`）で静的に行い、実行時の `Cell` では
判定しない。

| 列 | 公告 OID | バイナリ可否 | 根拠 |
| --- | --- | --- | --- |
| `ColumnMeta::Id` | numeric（1700） | **非対応 → `0A000`** | WIRE-14 は `NUMERIC` を非対応型としている。#895 で `int8` に変わったら対応する |
| `Scalar{ty: Text}` | text（25） | 対応（UTF-8 生バイト） | PostgreSQL の text send と同じ |
| `Scalar{ty: Vector(_)}` | text（25） | **非対応 → `0A000`** | WIRE-14 は `VECTOR(N)` を非対応型としている。公告が text だからといって `[1,2.5]` の文字列をそのまま送らない |
| `ColumnMeta::Computed` | text（25） | **非対応 → `0A000`（fail-closed）** | 実行時の型（Float／Bool／Vector）が静的に決まらず、事前検査で `VECTOR` を除外できない。#895 で型が付いたら見直す |

**`VECTOR` 列のバイナリ表現（受け入れ条件 3）**: spec（SSOT）の WIRE-14 に
従い「バイナリ非対応として `0A000` で拒否する」を決定とする。独自のバイナリ
表現（float4 配列など）は定義しない。

`WireType::supports_binary`（`Id`／`Text` の型そのものの対応可否）と
`column_binary_support`（列種別を見た最終判定。`Vector`／`Computed` の
上書きを含む）を分離し、`#[deny(clippy::wildcard_enum_match_arm)]` を付けた
網羅 `match` にすることで、`ColumnMeta`／`WireType` に variant が増えたとき
（#895）にバイナリ可否の決定漏れをコンパイルエラーで検出する。

## 型ごとのバイナリ表現（`binary` サブモジュール）

PostgreSQL の send 関数と同じレイアウトで 8 型のバイナリ表現を組み立てる
純関数を用意した（`int4`／`int8`／`float4`／`float8`／`bool_`／`bytea`／
`uuid`／`text`）。本 Issue で `WireType` 側に実際に結線されるのは `text`
のみで、他の型は `WireType`／`Cell` 拡張（#895）が使う部品として先行提供
する。golden バイト列の単体テストで PostgreSQL 規約との一致を固定した
（例: `int4(1)` → `00 00 00 01`、`int8(-1)` → `ff` × 8、`float8(1.0)` →
`3f f0 00 …`、`bool_(true)` → `01`）。

`#895` への申し送り: `id` を `int8` にした場合、`u64 > i64::MAX` の値を
バイナリでどう扱うかは #895 の論点となる。本 Issue では `Id` をバイナリ
非対応にしているため、この問題はまだ発生しない。

## 形式コードを受け取るエンコーダと既存出力の不変性

`encode_row_description_with_formats`／`encode_data_row_into_with_formats`
を新設し、既存の `encode_row_description`／`encode_data_row_into`／
`encode_data_row` は「全列テキスト」でこれらを呼ぶ薄いラッパーへ変更した。
生成バイト列は完全に同一であることを不変性テスト（`row_description_with_
all_text_formats_matches_legacy_encoder`・`data_row_with_all_text_formats_
matches_legacy_encoder`）で固定している（受け入れ条件 4）。シグネチャは
変えていないため破壊的変更ではない。

`formats.len()` が列数／セル数と一致しない場合は呼び出し元の内部不整合と
みなし `EncodeError`（`XX000`）とする。untrusted 入力由来のバイナリ形式
検査自体は `validate_binary_formats` が別途 `RowDescription` 送出前に
済ませている前提であり、`encode_data_row_into_with_formats` はさらに
「バイナリ指定なのに非対応セル（`Cell::Integer`／`Vector`／`Float`／
`Bool`）が来た」場合も契約違反として fail-closed に拒否する（黙って別表現
へフォールバックしない）。

失敗時に `out` を呼び出し前の長さへ `truncate` する既存の契約（部分フレーム
を残さない）は据え置いた。長さはすべて `i32::try_from`／`checked_*` で
計算し、`as` キャストは使わない。

## セキュリティ（OWASP・AGENTS.md P0）

- **インジェクション**: 形式コードは列挙型へ閉じた写像にし、SQL や文字列の
  組み立てには一切使わない。
- **アクセス制御・テナント境界**: エンコーダは engine が RLS を適用した後の
  `QueryResult` だけを受け取り、可視性判定には関与しない。RLS の経路は
  変わらない。
- **fail-closed**: 非対応型は送出前に `0A000` で拒否する（中途半端な応答を
  送らない）。`Computed` は型が静的に決まらないため拒否側に倒す。`VECTOR`
  を text バイトとして黙って送らない。形式コードの不正は `08P01`。失敗時は
  部分フレームを残さない。
- **情報漏えい**: エラーメッセージに含める情報は最大でも要求者自身の列番号
  まで。テーブル名・他テナントの存在情報は含めない。
- **DoS**: 解決結果の確保量は列数（`i16`）で有界。形式コード列自体の長さ
  上限検証は Bind パーサ（#934）の責務。`unwrap`／`expect`／`[]` は使わない。
- **依存**: 追加なし（`Cargo.toml` は無変更）。

## 申し送り（Phase B 以降）

- Bind の結果形式コードを本 API へ渡す結線・Describe(portal) の
  `RowDescription` への反映 → #934
- `0A000` のあと当該文だけを拒否して接続を維持する同期回復 → #934（WIRE-11）
- パラメータのバイナリ復号（長さ上限検証と `08P01`）→ #935／#934
- `id`・集計列の型 OID 拡張とバイナリ対応の拡大 → #895
- 3 クライアントのバイナリ受信モードでの値一致（層 B）→ #934 完了後
- カーソル（WIRE-15）→ #937
