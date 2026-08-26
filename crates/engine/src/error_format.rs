//! エラー分類共通形式（TASK-152。対象ビヘイビア: ERR-2。ポインタ:
//! `docs/spec/04-behavior/error-format.md`）。
//!
//! 責務境界: engine 各所（`sql::allowlist::SqlSurfaceError`・`tenant::TenantWriteError`
//! 等）が独立に持つ `wire_code()` 実装を、本モジュールの [`ErrorClass`] へ委譲させる
//! ための単一真実源（SSOT）を提供する。`wire_code` 写像が複数箇所へ文字列リテラルで
//! 分散し、決定的分類（同一入力に常に同一 `wire_code`）・一意対応（分類⇔`wire_code`）が
//! 構造的に保証されない状態を防ぐことが目的（ERR-2 が確定として保証する契約は
//! `wire_code` 列のみ）。
//!
//! 分類リストは [`define_error_classes`] マクロの 1 箇所のみで宣言し、`ErrorClass` の
//! 定義・`ALL`・`wire_code`・`label`・`is_err2_table_row` をそこから生成する。分類を
//! 追加・削除すると `ALL` の固定長（`count`）が合わなくなりコンパイルが失敗するため、
//! 「分類は増えたが `ALL` の更新を忘れる」乖離は構造的に発生しない。
//!
//! wire-server 側（`SQLSTATE_*` 定数・`ErrorResponse` 整形）は TASK-153 の管轄で、
//! 本モジュールはそれらを直接呼び出さない（workspace 責務境界: `.claude/rules/
//! coding-rust.md`）。`OperationIdContentMismatch`（ERR-3 管轄）は写像の一意性を
//! 保つために本 enum へ含めるが、これを実際に発生させる経路（`recovery.md`
//! RECOVER-10 の内容照合）は TASK-154 の管轄であり本モジュールでは実装しない。

/// [`ErrorClass`] の宣言と写像（`ALL`・`wire_code`・`label`・`is_err2_table_row`）を
/// **単一の分類リスト**から生成するマクロ。分類・`wire_code`・ラベル・ERR-2 分類表への
/// 掲載有無を 1 箇所に集約し、リスト間の乖離（分類を追加したのに `ALL` へ足し忘れる等）を
/// 構造的に起こせなくする（ERR-2 の一意対応・決定的分類を型で担保するのが本モジュールの
/// 責務）。`count` は `ALL` の固定長配列長であり、分類を増減させたのに更新しなければ
/// 配列長不一致でコンパイルが失敗する。
macro_rules! define_error_classes {
    (
        count = $count:literal;
        $(
            $(#[$variant_doc:meta])*
            $variant:ident => ($wire_code:literal, $label:literal, err2_table = $in_table:literal),
        )+
    ) => {
        /// エラー分類の共通表現。`docs/spec/04-behavior/error-format.md` の分類表
        /// （計 15 行）と、表外の発生条件に対しビヘイビアファイルが定義した拡張分類から成る
        /// （表と拡張の区別は [`ErrorClass::is_err2_table_row`]）。並びは表の掲載順に揃える。
        ///
        /// `#[non_exhaustive]` は付けない。分類の追加は
        /// [`define_error_classes`] のリストへの 1 行追加としてのみ行い、
        /// `wire_code`／`label`／`ALL`／`is_err2_table_row` は同リストから生成されるため
        /// 更新漏れが起こり得ない（`StorageError` と同じ「網羅 `match` を強制する」既定方針）。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorClass {
            $( $(#[$variant_doc])* $variant, )+
        }

        impl ErrorClass {
            /// 全分類（ERR-2 分類表の行 + 拡張分類）。テストでの網羅・一意性検証、
            /// [`ErrorClass::from_wire_code`] の逆引きに使う。
            pub const ALL: [ErrorClass; $count] = [ $( ErrorClass::$variant, )+ ];

            /// SQLSTATE 風の 5 文字コード。ERR-2 が確定として保証する契約そのもの。
            /// 外部状態・時刻・乱数を参照しない純粋な `match`（決定的分類の保証）。
            pub const fn wire_code(self) -> &'static str {
                match self { $( ErrorClass::$variant => $wire_code, )+ }
            }

            /// 非規範の人間可読ラベル（`SCREAMING_SNAKE_CASE`）。診断・ログ用途に限り、
            /// wire プロトコル応答の契約には含めない（確定契約は `wire_code` のみ）。
            pub const fn label(self) -> &'static str {
                match self { $( ErrorClass::$variant => $label, )+ }
            }

            /// ERR-2 の分類表そのものに掲載された分類なら `true`。表外の発生条件に対して
            /// ビヘイビアファイルが独自に定義した拡張分類（例: SQL-13 の
            /// [`ErrorClass::NumericOutOfRange`]）は `false` を返す。spec 表との対応が
            /// 将来ずれた場合にテストで検知するために持つ。
            pub const fn is_err2_table_row(self) -> bool {
                match self { $( ErrorClass::$variant => $in_table, )+ }
            }
        }
    };
}

define_error_classes! {
    count = 16;

    /// 構文上受理された SQL の値・引数が不正（`22000`）。
    InvalidInput => ("22000", "INVALID_INPUT", err2_table = true),
    /// 認証資格情報なし（未認証セッションでのクエリ受信等。`28000`）。
    AuthRequired => ("28000", "AUTH_REQUIRED", err2_table = true),
    /// 認証資格情報が無効（誤りパスワード等。`28P01`）。
    AuthInvalid => ("28P01", "AUTH_INVALID", err2_table = true),
    /// テナント帰属不一致（`42501`）。
    ForbiddenTenantMismatch => ("42501", "FORBIDDEN_TENANT_MISMATCH", err2_table = true),
    /// FROM に指定したテーブルがカタログ未存在（`42P01`）。
    TableNotFound => ("42P01", "TABLE_NOT_FOUND", err2_table = true),
    /// 指定した行が存在しない（`P0002`）。
    RowNotFound => ("P0002", "ROW_NOT_FOUND", err2_table = true),
    /// `operation_id` 重複（`23505`）。
    DuplicateOperationId => ("23505", "DUPLICATE_OPERATION_ID", err2_table = true),
    /// `operation_id` 省略（`23502`）。
    MissingOperationId => ("23502", "MISSING_OPERATION_ID", err2_table = true),
    /// untrusted 入力のサイズがアロケーション前の上限を超過（`54000`）。
    PayloadTooLarge => ("54000", "PAYLOAD_TOO_LARGE", err2_table = true),
    /// 接続数上限超過（`53300`）。
    ConnectionLimitExceeded => ("53300", "CONNECTION_LIMIT_EXCEEDED", err2_table = true),
    /// 拡張クエリプロトコル未対応（`0A000`）。
    FeatureNotSupported => ("0A000", "FEATURE_NOT_SUPPORTED", err2_table = true),
    /// 受理範囲外の SQL 構文（構文解析失敗・AST 許可リスト外。`42601`）。
    UnsupportedSqlSyntax => ("42601", "UNSUPPORTED_SQL_SYNTAX", err2_table = true),
    /// 起動メッセージ不正（`08P01`）。
    StartupMessageInvalid => ("08P01", "STARTUP_MESSAGE_INVALID", err2_table = true),
    /// commit 済み `operation_id` が内容の異なる書き込みへ再利用された（`22023`）。
    /// ERR-3 管轄。写像のみ本 enum で定義し、発生させる経路は TASK-154 が実装する。
    OperationIdContentMismatch => ("22023", "OPERATION_ID_CONTENT_MISMATCH", err2_table = true),
    /// 予期しない内部エラー（`XX000`）。クライアントへは詳細を運ばない
    /// （[`WireError::internal`] 参照）。
    InternalError => ("XX000", "INTERNAL_ERROR", err2_table = true),
    /// 集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`）の数値演算が表現範囲を超過
    /// （`22003`）。ERR-2 の分類表には未掲載で、SQL-13（`sql::aggregate`）が
    /// ERR-2 の拡張規則（表外の発生条件はビヘイビアファイルが一意の `wire_code` を
    /// 定義してよい）に基づき定義した分類。
    NumericOutOfRange => ("22003", "NUMERIC_OUT_OF_RANGE", err2_table = false),
}

impl ErrorClass {
    /// `wire_code` からの逆引き。未知のコードは `None`（fail-closed。呼び出し元が
    /// 未知コードを既定分類へ丸めて誤った意味論を持たせることを防ぐ）。
    pub fn from_wire_code(code: &str) -> Option<ErrorClass> {
        ErrorClass::ALL.into_iter().find(|c| c.wire_code() == code)
    }
}

/// エラーメッセージへ含める文言の長さ上限。untrusted 断片（テーブル名・SQL 片等）を
/// そのまま無加工で長大に埋め込まない（`.claude/rules/security.md`「情報漏えい」対応）。
/// engine 全体の切り詰め上限の単一真実源であり、`sql::allowlist` は構築時点
/// （コンストラクタ）の切り詰めにこの値を参照する（`WireError::new` 側は最終防波堤として
/// 同じ規約を適用する。2 箇所が独立の値を持って乖離することを構造的に防ぐ）。
pub(crate) const MAX_MESSAGE_LEN: usize = 200;

/// 文字境界で安全に切り詰める（マルチバイト文字の途中で切らない）。添字直接アクセス
/// をせず `get()` で明示的に処理する（`.claude/rules/coding-rust.md`「untrusted 入力の
/// 扱い」）。
fn truncate_message(s: &str) -> String {
    if s.len() <= MAX_MESSAGE_LEN {
        return s.to_string();
    }
    let mut end = MAX_MESSAGE_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    match s.get(..end) {
        Some(prefix) => format!("{prefix}..."),
        None => "...".to_string(),
    }
}

/// engine の各エラー型が共通分類へ写像するための trait。`SqlSurfaceError`
/// （`sql::allowlist`）・`TenantWriteError`（`tenant`）が実装し、既存の
/// `wire_code()`／`client_message()` をこの trait 経由へ委譲する。
pub trait ClassifiedError {
    /// この値が属する [`ErrorClass`]。
    fn error_class(&self) -> ErrorClass;

    /// クライアント（wire 層 `ErrorResponse`）へそのまま返してよい文言。内部詳細・
    /// 他テナントのデータ・存在情報を含めない契約（`.claude/rules/security.md` P0）。
    fn client_message(&self) -> String;

    /// SQLSTATE 風 `wire_code`。既定実装は `error_class().wire_code()` に委譲する
    /// （実装型ごとに再定義しない。乖離を構造的に防ぐ）。
    fn wire_code(&self) -> &'static str {
        self.error_class().wire_code()
    }
}

/// wire 層（TASK-97・TASK-153）へ渡す最終形。`ClassifiedError` を実装する engine の
/// 各エラー型から `From`／`from_classified` で変換して得る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    class: ErrorClass,
    message: String,
}

impl WireError {
    /// 新規構築。`message` は [`MAX_MESSAGE_LEN`] で切り詰める（DoS・情報漏えい対応）。
    /// `class == InternalError` の詳細文言を運びたい場合はこの API を使わず、必ず
    /// [`WireError::internal`] を使うこと（内部ストレージ詳細等の漏えい経路を型で塞ぐ）。
    /// `InternalError` を渡された場合、渡された `message` は使わず
    /// [`WireError::internal`] へ差し替える（コメントの主張を実装でも強制し、
    /// 呼び出し側の実装漏れによる詳細漏えいを構造的に防ぐ）。
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        if matches!(class, ErrorClass::InternalError) {
            return WireError::internal();
        }
        WireError {
            class,
            message: truncate_message(&message.into()),
        }
    }

    /// 内部エラー用の固定文言。呼び出し元は詳細を渡せない（redb I/O エラー等の
    /// 内部ストレージ詳細をクライアントへ運ばないための構造的な防止策。
    /// `.claude/rules/security.md`「不安全な設計」対応）。
    pub fn internal() -> Self {
        WireError {
            class: ErrorClass::InternalError,
            message: "internal error".to_string(),
        }
    }

    /// この値が属する [`ErrorClass`]。
    pub fn class(&self) -> ErrorClass {
        self.class
    }

    /// SQLSTATE 風 `wire_code`。
    pub fn wire_code(&self) -> &'static str {
        self.class.wire_code()
    }

    /// クライアントへ返す文言。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WireError {}

impl<E: ClassifiedError> From<&E> for WireError {
    fn from(e: &E) -> Self {
        if matches!(e.error_class(), ErrorClass::InternalError) {
            return WireError::internal();
        }
        WireError::new(e.error_class(), e.client_message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // `ALL` が enum の全 variant を漏れなく列挙していることの最小限の確認
    // （網羅性の主な検証は `tests/error_format.rs` の結合テスト側で行う）。
    #[test]
    fn all_classes_are_distinct() {
        let set: HashSet<ErrorClass> = ErrorClass::ALL.into_iter().collect();
        assert_eq!(set.len(), ErrorClass::ALL.len());
    }

    #[test]
    fn wire_codes_are_pairwise_distinct() {
        let codes: HashSet<&str> = ErrorClass::ALL.iter().map(|c| c.wire_code()).collect();
        assert_eq!(codes.len(), ErrorClass::ALL.len());
    }
}
