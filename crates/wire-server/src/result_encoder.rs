//! 簡易クエリプロトコルの応答メッセージ（`RowDescription`/`DataRow`/
//! `CommandComplete`/`EmptyQueryResponse`）のバイト列生成を担う。
//!
//! 責務境界: 本モジュールは純関数のみを持ち、I/O を一切行わない
//! （`Vec<u8>` を返すのみ）。呼び出し元の [`crate::simple_query`] が
//! `TcpStream` への書き込みを担当する。`engine::sql::exec::{ColumnMeta, Cell,
//! ResultRow}` から wire v3 の text/binary フォーマットへの写像がここに閉じる
//! （TASK-73・WIRE-1・WIRE-14）。
//!
//! 型写像。単一情報源は [`WireType`]（本モジュール `pub(crate)`）で、
//! `crate::http::query::response`（NoSQL 表層の JSON 応答スキーマ
//! `columns[].type`。Issue #762・NOSQL-11）もこの enum を経由して同じ写像を
//! 参照する（2 箇所に写像表を持たない）:
//! - `ColumnMeta::Id` → `numeric`（OID 1700, typlen -1）。engine の行 ID は
//!   `u64` 全域（`u64::MAX` を含む）を有効値とするため、符号付き 64bit の
//!   `int8`（OID 20）では `i64::MAX` を超える正当な ID を表現できない
//!   （PR #210 レビュー指摘）。10進テキスト表現は `int8`/`numeric` のどちらでも
//!   同一であり、`numeric` として公告することで値域制限なく全 `u64` 値を
//!   そのまま送出できる
//! - `ColumnMeta::Scalar{ty: Text}` → `text`（OID 25, typlen -1）
//! - `ColumnMeta::Scalar{ty: Vector(_)}` → `text`（OID 25。値は `[v1,v2,...]` 形式）
//! - `ColumnMeta::Computed{..}` → `text`（OID 25。実行時型のため text 固定）
//!
//! バイナリ形式（format code 1・WIRE-14・TASK-218・Issue #936）: 列ごとに
//! テキスト／バイナリを要求できる（PostgreSQL の Bind 規則。[`ResultFormats`]）。
//! 対応型は `WireType::Text`（UTF-8 生バイト）のみで、`Id`（`numeric`）・
//! `Vector`（text 表現だが値はベクトル）・`Computed`（実行時型で静的に決まらない）
//! はいずれもバイナリ非対応として事前検査（[`validate_binary_formats`]）で
//! `0A000` に拒否する（spec の WIRE-14 が定める対応範囲。`VECTOR` 列の独自
//! バイナリ表現は定義しない）。8 型のレイアウト関数（[`binary`]）は
//! `WireType` 側の対応拡大（#895・NOSQL-13 ポインタ）に備えた部品として先行
//! 提供するが、本モジュールが実際に結線するのは `Text` のみ。
//!
//! **本モジュール単独では wire 経由でバイナリ形式を要求する経路が無い**
//! （拡張クエリプロトコルの Bind／Describe は #933・#934 が未実装。
//! `protocol_dispatch::classify` が `'B'` を `0A000` で拒否する）。本 Issue の
//! 範囲は結果側エンコーダの提供までで、Bind の結果形式コードから本 API への
//! 結線・同期回復（`0A000` 後の接続維持）は #934 の担当。
//!
//! サイズ安全: フレーム長は `i32::try_from`/`checked_add` で算出し、超過は
//! `Err(EncodeError::FrameTooLarge)` とする（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」。応答側だが同じ規律を踏襲し `as i32` によるオーバー
//! フローを避ける）。

use engine::error_format::ErrorClass;
use engine::sql::exec::{Cell, ColumnMeta, ResultRow};

/// 応答メッセージ組み立て時のフレーム長超過エラー。呼び出し元
/// （[`crate::simple_query`]）はこれを内部エラー（`XX000`）として扱い、panic
/// させない。
#[derive(Debug)]
pub struct EncodeError;

/// 結果列 1 個の応答形式（PostgreSQL Bind の format code。WIRE-14）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatCode {
    Text,
    Binary,
}

/// バイナリ形式（WIRE-14）の解決・事前検査で生じる分類済みエラー。
/// `error_class()` で `engine::error_format::ErrorClass` へ写像し、呼び出し元
/// （`crate::simple_query`／Bind 結線先の #934）が `wire_code` を一意に決定
/// できるようにする。メッセージには要求者自身の列番号までしか含めない
/// （テーブル名・他テナントの存在情報を含めない。`.claude/rules/security.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFormatError {
    /// 形式コードの個数が列数と一致しない（0 個・1 個・列数と同数のいずれでもない）。
    FormatCountMismatch,
    /// 形式コードの値が {0, 1} 以外。
    InvalidFormatCode,
    /// 指定列がバイナリ形式に非対応（[`column_binary_support`] が `false`）。
    UnsupportedType { column_index: usize },
}

impl BinaryFormatError {
    /// `wire_code` 分類。個数不一致・不正値は untrusted な Bind 入力の構文
    /// 違反として `08P01`（`ProtocolViolation`）、非対応型は fail-closed の
    /// 意図的な拒否として `0A000`（`FeatureNotSupported`）に写像する。
    ///
    /// PostgreSQL 本体は不正な format code 値を `22023` で返すが、本リポの
    /// `ErrorClass::OperationIdContentMismatch` は `22023` を専有しており
    /// 意味が異なるため、`ErrorClass` を増やさず本モジュールの fail-closed
    /// 方針（構文違反は `08P01`）に寄せる（設計判断の詳細は
    /// `docs/design/wire-binary-format.md` 参照）。
    pub const fn error_class(self) -> ErrorClass {
        match self {
            BinaryFormatError::FormatCountMismatch => ErrorClass::ProtocolViolation,
            BinaryFormatError::InvalidFormatCode => ErrorClass::ProtocolViolation,
            BinaryFormatError::UnsupportedType { .. } => ErrorClass::FeatureNotSupported,
        }
    }
}

/// Bind が送る結果形式コード列（PostgreSQL の規約。WIRE-14）。列ごとへの解決
/// （[`ResultFormats::resolve`]）までを本型に閉じ、解決結果 `Vec<FormatCode>`
/// の長さは列数（`i16` で有界）に比例するのみで、`codes` スライス自体の長さ
/// 上限検証は untrusted な Bind 本文を解析する側（#934）の責務とする
/// （本型は受け取ったスライス長に比例した確保を行わない）。
#[derive(Debug, Clone, Copy)]
pub struct ResultFormats<'a> {
    codes: &'a [i16],
}

impl<'a> ResultFormats<'a> {
    /// Bind から受け取った形式コード列をそのまま保持する。
    pub const fn new(codes: &'a [i16]) -> Self {
        ResultFormats { codes }
    }

    /// 列ごとの [`FormatCode`] へ解決する（PostgreSQL の Bind 規則）。
    /// - 0 個 → 全列 [`FormatCode::Text`]
    /// - 1 個 → 全列に同じ値を適用
    /// - 列数と同数 → 列ごとに適用
    /// - それ以外の個数 → [`BinaryFormatError::FormatCountMismatch`]
    /// - 値が {0, 1} 以外 → [`BinaryFormatError::InvalidFormatCode`]
    pub fn resolve(&self, column_count: usize) -> Result<Vec<FormatCode>, BinaryFormatError> {
        let to_format = |raw: i16| -> Result<FormatCode, BinaryFormatError> {
            match raw {
                0 => Ok(FormatCode::Text),
                1 => Ok(FormatCode::Binary),
                _ => Err(BinaryFormatError::InvalidFormatCode),
            }
        };
        match self.codes.len() {
            0 => Ok(vec![FormatCode::Text; column_count]),
            1 => {
                // `get(0)` で明示的に取得する（untrusted 由来スライスへの `[]` 直接
                // アクセスを避ける規律。`.claude/rules/coding-rust.md`）。
                let raw = *self
                    .codes
                    .first()
                    .ok_or(BinaryFormatError::FormatCountMismatch)?;
                let format = to_format(raw)?;
                Ok(vec![format; column_count])
            }
            n if n == column_count => self.codes.iter().copied().map(to_format).collect(),
            _ => Err(BinaryFormatError::FormatCountMismatch),
        }
    }
}

/// バイナリ形式を要求された列がすべて対応型であることを、`RowDescription` を
/// 送出する前に検査する（WIRE-14）。1 バイトも送る前に判定することで、応答
/// 列の一部を送ってからエラーにする（フレーム崩壊）事態を避ける。
///
/// 判定は公告型（[`ColumnMeta`]／[`WireType`]）で静的に行い、実行時の
/// `Cell` は参照しない（型公告と実値の不一致を防ぐ）。
///
/// `columns.len() != formats.len()` は本来 [`ResultFormats::resolve`] の
/// 呼び出し規約違反（Bind 形式コードの解決結果が列数と食い違う）だが、
/// `.zip()` は短い方に合わせて黙って打ち切るため、この不一致をここで
/// 検査しないと後段のエンコーダまで持ち越され `EncodeError`（`XX000`）
/// として現れてしまう。入力由来の不整合は `08P01`
/// （[`BinaryFormatError::FormatCountMismatch`]）として、事前検査という
/// 本関数の契約どおりここで拒否する。
pub fn validate_binary_formats(
    columns: &[ColumnMeta],
    formats: &[FormatCode],
) -> Result<(), BinaryFormatError> {
    if columns.len() != formats.len() {
        return Err(BinaryFormatError::FormatCountMismatch);
    }
    for (index, (meta, format)) in columns.iter().zip(formats.iter()).enumerate() {
        if matches!(format, FormatCode::Binary) && !column_binary_support(meta) {
            return Err(BinaryFormatError::UnsupportedType {
                column_index: index,
            });
        }
    }
    Ok(())
}

/// PostgreSQL の send 関数と同じレイアウトで型ごとのバイナリ表現を組み立てる
/// 純関数群（WIRE-14）。本 Issue で `WireType` 側に結線されるのは `text`
/// （[`text`]）のみで、他の型は `WireType`／`Cell` 拡張（#895・NOSQL-13
/// ポインタ）が使う部品として先行提供する。
pub mod binary {
    /// `int4` のバイナリ表現（4 バイト・ビッグエンディアン 2 の補数）。
    pub fn int4(v: i32) -> [u8; 4] {
        v.to_be_bytes()
    }

    /// `int8` のバイナリ表現（8 バイト・ビッグエンディアン 2 の補数）。
    pub fn int8(v: i64) -> [u8; 8] {
        v.to_be_bytes()
    }

    /// `float4` のバイナリ表現（IEEE 754 単精度のビット列をそのままビッグ
    /// エンディアンで送る。NaN／±Inf もビット列のまま送出する）。
    pub fn float4(v: f32) -> [u8; 4] {
        v.to_bits().to_be_bytes()
    }

    /// `float8` のバイナリ表現（IEEE 754 倍精度のビット列。float4 と同じ方針）。
    pub fn float8(v: f64) -> [u8; 8] {
        v.to_bits().to_be_bytes()
    }

    /// `bool` のバイナリ表現（1 バイト。`true` → `0x01`、`false` → `0x00`）。
    pub fn bool_(v: bool) -> [u8; 1] {
        [if v { 1 } else { 0 }]
    }

    /// `bytea` のバイナリ表現（生バイトをそのまま返す。長さ付与は呼び出し元）。
    pub fn bytea(v: &[u8]) -> &[u8] {
        v
    }

    /// `uuid` のバイナリ表現（16 バイトをそのまま返す）。
    pub fn uuid(v: [u8; 16]) -> [u8; 16] {
        v
    }

    /// `text` のバイナリ表現（UTF-8 生バイト。PostgreSQL の text send と同一。
    /// [`super::encode_data_row_into_with_formats`] が結線する唯一の型）。
    pub fn text(v: &str) -> &[u8] {
        v.as_bytes()
    }
}

/// `RowDescription` が公告する PostgreSQL 型（型写像表の単一情報源。
/// Issue #762・NOSQL-11）。`crate::http::query::response`（NoSQL 表層の JSON
/// 応答スキーマ `columns[].type`）も本 enum を経由して同じ写像を参照し、
/// wire 側 `RowDescription` と JSON `columns` の型名が乖離しない構造にする
/// （2 箇所に写像表を持たない）。`#[deny(clippy::wildcard_enum_match_arm)]`
/// を付けた網羅 `match`（`http/status.rs` と同方針）で
/// [`ColumnMeta`] に variant が増えたら両表層が同時にコンパイルエラーになる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireType {
    /// `ColumnMeta::Id`。engine の行 ID は `u64` 全域（`u64::MAX` を含む）を
    /// 有効値とするため、符号付き 64bit の `int8`（OID 20）では表現できない
    /// 値が生じる（PR #210 レビュー指摘）。
    Numeric,
    /// `ColumnMeta::Scalar{ty: Text}`／`Scalar{ty: Vector(_)}`／`Computed{..}`。
    Text,
}

impl WireType {
    /// PostgreSQL 型 OID（`pg_type.oid`）。
    pub(crate) const fn oid(self) -> i32 {
        match self {
            WireType::Numeric => 1700,
            WireType::Text => 25,
        }
    }

    /// 型長（`pg_type.typlen`）。可変長型のみを扱うためいずれも `-1`。
    pub(crate) const fn typlen(self) -> i16 {
        match self {
            WireType::Numeric | WireType::Text => -1,
        }
    }

    /// PostgreSQL 型名（`pg_type.typname`）。JSON 応答スキーマ
    /// （`crate::http::query::response`）の `columns[].type` が使う。
    pub(crate) const fn pg_type_name(self) -> &'static str {
        match self {
            WireType::Numeric => "numeric",
            WireType::Text => "text",
        }
    }

    /// OID からの逆引き（単体テスト専用。結合テスト側は独立の固定表を持つ
    /// ため非テストビルドでは未使用となり `#[cfg(test)]` で dead_code を
    /// 回避する）。
    #[cfg(test)]
    pub(crate) fn from_oid(oid: i32) -> Option<Self> {
        match oid {
            1700 => Some(WireType::Numeric),
            25 => Some(WireType::Text),
            _ => None,
        }
    }

    /// この型がバイナリ形式（format code 1）に対応するか（WIRE-14）。
    /// 網羅 `match`（`#[deny(clippy::wildcard_enum_match_arm)]`）にし、
    /// `WireType` に variant が増えたとき（#895・NOSQL-13 ポインタ）に
    /// バイナリ可否の決定漏れをコンパイルエラーで検出する。
    #[deny(clippy::wildcard_enum_match_arm)]
    pub(crate) const fn supports_binary(self) -> bool {
        match self {
            // `id` は `numeric` として公告しており、WIRE-14 は `NUMERIC` を
            // 非対応型としている（#895 で `int8` へ変わったら見直す）。
            WireType::Numeric => false,
            // `text` は UTF-8 生バイトがそのままバイナリ表現（PostgreSQL の
            // text send と同じ）。ただし `ColumnMeta::Scalar{ty: Vector(_)}`・
            // `Computed` は公告が text でも本来の値がベクトル／実行時型で
            // あるため、型そのものの対応可否とは別に列種別で判定する
            // （[`column_binary_support`] 参照）。
            WireType::Text => true,
        }
    }
}

/// `ColumnMeta` 1 個がバイナリ形式に対応するか（WIRE-14）。`Id`・
/// `Scalar{ty: Text}` は [`column_wire_type`] が公告する [`WireType`] の
/// [`WireType::supports_binary`] へそのまま委譲できるが、
/// `Scalar{ty: Vector(_)}`・`Computed` は公告 OID が `text` でも本来の値が
/// ベクトル／実行時型であるため、`WireType` だけでは判定できず列種別を
/// 直接判定する必要がある（[`column_wire_type`] と同じ「網羅 `match` で
/// variant 増加をコンパイルエラーにする」方針を踏襲）。
#[deny(clippy::wildcard_enum_match_arm)]
pub(crate) fn column_binary_support(meta: &ColumnMeta) -> bool {
    match meta {
        ColumnMeta::Id => column_wire_type(meta).supports_binary(),
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Text,
            ..
        } => column_wire_type(meta).supports_binary(),
        // `VECTOR` 列はバイナリ非対応として `0A000` で拒否する（spec の
        // WIRE-14 決定。独自のバイナリ表現は定義しない）。公告 OID
        // （`WireType::Text`）は `supports_binary() == true` だが、値の実体が
        // ベクトルであるため `WireType` の判定を上書きして拒否する。
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Vector(_),
            ..
        } => false,
        // `BOOLEAN` 列（TABLE-13・TASK-196・Issue #883）は本 Issue（#936・
        // WIRE-14）の策定時点では未存在の型のため、バイナリ表現は spec 側で
        // 未決定。公告 OID（`WireType::Text`）は `supports_binary() == true`
        // だが、`VECTOR` と同様に値の実体が `Text` の生バイト表現とは異なる
        // ため fail-closed で非対応とする。
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Boolean,
            ..
        } => false,
        // `BYTEA` 列（Issue #886）は本 Issue（#936・WIRE-14）の策定時点では
        // 未存在の型のため、バイナリ表現は spec 側で未決定。公告 OID
        // （`WireType::Text`）は `supports_binary() == true` だが、値の実体は
        // 生バイト列の hex テキスト表現（`Cell::Bytes`）であり `Text` の
        // UTF-8 生バイト表現とは異なるため、`VECTOR`・`BOOLEAN` と同様に
        // fail-closed で非対応とする（RowDescription への専用 OID 公告は
        // Issue #895 の担当）。
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Bytea,
            ..
        } => false,
        // `ENUM` 列（TABLE-14・TASK-198、Issue #890）も同様の理由で
        // fail-closed に非対応とする。値は `Cell::Text`（TEXT と同一表示形）に
        // 写像されるが、`ColumnMeta::Scalar` としては別 variant であるため
        // `Text` 分岐へは流れ込まない（多層防御。バイナリ指定は
        // `BinaryFormatError::UnsupportedType`（`0A000`）で拒否する）。
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Enum(_),
            ..
        } => false,
        // `ARRAY` 列（TABLE-14・Issue #888）も本 Issue（#936・WIRE-14）の
        // 策定時点では未存在の型のため、バイナリ表現は spec 側で未決定。
        // `VECTOR`・`BOOLEAN`・`BYTEA` と同様に fail-closed で非対応とする。
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Array(_),
            ..
        } => false,
        // `JSON`／`JSONB` 列（TABLE-14・Issue #889）も同じ理由で fail-closed に
        // 非対応とする（値の実体が `Cell::Json` の格納テキストであり `Text` の
        // 単純な UTF-8 生バイト表現と意味論が異なるため。RowDescription への
        // 専用 OID 公告は Issue #895 の担当）。
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Json | engine::catalog::ColumnType::Jsonb,
            ..
        } => false,
        // `NUMERIC` 列（TABLE-13〔検討中〕・TASK-197、Issue #885）も本 Issue
        // （#936・WIRE-14）の策定時点では未存在の型のため、バイナリ表現は
        // spec 側で未決定。値の実体が `Cell::Numeric`（`Decimal` の正規テキスト）
        // であり `Text` の単純な UTF-8 生バイト表現とは異なるため、他の後発型と
        // 同様に fail-closed で非対応とする（RowDescription への専用 OID
        // 〔OID 1700〕公告は Issue #895 の担当）。
        ColumnMeta::Scalar {
            ty: engine::catalog::ColumnType::Numeric { .. },
            ..
        } => false,
        // 実行時型（Float/Bool/Vector）が静的に決まらないため fail-closed
        // で非対応とする（#895 で型情報が付いたら見直す）。
        ColumnMeta::Computed { .. } => false,
    }
}

/// `ColumnMeta` 1 個が公告する [`WireType`]（本モジュール先頭の型写像表を参照）。
#[deny(clippy::wildcard_enum_match_arm)]
pub(crate) fn column_wire_type(meta: &ColumnMeta) -> WireType {
    match meta {
        ColumnMeta::Id => WireType::Numeric, // u64 全域を表現するため int8 ではなく numeric
        ColumnMeta::Scalar { .. } => WireType::Text, // Vector も text 表現で返す
        ColumnMeta::Computed { .. } => WireType::Text,
    }
}

pub(crate) fn column_name(meta: &ColumnMeta) -> &str {
    match meta {
        ColumnMeta::Id => "id",
        ColumnMeta::Scalar { name, .. } => name.as_str(),
        ColumnMeta::Computed { name } => name.as_str(),
    }
}

/// メッセージ長フィールド（自身の 4 バイトを含む i32）を `checked` に計算する。
/// `crate::error_response`（TASK-153・ERR-1）も同じ `checked` 方式を踏襲するため
/// crate 内に限り公開する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub(crate) fn frame_len(body_len: usize) -> Result<i32, EncodeError> {
    let total = body_len.checked_add(4).ok_or(EncodeError)?;
    i32::try_from(total).map_err(|_| EncodeError)
}

/// `RowDescription`（'T'）を組み立てる。全列テキスト形式（format code 0）の
/// 薄いラッパーで、[`encode_row_description_with_formats`] を呼ぶ
/// （既存呼び出し元・テストとの互換のため残す。生成バイト列は完全に同一。
/// WIRE-14・Issue #936）。
pub fn encode_row_description(columns: &[ColumnMeta]) -> Result<Vec<u8>, EncodeError> {
    let formats = vec![FormatCode::Text; columns.len()];
    encode_row_description_with_formats(columns, &formats)
}

/// `RowDescription`（'T'）を列ごとの [`FormatCode`] を反映して組み立てる
/// （WIRE-14）。フィールド数は `i16` に収まる必要がある（カタログの列数上限で
/// 有界だが、念のため `try_from` で検証する）。`formats.len() != columns.len()`
/// は呼び出し元の内部不整合として `EncodeError`（`XX000`）とする
/// （untrusted 入力由来のバイナリ形式検査は [`validate_binary_formats`] が
/// 別途 `RowDescription` 送出前に済ませている前提）。
pub fn encode_row_description_with_formats(
    columns: &[ColumnMeta],
    formats: &[FormatCode],
) -> Result<Vec<u8>, EncodeError> {
    if columns.len() != formats.len() {
        return Err(EncodeError);
    }
    let field_count = i16::try_from(columns.len()).map_err(|_| EncodeError)?;
    let mut body = Vec::new();
    body.extend_from_slice(&field_count.to_be_bytes());
    for (meta, format) in columns.iter().zip(formats.iter()) {
        let name = column_name(meta);
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table OID
        body.extend_from_slice(&0i16.to_be_bytes()); // attr number
        let wire_type = column_wire_type(meta);
        body.extend_from_slice(&wire_type.oid().to_be_bytes());
        body.extend_from_slice(&wire_type.typlen().to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        let format_code: i16 = match format {
            FormatCode::Text => 0,
            FormatCode::Binary => 1,
        };
        body.extend_from_slice(&format_code.to_be_bytes());
    }
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'T');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `Cell` の text フォーマット表現。`Null` は `None`（`DataRow` の -1 長へ写像）。
///
/// `Cell::Integer` は `u64` 全域（`u64::MAX` を含む）を保持しうる。本モジュール
/// は `id` 列を `numeric`（OID 1700, 型写像表参照）として公告しており、
/// `numeric` の text 表現は符号無し10進整数をそのまま送出してよい（`int8` の
/// ような signed 64bit 制約が無い）ため、`i64` への変換は行わず値域制限なく
/// `to_string()` する（PR #210 レビュー指摘: 旧実装は `i64::try_from` で
/// `i64::MAX` 超の正当な ID を `EncodeError`/`XX000` にしていた）。
fn cell_to_text(cell: &Cell) -> Result<Option<String>, EncodeError> {
    match cell {
        Cell::Null => Ok(None),
        Cell::Integer(v) => Ok(Some(v.to_string())),
        Cell::Text(s) => Ok(Some(s.clone())),
        Cell::Vector(v) => {
            let joined = v
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Ok(Some(format!("[{joined}]")))
        }
        Cell::Float(f) => Ok(Some(f.to_string())),
        Cell::Bool(b) => Ok(Some(if *b { "t".to_string() } else { "f".to_string() })),
        Cell::Array(array_value) => Ok(Some(pg_array_text(array_value))),
        // `BYTEA` のテキスト表現は PostgreSQL 既定の `bytea_output=hex`（`\x` ＋
        // 小文字 16 進）と同形にする（B5・Issue #886）。
        Cell::Bytes(b) => Ok(Some(engine::bytea::format_hex_text(b))),
        // `JSON`／`JSONB` の text 表現は格納テキストをそのまま出力する
        // （TABLE-14・Issue #889。`JSON` は入力テキスト保持・`JSONB` は正規化
        // 済みテキストのため、いずれもここで再シリアライズしない）。
        Cell::Json(s) => Ok(Some(s.clone())),
        // NUMERIC 列の text フォーマット表現は正規テキスト（`Decimal::Display`）
        // をそのまま送る（TABLE-13〔検討中〕・TASK-197、Issue #885）。
        Cell::Numeric(d) => Ok(Some(d.to_string())),
    }
}

/// 配列列（TABLE-14・TASK-198、Issue #888・D-A4）を PostgreSQL 配列テキスト形式
/// （`{a,b}`）へ描画する。空配列は `{}`、`BOOLEAN` 要素は `t`/`f`。`TEXT` 要素の
/// うち、空文字列・`,{}"\` や空白を含むもの・大小無視で `NULL` に一致するものは
/// `"..."` で囲み `"`・`\` をバックスラッシュでエスケープする（`RowDescription`
/// の OID は他の非 VECTOR スカラー列と同じ text（25）のまま変更しない。WIRE-13）。
fn pg_array_text(array_value: &engine::row_codec::ArrayValue) -> String {
    use engine::row_codec::ArrayValue;
    let elements: Vec<String> = match array_value {
        ArrayValue::Text(items) => items.iter().map(|s| quote_pg_array_text_elem(s)).collect(),
        ArrayValue::Bool(items) => items
            .iter()
            .map(|b| if *b { "t".to_string() } else { "f".to_string() })
            .collect(),
    };
    format!("{{{}}}", elements.join(","))
}

/// [`pg_array_text`] の TEXT 要素 1 個の引用要否判定・エスケープ処理。
fn quote_pg_array_text_elem(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `DataRow`（'D'）を `out` の末尾へ追記する（Issue #481。行ごとに新規
/// `Vec<u8>` を確保していた旧 [`encode_data_row`] を、呼び出し元
/// （`crate::response_buffer::ResponseBuffer` 経由の
/// `crate::simple_query::respond_query_result`）が持つ 1 個のバッファへ
/// 直接組み立てる形へ置き換えたもの。`row.cells` は呼び出し元の
/// `QueryResult::columns` と同じ順序・同じ長さであることを engine 側が保証する
/// （`sql::exec::execute_statement` の投影順。ポインタ: TASK-75・SQL-1〜4）。
///
/// 長さフィールド（フレーム先頭 4 バイト）は本体を書き終えるまで値が
/// 定まらないため、まずプレースホルダを push してから最後に
/// `out.get_mut` で backpatch する（`unwrap`/`[]` を使わない。取得できない
/// ことは有り得ないが、regression で `out` の構造が壊れた場合に panic では
/// なく `EncodeError` へ倒す）。
///
/// **失敗時は `out` を呼び出し前の長さへ必ず `truncate` してから返す**
/// （呼び出し元が完成済みフレームだけを送出できるようにするための契約。
/// 部分フレームを絶対に残さない）。
///
/// 全列テキスト形式（format code 0）の薄いラッパーで、
/// [`encode_data_row_body`] を「全列テキスト」の形式解決関数付きで呼ぶ
/// （既存呼び出し元・テストとの互換のため残す。生成バイト列は完全に同一。
/// WIRE-14・Issue #936）。
///
/// 呼び出しのたびに `formats` 用の `Vec<FormatCode>` を新規確保していた
/// 旧実装（[`encode_data_row_into_with_formats`] 経由）は、結果行数に
/// 比例したヒープ確保を発生させ「行ごとの `Vec<u8>` 確保を避ける」
/// （Issue #481）の最適化を損なっていたため、形式コードを列インデックスから
/// 直接解決する [`encode_data_row_body`] を共有する形へ変更した
/// （codex-review 指摘・PR #998）。
pub fn encode_data_row_into(row: &ResultRow, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    encode_data_row_body(row, out, |_| FormatCode::Text)
}

/// `DataRow`（'D'）を列ごとの [`FormatCode`] を反映して `out` の末尾へ追記する
/// （WIRE-14）。`formats.len() != row.cells.len()` は呼び出し元の内部不整合
/// として `EncodeError`（`XX000`）とする（untrusted 入力由来のバイナリ形式
/// 検査は [`validate_binary_formats`] が別途 `RowDescription` 送出前に
/// 済ませている前提）。
///
/// [`Null`](Cell::Null) セルは形式コードにかかわらず長さ `-1` を書く
/// （PostgreSQL の規約）。テキスト列のバイナリ表現は [`cell_to_text`] と
/// 同じ UTF-8 バイトをそのまま長さ付きで書く（[`binary::text`]）。
///
/// **失敗時は `out` を呼び出し前の長さへ必ず `truncate` してから返す**
/// （[`encode_data_row_into`] と同じ契約。部分フレームを絶対に残さない）。
pub fn encode_data_row_into_with_formats(
    row: &ResultRow,
    formats: &[FormatCode],
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    if row.cells.len() != formats.len() {
        return Err(EncodeError);
    }
    // `formats` は呼び出し元内部で長さ一致を確認済みだが、添字アクセス
    // （`[]`）は使わず `get` で明示的に処理する（coding-rust 規約）。
    encode_data_row_body(row, out, |i| {
        formats.get(i).copied().unwrap_or(FormatCode::Text)
    })
}

/// [`encode_data_row_into`]／[`encode_data_row_into_with_formats`] が共有する
/// フレーム組み立て本体。`format_at(i)` は `row.cells` の列インデックス `i`
/// に対する形式コードを返す（全列テキストの呼び出し元は割り当てなしの
/// 定数クロージャを渡せる）。
///
/// **失敗時は `out` を呼び出し前の長さへ必ず `truncate` してから返す**
/// （部分フレームを絶対に残さない）。
fn encode_data_row_body<F>(
    row: &ResultRow,
    out: &mut Vec<u8>,
    format_at: F,
) -> Result<(), EncodeError>
where
    F: Fn(usize) -> FormatCode,
{
    let start = out.len();
    let field_count = match i16::try_from(row.cells.len()) {
        Ok(n) => n,
        Err(_) => {
            out.truncate(start);
            return Err(EncodeError);
        }
    };

    let mut write_body = || -> Result<(), EncodeError> {
        out.push(b'D');
        // 長さフィールドのプレースホルダ（後で backpatch）。
        out.extend_from_slice(&0i32.to_be_bytes());
        let body_start = out.len();
        out.extend_from_slice(&field_count.to_be_bytes());
        for (i, cell) in row.cells.iter().enumerate() {
            match format_at(i) {
                FormatCode::Text => match cell_to_text(cell)? {
                    None => out.extend_from_slice(&(-1i32).to_be_bytes()),
                    Some(text) => {
                        let bytes = text.as_bytes();
                        let len = i32::try_from(bytes.len()).map_err(|_| EncodeError)?;
                        out.extend_from_slice(&len.to_be_bytes());
                        out.extend_from_slice(bytes);
                    }
                },
                FormatCode::Binary => match cell {
                    Cell::Null => out.extend_from_slice(&(-1i32).to_be_bytes()),
                    // `validate_binary_formats` が事前検査を通した経路のみが
                    // ここへ到達する契約であり、バイナリ対応列は
                    // `Cell::Text`（`WireType::Text` かつ `ColumnMeta::Scalar
                    // {ty: Text}`）に限られる（`column_binary_support`
                    // 参照）。それ以外の `Cell` variant が来た場合は
                    // 呼び出し元の契約違反として fail-closed に拒否する
                    // （黙って text/他表現へフォールバックしない）。
                    Cell::Text(s) => {
                        let bytes = binary::text(s);
                        let len = i32::try_from(bytes.len()).map_err(|_| EncodeError)?;
                        out.extend_from_slice(&len.to_be_bytes());
                        out.extend_from_slice(bytes);
                    }
                    Cell::Integer(_)
                    | Cell::Vector(_)
                    | Cell::Float(_)
                    | Cell::Bool(_)
                    | Cell::Array(_)
                    | Cell::Bytes(_)
                    | Cell::Json(_)
                    | Cell::Numeric(_) => {
                        return Err(EncodeError);
                    }
                },
            }
        }
        let body_len = out.len().checked_sub(body_start).ok_or(EncodeError)?;
        let total_len = frame_len(body_len)?;
        let len_pos = body_start.checked_sub(4).ok_or(EncodeError)?;
        let len_slice = out
            .get_mut(len_pos..len_pos.checked_add(4).ok_or(EncodeError)?)
            .ok_or(EncodeError)?;
        len_slice.copy_from_slice(&total_len.to_be_bytes());
        Ok(())
    };

    match write_body() {
        Ok(()) => Ok(()),
        Err(e) => {
            out.truncate(start);
            Err(e)
        }
    }
}

/// `DataRow`（'D'）を 1 行ぶん新規 `Vec<u8>` として組み立てる。
/// [`encode_data_row_into`] を呼ぶ薄いラッパーで、既存呼び出し元・テストとの
/// 互換のため残す（生成バイト列は完全に同一）。
pub fn encode_data_row(row: &ResultRow) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    encode_data_row_into(row, &mut out)?;
    Ok(out)
}

/// `CommandComplete`（'C'）。`tag` は `SELECT n` / `INSERT 0 1` / `SET` /
/// `CREATE FUNCTION` 等、簡易クエリプロトコルが規定するコマンドタグ文字列。
pub fn encode_command_complete(tag: &str) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::with_capacity(tag.len() + 1);
    body.extend_from_slice(tag.as_bytes());
    body.push(0);
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'C');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `ReadyForQuery`（'Z'）。固定長 6 バイト（タグ 1 + 長さ 4 + トランザクション
/// 状態 1）。`crate::handshake::write_ready_for_query` から使う唯一のレイアウト
/// 実体（Issue #481。以前は同モジュール内にバイト列組み立てが個別に存在し、
/// `crate::response_buffer::ResponseBuffer` へ他フレームと同じ形で積める
/// フレームが無かった）。状態は常に `'I'`（idle・トランザクション外）で固定
/// ―― 本実装は明示トランザクション（`BEGIN`/`COMMIT`）を持たないため。
pub fn encode_ready_for_query() -> [u8; 6] {
    let mut msg = [0u8; 6];
    msg[0] = b'Z';
    let len_bytes = 5i32.to_be_bytes();
    msg[1] = len_bytes[0];
    msg[2] = len_bytes[1];
    msg[3] = len_bytes[2];
    msg[4] = len_bytes[3];
    msg[5] = b'I';
    msg
}

/// `EmptyQueryResponse`（'I'）。body なし・長さ固定（4）。
pub fn encode_empty_query_response() -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(b'I');
    msg.extend_from_slice(&4i32.to_be_bytes());
    msg
}

/// `ParseComplete`（'1'）。拡張クエリプロトコルの Parse（Issue #933・TASK-71・
/// WIRE-11）が成功したことを示す固定応答。body なし・長さ固定（4）。
pub fn encode_parse_complete() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b'1';
    let len_bytes = 4i32.to_be_bytes();
    msg[1..5].copy_from_slice(&len_bytes);
    msg
}

/// `ParameterDescription`（'t'）。Describe（'D' 種別 S。Issue #933）が返す
/// パラメータ型 OID 一覧。`$n` 束縛（WIRE-12・#935）は本 Issue の対象外のため
/// `param_oids` は常に空スライスで呼ばれる契約だが、将来の非空呼び出しにも
/// 対応できる汎用実装としておく。件数は `i16` に収まる必要がある。
pub fn encode_parameter_description(param_oids: &[i32]) -> Result<Vec<u8>, EncodeError> {
    let count = i16::try_from(param_oids.len()).map_err(|_| EncodeError)?;
    let mut body = Vec::with_capacity(2 + param_oids.len() * 4);
    body.extend_from_slice(&count.to_be_bytes());
    for oid in param_oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b't');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `NoData`（'n'）。Describe（'D' 種別 S。Issue #933）の対象文が結果列を持たない
/// （`RETURNING` の無い DML・`SET`・`CREATE FUNCTION`・`TRUNCATE`）ことを示す
/// 固定応答。body なし・長さ固定（4）。
pub fn encode_no_data() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b'n';
    let len_bytes = 4i32.to_be_bytes();
    msg[1..5].copy_from_slice(&len_bytes);
    msg
}

/// `ErrorResponse`（'E'）body の `S`（severity）/`C`（sqlstate）/`M`（message）の
/// 3 フィールドを `body` へ書き込む。フィールド終端（末尾の NUL）・フレーム化
/// （`E` タグ・長さ）は呼び出し元が行う。
///
/// `severity` は呼び出し元が明示的に決定した値をそのまま書き込む（本関数自身は
/// `ErrorClass` 等から severity を判定しない）。接続を閉じる契約の分類
/// （`ErrorClass::ConnectionLimitExceeded`＝`53300`）は `FATAL`、それ以外は
/// `ERROR` を渡す契約（codex-review P1 指摘対応・PR #258。`crate::error_response`
/// の呼び出し元が `severity_for` で決定し本関数へ渡す。写像を本関数側で持たせると
/// `limits.rs::reject_too_many_connections` の独自実装〔`FATAL` 固定〕と分類の
/// 判定基準が 2 箇所に分散するため、判定は呼び出し元に閉じる）。
///
/// [`encode_error_response`] と `crate::error_response::encode`（TASK-153・ERR-1。
/// `ErrorClass` から severity/SQLSTATE を一元的に決定する横断写像）が共有する
/// 唯一のレイアウト実体（codex-review Low 指摘対応・PR #101。以前は両者が同じ
/// S/C/M 書き込みをそれぞれ自前で複製しており、フィールドレイアウト変更時に
/// 乖離しうる状態だった）。crate 内に限り公開する。
pub(crate) fn push_s_c_m_fields(body: &mut Vec<u8>, severity: &str, sqlstate: &str, message: &str) {
    body.push(b'S');
    body.extend_from_slice(severity.as_bytes());
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(sqlstate.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message.as_bytes());
    body.push(0);
}

/// `ErrorResponse`（'E'）の `D`（detail）フィールドを 1 個 `body` へ追記する
/// （タグ 1 バイト＋値＋NUL 終端。[`push_s_c_m_fields`] と同じ「フィールド
/// 終端・フレーム化は呼び出し元が担う」方針）。
///
/// 呼び出し元は `crate::error_response::encode_with_detail` のみ（ERR-5・
/// TASK-153 ポインタ。`RECOVER-5` (3) の commit 後 panic 緊急応答で
/// `state=may_be_committed` を搬送する用途）。`crate` 内限定公開とし、
/// wire-server 外へは公開しない。
pub(crate) fn push_d_field(body: &mut Vec<u8>, detail: &str) {
    body.push(b'D');
    body.extend_from_slice(detail.as_bytes());
    body.push(0);
}

/// `ErrorResponse`（'E'）を `S`/`C`/`M` の 3 フィールドのみ、severity `ERROR`
/// 固定で組み立てる（`sqlstate`/`message`。他テナント・存在情報は含めない）。
///
/// **本 crate の通常・緊急いずれの送出経路もこの関数は経由しない**
/// （TASK-153・ERR-1・codex-review P1 指摘対応・PR #258）。`handshake.rs::
/// write_error_response`・`crate::simple_query::build_emergency_response_bytes`
/// はいずれも `crate::error_response::encode`（`ErrorClass` から severity・
/// SQLSTATE を一元的に決定する横断写像。severity は分類ごとに変わりうるため、
/// 本関数の `ERROR` 固定では `ErrorClass::ConnectionLimitExceeded` の `FATAL`
/// 契約を表現できない）を使う。フィールド書き込みの実体
/// （[`push_s_c_m_fields`]）は共有するため、`sqlstate`/`message` を直接受け取る
/// 薄い公開 API として残す（severity を常に `ERROR` に固定できる用途向け）。
///
/// フレーム長の算出は既存 encoder（[`encode_command_complete`] 等）と同じ
/// `checked` 方式（[`frame_len`]）を使い、`as i32` によるオーバーフローを
/// 起こさない（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」。
/// 本関数の入力自体は untrusted ではないが、同じ規律を踏襲する）。
pub fn encode_error_response(sqlstate: &str, message: &str) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::new();
    push_s_c_m_fields(&mut body, "ERROR", sqlstate, message);
    body.push(0); // フィールド終端
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'E');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストのみが使うバイト列アサーションヘルパー（`.claude/rules/
    /// coding-rust.md` の添字アクセス禁止は untrusted 受信入力経路が対象だが、
    /// `[]` の使用箇所をレビュー時に P0 判定と混同されないよう `get()` で統一する）。
    fn byte_at(msg: &[u8], idx: usize) -> u8 {
        *msg.get(idx).expect("message too short")
    }

    fn i32_at(msg: &[u8], idx: usize) -> i32 {
        let bytes: [u8; 4] = msg
            .get(idx..idx + 4)
            .expect("message too short")
            .try_into()
            .expect("slice is exactly 4 bytes");
        i32::from_be_bytes(bytes)
    }

    fn i16_at(msg: &[u8], idx: usize) -> i16 {
        let bytes: [u8; 2] = msg
            .get(idx..idx + 2)
            .expect("message too short")
            .try_into()
            .expect("slice is exactly 2 bytes");
        i16::from_be_bytes(bytes)
    }

    fn slice_at(msg: &[u8], idx: usize, len: usize) -> &[u8] {
        msg.get(idx..idx + len).expect("message too short")
    }

    #[test]
    fn row_description_encodes_id_and_text_columns() {
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
        ];
        let msg = encode_row_description(&columns).expect("encode");
        assert_eq!(byte_at(&msg, 0), b'T');
        // フィールド数は body 先頭の i16（type/len/OID ヘッダ直後の先頭 4 バイトが
        // 'T' + length のため、body はインデックス 5 から始まる）。
        assert_eq!(i16_at(&msg, 5), 2);
    }

    #[test]
    fn data_row_null_cell_has_length_negative_one() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        let msg = encode_data_row(&row).expect("encode");
        // 'D' + length(4) + field_count(2) + cell length(4)
        assert_eq!(i32_at(&msg, 7), -1);
    }

    #[test]
    fn data_row_vector_cell_is_bracketed_text() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Vector(vec![1.0, 2.5])],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, "[1,2.5]");
    }

    #[test]
    fn data_row_bool_cell_encodes_t_or_f() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Bool(true), Cell::Bool(false)],
        };
        let msg = encode_data_row(&row).expect("encode");
        // 先頭 field: 'D' + length(4) + field_count(2) = idx 7 から length(4) + 't'
        let first_len = i32_at(&msg, 7) as usize;
        assert_eq!(first_len, 1);
        assert_eq!(slice_at(&msg, 11, first_len), b"t");
    }

    #[test]
    fn data_row_array_text_cell_encodes_pg_array_text_with_quoting() {
        use engine::row_codec::ArrayValue;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Array(ArrayValue::Text(vec![
                "a".to_string(),
                "b c".to_string(),
                "".to_string(),
                "d\"e".to_string(),
                "NULL".to_string(),
            ]))],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, r#"{a,"b c","","d\"e","NULL"}"#);
    }

    #[test]
    fn data_row_array_bool_cell_encodes_t_f() {
        use engine::row_codec::ArrayValue;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Array(ArrayValue::Bool(vec![true, false]))],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, "{t,f}");
    }

    #[test]
    fn data_row_array_empty_cell_encodes_braces_only() {
        use engine::row_codec::ArrayValue;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Array(ArrayValue::Text(vec![]))],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, "{}");
    }

    #[test]
    fn data_row_integer_cell_within_i64_range_encodes_as_decimal() {
        let row = ResultRow {
            id: i64::MAX as u64,
            score: 0.0,
            cells: vec![Cell::Integer(i64::MAX as u64)],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, i64::MAX.to_string());
    }

    #[test]
    fn data_row_integer_cell_beyond_i64_max_encodes_as_decimal() {
        // `id` 列は `numeric`（OID 1700）として公告するため（型写像表参照）、
        // `i64::MAX` を超える正当な `u64` 行 ID も値域制限なくそのまま10進
        // テキストで送出できる（PR #210 レビュー指摘: 旧実装は int8 前提で
        // `EncodeError`/`XX000` にしていた）。
        let row = ResultRow {
            id: u64::MAX,
            score: 0.0,
            cells: vec![Cell::Integer(u64::MAX)],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, u64::MAX.to_string());
    }

    // --- encode_data_row_into（Issue #481）---

    #[test]
    fn encode_data_row_into_matches_encode_data_row_byte_for_byte() {
        let row = ResultRow {
            id: 7,
            score: 0.0,
            cells: vec![Cell::Integer(7), Cell::Text("lang".to_string()), Cell::Null],
        };
        let standalone = encode_data_row(&row).expect("encode standalone");
        let mut out = Vec::new();
        encode_data_row_into(&row, &mut out).expect("encode into");
        assert_eq!(out, standalone);
    }

    #[test]
    fn encode_data_row_into_appends_after_existing_content() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Bool(true)],
        };
        let mut out = b"PREFIX".to_vec();
        encode_data_row_into(&row, &mut out).expect("encode into");
        assert!(out.starts_with(b"PREFIX"));
        assert_eq!(byte_at(&out, 6), b'D');
    }

    #[test]
    fn encode_data_row_into_truncates_back_to_start_on_failure() {
        // field_count は i16 に収まる必要がある（32,768 セルは超過）。
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null; 32_768],
        };
        let mut out = b"KEEP".to_vec();
        let result = encode_data_row_into(&row, &mut out);
        assert!(result.is_err());
        assert_eq!(out, b"KEEP", "failed encode must not leave partial bytes");
    }

    #[test]
    fn ready_for_query_has_fixed_layout() {
        let msg = encode_ready_for_query();
        assert_eq!(msg, [b'Z', 0, 0, 0, 5, b'I']);
    }

    #[test]
    fn command_complete_contains_tag() {
        let msg = encode_command_complete("SELECT 3").expect("encode");
        assert_eq!(byte_at(&msg, 0), b'C');
        assert!(msg.ends_with(b"SELECT 3\0"));
    }

    #[test]
    fn empty_query_response_has_fixed_length_and_no_body() {
        let msg = encode_empty_query_response();
        assert_eq!(msg, vec![b'I', 0, 0, 0, 4]);
    }

    // --- encode_error_response（TASK-97・RECOVER-6、codex-review P1・PR #253 指摘対応）---

    #[test]
    fn error_response_contains_s_c_m_fields_and_terminator_only() {
        let msg = encode_error_response("XX000", "internal error").expect("encode");
        assert_eq!(byte_at(&msg, 0), b'E');

        // body は 'E' + length(4 バイト) の直後（インデックス 5）から始まる。
        let body = slice_at(&msg, 5, msg.len() - 5);
        assert_eq!(body.first().copied(), Some(b'S'));
        assert!(body.windows(6).any(|w| w == b"ERROR\0"));
        assert!(body.windows(6).any(|w| w == b"XX000\0"));
        assert!(body.windows(15).any(|w| w == b"internal error\0"));
        // 通常応答（`encode_error_response`／`crate::error_response::encode`）は
        // `D` を付けない契約（ERR-5。緊急応答チャネルのみ `crate::error_response::
        // encode_with_detail` 経由で `D` を付与する。`crate::error_response`
        // モジュールドキュメント参照）。
        assert!(!body.contains(&b'D'));
        assert_eq!(
            body.last().copied(),
            Some(0),
            "field terminator (double NUL)"
        );

        // 長さフィールドは i32 で self を含む total_len。
        let declared_len = i32_at(&msg, 1) as usize;
        assert_eq!(
            declared_len,
            msg.len() - 1,
            "length field excludes only the leading 'E' type byte"
        );
    }

    // --- ResultFormats::resolve（WIRE-14・Issue #936）---

    #[test]
    fn result_formats_resolve_zero_codes_defaults_all_text() {
        let formats = ResultFormats::new(&[]).resolve(3).expect("resolve");
        assert_eq!(formats, vec![FormatCode::Text; 3]);
    }

    #[test]
    fn result_formats_resolve_single_code_applies_to_all_columns() {
        let formats = ResultFormats::new(&[1]).resolve(3).expect("resolve");
        assert_eq!(formats, vec![FormatCode::Binary; 3]);
    }

    #[test]
    fn result_formats_resolve_per_column_codes() {
        let formats = ResultFormats::new(&[0, 1, 0]).resolve(3).expect("resolve");
        assert_eq!(
            formats,
            vec![FormatCode::Text, FormatCode::Binary, FormatCode::Text]
        );
    }

    #[test]
    fn result_formats_resolve_count_mismatch_is_rejected() {
        let err = ResultFormats::new(&[0, 1]).resolve(3).unwrap_err();
        assert_eq!(err, BinaryFormatError::FormatCountMismatch);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    #[test]
    fn result_formats_resolve_invalid_value_is_rejected() {
        let err = ResultFormats::new(&[2]).resolve(1).unwrap_err();
        assert_eq!(err, BinaryFormatError::InvalidFormatCode);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    // --- validate_binary_formats（WIRE-14）---

    #[test]
    fn validate_binary_formats_accepts_text_column_as_binary() {
        let columns = vec![ColumnMeta::Scalar {
            name: "lang".to_string(),
            ty: engine::catalog::ColumnType::Text,
        }];
        let formats = vec![FormatCode::Binary];
        assert!(validate_binary_formats(&columns, &formats).is_ok());
    }

    #[test]
    fn validate_binary_formats_rejects_id_column_as_binary() {
        let columns = vec![ColumnMeta::Id];
        let formats = vec![FormatCode::Binary];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
    }

    #[test]
    fn validate_binary_formats_rejects_vector_column_as_binary() {
        let columns = vec![ColumnMeta::Scalar {
            name: "embedding".to_string(),
            ty: engine::catalog::ColumnType::Vector(3),
        }];
        let formats = vec![FormatCode::Binary];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
    }

    #[test]
    fn validate_binary_formats_rejects_computed_column_as_binary() {
        let columns = vec![ColumnMeta::Computed {
            name: "expr".to_string(),
        }];
        let formats = vec![FormatCode::Binary];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
    }

    #[test]
    fn validate_binary_formats_rejects_formats_shorter_than_columns() {
        // `.zip()` の共通部分のみ検査すると formats が短い（列数不足）場合に
        // 素通りしてしまう（codex-review 指摘・Issue #936 PR #998）。
        // 事前検査という契約どおり `08P01` で拒否する。
        let columns = vec![
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
            ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
        ];
        let formats = vec![FormatCode::Text];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::FormatCountMismatch);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    #[test]
    fn validate_binary_formats_rejects_formats_longer_than_columns() {
        // formats が列数より多い場合も同様に `08P01` で拒否する
        // （`.zip()` は左側 `columns` の長さで打ち切るため長すぎる側も
        // 素通りしていた）。
        let columns = vec![ColumnMeta::Scalar {
            name: "lang".to_string(),
            ty: engine::catalog::ColumnType::Text,
        }];
        let formats = vec![FormatCode::Text, FormatCode::Text];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::FormatCountMismatch);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    #[test]
    fn validate_binary_formats_accepts_all_text_columns_unconditionally() {
        // バイナリを一切要求しない場合は非対応型が混在していても通る
        // （検査対象は「バイナリを要求された列」のみ）。
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: engine::catalog::ColumnType::Vector(3),
            },
            ColumnMeta::Computed {
                name: "expr".to_string(),
            },
        ];
        let formats = vec![FormatCode::Text; 3];
        assert!(validate_binary_formats(&columns, &formats).is_ok());
    }

    // --- binary モジュール（golden バイト列。WIRE-14）---

    #[test]
    fn binary_int4_golden_bytes() {
        assert_eq!(binary::int4(1), [0x00, 0x00, 0x00, 0x01]);
        assert_eq!(binary::int4(-1), [0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn binary_int8_golden_bytes() {
        assert_eq!(
            binary::int8(-1),
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            binary::int8(1),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]
        );
    }

    #[test]
    fn binary_float8_golden_bytes() {
        // IEEE 754 倍精度で 1.0 は指数部 1023（バイアス済み）・仮数部 0。
        assert_eq!(
            binary::float8(1.0),
            [0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn binary_float4_golden_bytes() {
        assert_eq!(binary::float4(1.0), [0x3f, 0x80, 0x00, 0x00]);
    }

    #[test]
    fn binary_bool_golden_bytes() {
        assert_eq!(binary::bool_(true), [0x01]);
        assert_eq!(binary::bool_(false), [0x00]);
    }

    #[test]
    fn binary_uuid_round_trips() {
        let uuid_bytes: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        assert_eq!(binary::uuid(uuid_bytes), uuid_bytes);
    }

    #[test]
    fn binary_text_is_utf8_bytes() {
        // マルチバイト文字（'é' は UTF-8 で 2 バイト）でも生バイトをそのまま返す。
        assert_eq!(binary::text("é"), "é".as_bytes());
        assert_eq!(binary::text("é").len(), 2);
    }

    #[test]
    fn binary_bytea_is_identity() {
        let data = [1u8, 2, 3];
        assert_eq!(binary::bytea(&data), &data);
    }

    // --- encode_row_description_with_formats / encode_data_row_into_with_formats
    // の全テキスト不変性（受け入れ条件 4・WIRE-14）---

    #[test]
    fn row_description_with_all_text_formats_matches_legacy_encoder() {
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
            ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: engine::catalog::ColumnType::Vector(3),
            },
            ColumnMeta::Computed {
                name: "expr".to_string(),
            },
        ];
        let legacy = encode_row_description(&columns).expect("legacy encode");
        let formats = vec![FormatCode::Text; columns.len()];
        let via_formats =
            encode_row_description_with_formats(&columns, &formats).expect("formats encode");
        assert_eq!(legacy, via_formats);
    }

    #[test]
    fn data_row_with_all_text_formats_matches_legacy_encoder() {
        let row = ResultRow {
            id: 7,
            score: 0.0,
            cells: vec![
                Cell::Integer(7),
                Cell::Text("lang".to_string()),
                Cell::Null,
                Cell::Vector(vec![1.0, 2.5]),
                Cell::Bool(true),
                Cell::Float(1.5),
            ],
        };
        let legacy = encode_data_row(&row).expect("legacy encode");
        let formats = vec![FormatCode::Text; row.cells.len()];
        let mut via_formats = Vec::new();
        encode_data_row_into_with_formats(&row, &formats, &mut via_formats)
            .expect("formats encode");
        assert_eq!(legacy, via_formats);
    }

    #[test]
    fn data_row_into_with_formats_rejects_count_mismatch() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Integer(1)],
        };
        let mut out = b"KEEP".to_vec();
        let result = encode_data_row_into_with_formats(&row, &[], &mut out);
        assert!(result.is_err());
        assert_eq!(out, b"KEEP", "mismatch must not leave partial bytes");
    }

    // --- encode_data_row_into_with_formats の実バイナリ経路（WIRE-14）---

    #[test]
    fn data_row_binary_text_cell_uses_raw_utf8_bytes() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Text("lang".to_string())],
        };
        let mut out = Vec::new();
        encode_data_row_into_with_formats(&row, &[FormatCode::Binary], &mut out).expect("encode");
        // 'D' + length(4) + field_count(2) = idx 7 から cell length(4)。
        let cell_len = i32_at(&out, 7) as usize;
        assert_eq!(cell_len, 4);
        assert_eq!(slice_at(&out, 11, cell_len), b"lang");
    }

    #[test]
    fn data_row_binary_null_cell_has_length_negative_one_regardless_of_format() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        let mut out = Vec::new();
        encode_data_row_into_with_formats(&row, &[FormatCode::Binary], &mut out).expect("encode");
        assert_eq!(i32_at(&out, 7), -1);
    }

    #[test]
    fn data_row_binary_non_text_cell_is_rejected() {
        // `validate_binary_formats` を経由しない直接呼び出しでの契約違反
        // （バイナリ非対応列の実値セル）を fail-closed に拒否する。
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Integer(1)],
        };
        let mut out = b"KEEP".to_vec();
        let result = encode_data_row_into_with_formats(&row, &[FormatCode::Binary], &mut out);
        assert!(result.is_err());
        assert_eq!(out, b"KEEP", "failed encode must not leave partial bytes");
    }

    // --- column_binary_support（WIRE-14）---

    #[test]
    fn column_binary_support_matrix() {
        assert!(!column_binary_support(&ColumnMeta::Id));
        assert!(column_binary_support(&ColumnMeta::Scalar {
            name: "lang".to_string(),
            ty: engine::catalog::ColumnType::Text,
        }));
        assert!(!column_binary_support(&ColumnMeta::Scalar {
            name: "embedding".to_string(),
            ty: engine::catalog::ColumnType::Vector(3),
        }));
        assert!(!column_binary_support(&ColumnMeta::Computed {
            name: "expr".to_string(),
        }));
    }
}
