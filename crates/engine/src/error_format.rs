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
//! wire-server 側（`SQLSTATE_*` 定数・`ErrorResponse` 整形）は TASK-153 の管轄で、
//! 本モジュールはそれらを直接呼び出さない（workspace 責務境界: `.claude/rules/
//! coding-rust.md`）。`OperationIdContentMismatch`（ERR-3 管轄）は写像の一意性を
//! 保つために本 enum へ含めるが、これを実際に発生させる経路（`recovery.md`
//! RECOVER-10 の内容照合）は TASK-154 の管轄であり本モジュールでは実装しない。

/// エラー分類の共通表現。`docs/spec/04-behavior/error-format.md` の分類表
/// （計 15 行・15 分類）に 1 対 1 対応する。表の掲載順と揃える。
///
/// `#[non_exhaustive]` は付けない。網羅 `match` によって、新しい分類を追加した際
/// `wire_code`／`label`／`ALL` のすべてを更新し忘れるとコンパイルが失敗する構造に
/// しておくことを優先する（`StorageError` と同じ既定方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// 構文上受理された SQL の値・引数が不正（`22000`）。
    InvalidInput,
    /// 認証資格情報なし（未認証セッションでのクエリ受信等。`28000`）。
    AuthRequired,
    /// 認証資格情報が無効（誤りパスワード等。`28P01`）。
    AuthInvalid,
    /// テナント帰属不一致（`42501`）。
    ForbiddenTenantMismatch,
    /// FROM に指定したテーブルがカタログ未存在（`42P01`）。
    TableNotFound,
    /// 指定した行が存在しない（`P0002`）。
    RowNotFound,
    /// `operation_id` 重複（`23505`）。
    DuplicateOperationId,
    /// `operation_id` 省略（`23502`）。
    MissingOperationId,
    /// untrusted 入力のサイズがアロケーション前の上限を超過（`54000`）。
    PayloadTooLarge,
    /// 接続数上限超過（`53300`）。
    ConnectionLimitExceeded,
    /// 拡張クエリプロトコル未対応（`0A000`）。
    FeatureNotSupported,
    /// 受理範囲外の SQL 構文（構文解析失敗・AST 許可リスト外。`42601`）。
    UnsupportedSqlSyntax,
    /// 起動メッセージ不正（`08P01`）。
    StartupMessageInvalid,
    /// commit 済み `operation_id` が内容の異なる書き込みへ再利用された（`22023`）。
    /// ERR-3 管轄。写像のみ本 enum で定義し、発生させる経路は TASK-154 が実装する。
    OperationIdContentMismatch,
    /// 予期しない内部エラー（`XX000`）。クライアントへは詳細を運ばない
    /// （[`WireError::internal`] 参照）。
    InternalError,
}

impl ErrorClass {
    /// 全 15 分類。テストでの網羅・一意性検証、`from_wire_code` の逆引きに使う。
    pub const ALL: [ErrorClass; 15] = [
        ErrorClass::InvalidInput,
        ErrorClass::AuthRequired,
        ErrorClass::AuthInvalid,
        ErrorClass::ForbiddenTenantMismatch,
        ErrorClass::TableNotFound,
        ErrorClass::RowNotFound,
        ErrorClass::DuplicateOperationId,
        ErrorClass::MissingOperationId,
        ErrorClass::PayloadTooLarge,
        ErrorClass::ConnectionLimitExceeded,
        ErrorClass::FeatureNotSupported,
        ErrorClass::UnsupportedSqlSyntax,
        ErrorClass::StartupMessageInvalid,
        ErrorClass::OperationIdContentMismatch,
        ErrorClass::InternalError,
    ];

    /// SQLSTATE 風の 5 文字コード。ERR-2 が確定として保証する契約そのもの。
    /// 外部状態・時刻・乱数を参照しない純粋な `match`（決定的分類の保証）。
    pub const fn wire_code(self) -> &'static str {
        match self {
            ErrorClass::InvalidInput => "22000",
            ErrorClass::AuthRequired => "28000",
            ErrorClass::AuthInvalid => "28P01",
            ErrorClass::ForbiddenTenantMismatch => "42501",
            ErrorClass::TableNotFound => "42P01",
            ErrorClass::RowNotFound => "P0002",
            ErrorClass::DuplicateOperationId => "23505",
            ErrorClass::MissingOperationId => "23502",
            ErrorClass::PayloadTooLarge => "54000",
            ErrorClass::ConnectionLimitExceeded => "53300",
            ErrorClass::FeatureNotSupported => "0A000",
            ErrorClass::UnsupportedSqlSyntax => "42601",
            ErrorClass::StartupMessageInvalid => "08P01",
            ErrorClass::OperationIdContentMismatch => "22023",
            ErrorClass::InternalError => "XX000",
        }
    }

    /// 非規範の人間可読ラベル（`SCREAMING_SNAKE_CASE`）。診断・ログ用途に限り、
    /// wire プロトコル応答の契約には含めない（確定契約は `wire_code` のみ）。
    pub const fn label(self) -> &'static str {
        match self {
            ErrorClass::InvalidInput => "INVALID_INPUT",
            ErrorClass::AuthRequired => "AUTH_REQUIRED",
            ErrorClass::AuthInvalid => "AUTH_INVALID",
            ErrorClass::ForbiddenTenantMismatch => "FORBIDDEN_TENANT_MISMATCH",
            ErrorClass::TableNotFound => "TABLE_NOT_FOUND",
            ErrorClass::RowNotFound => "ROW_NOT_FOUND",
            ErrorClass::DuplicateOperationId => "DUPLICATE_OPERATION_ID",
            ErrorClass::MissingOperationId => "MISSING_OPERATION_ID",
            ErrorClass::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            ErrorClass::ConnectionLimitExceeded => "CONNECTION_LIMIT_EXCEEDED",
            ErrorClass::FeatureNotSupported => "FEATURE_NOT_SUPPORTED",
            ErrorClass::UnsupportedSqlSyntax => "UNSUPPORTED_SQL_SYNTAX",
            ErrorClass::StartupMessageInvalid => "STARTUP_MESSAGE_INVALID",
            ErrorClass::OperationIdContentMismatch => "OPERATION_ID_CONTENT_MISMATCH",
            ErrorClass::InternalError => "INTERNAL_ERROR",
        }
    }

    /// `wire_code` からの逆引き。未知のコードは `None`（fail-closed。呼び出し元が
    /// 未知コードを既定分類へ丸めて誤った意味論を持たせることを防ぐ）。
    pub fn from_wire_code(code: &str) -> Option<ErrorClass> {
        ErrorClass::ALL.into_iter().find(|c| c.wire_code() == code)
    }
}

/// エラーメッセージへ含める文言の長さ上限。untrusted 断片（テーブル名・SQL 片等）を
/// そのまま無加工で長大に埋め込まない（`.claude/rules/security.md`「情報漏えい」対応）。
/// `sql::allowlist::MAX_ERROR_DETAIL_LEN` と同値。両モジュールが独立に定数を持つのは、
/// `allowlist.rs` 側の切り詰めが構築時点（コンストラクタ）で完結する既存契約のためで、
/// ここでは `WireError::new` が受け取る文言に対する最終防波堤として同じ規約を適用する。
const MAX_MESSAGE_LEN: usize = 200;

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
    fn all_has_fifteen_distinct_classes() {
        assert_eq!(ErrorClass::ALL.len(), 15);
        let set: HashSet<ErrorClass> = ErrorClass::ALL.into_iter().collect();
        assert_eq!(set.len(), 15);
    }

    #[test]
    fn wire_codes_are_pairwise_distinct() {
        let codes: HashSet<&str> = ErrorClass::ALL.iter().map(|c| c.wire_code()).collect();
        assert_eq!(codes.len(), 15);
    }
}
