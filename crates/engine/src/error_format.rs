//! エラー分類共通形式（TASK-152。対象ビヘイビア: ERR-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-152・`docs/spec/04-behavior/error-format.md`）。
//!
//! 責務境界: engine 各所（`sql::allowlist::SqlSurfaceError`・`tenant::TenantWriteError`
//! 等）が独立に持つ `wire_code()` 実装を、本モジュールの [`ErrorClass`] へ委譲させる
//! ための単一真実源（SSOT）を提供する。`wire_code` 写像が複数箇所へ文字列リテラルで
//! 分散し、決定的分類（同一入力に常に同一 `wire_code`）・一意対応（分類⇔`wire_code`）が
//! 構造的に保証されない状態を防ぐことが目的。
//!
//! 収録範囲は「engine・wire-server が現に返している `wire_code`」に限る。未実装の分類は
//! 実装タスク（wire 応答への写像は TASK-153）が追加する。分類の追加自体は TASK-101
//! （`operation_id` 内容照合。RECOVER-10）で行った。「他分類（特に `23505`）へ写像しない」
//! ことの正式検証は TASK-154（対象ビヘイビア ERR-3）が担い、`tests/error_format_err3.rs`
//! の結合テストで検証済み。分類の定義そのものは spec 側の管理事項であり、本コメント・
//! 本モジュールへ転記しない（`.claude/rules/spec-confidentiality.md`）。
//!
//! 分類リストは [`define_error_classes`] マクロの 1 箇所のみで宣言し、`ErrorClass` の
//! 定義・`ALL`・`wire_code`・`label` をそこから生成する。分類を追加・削除すると `ALL` の
//! 固定長（`count`）が合わなくなりコンパイルが失敗するため、「分類は増えたが `ALL` の
//! 更新を忘れる」乖離は構造的に発生しない。
//!
//! wire-server 側（`SQLSTATE_*` 定数・`ErrorResponse` 整形）は TASK-153 の管轄で、
//! 本モジュールはそれらを直接呼び出さない（workspace 責務境界: `.claude/rules/
//! coding-rust.md`）。

/// [`ErrorClass`] の宣言と写像（`ALL`・`wire_code`・`label`）を**単一の分類リスト**から
/// 生成するマクロ。分類・`wire_code`・ラベルを 1 箇所に集約し、リスト間の乖離（分類を
/// 追加したのに `ALL` へ足し忘れる等）を構造的に起こせなくする（ERR-2 の一意対応・
/// 決定的分類を型で担保するのが本モジュールの責務）。`count` は `ALL` の固定長配列長であり、分類を増減させたのに更新しなければ
/// 配列長不一致でコンパイルが失敗する。
macro_rules! define_error_classes {
    (
        count = $count:literal;
        $(
            $(#[$variant_doc:meta])*
            $variant:ident => ($wire_code:literal, $label:literal),
        )+
    ) => {
        /// エラー分類の共通表現。engine・wire-server が現に返す `wire_code` に 1 対 1 で
        /// 対応する（ERR-2。ポインタ: `docs/spec/04-behavior/error-format.md`）。
        ///
        /// `#[non_exhaustive]` は付けない。分類の追加は
        /// [`define_error_classes`] のリストへの 1 行追加としてのみ行い、
        /// `wire_code`／`label`／`ALL` は同リストから生成されるため
        /// 更新漏れが起こり得ない（`StorageError` と同じ「網羅 `match` を強制する」既定方針）。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorClass {
            $( $(#[$variant_doc])* $variant, )+
        }

        impl ErrorClass {
            /// 全分類。テストでの網羅・一意性検証、
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

        }
    };
}

define_error_classes! {
    count = 15;

    /// 構文上受理された SQL の値・引数が不正（`22000`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::InvalidInput`] の写像。
    InvalidInput => ("22000", "INVALID_INPUT"),
    /// 認証資格情報が無効（`28P01`）。wire-server の `auth::SQLSTATE_INVALID_PASSWORD`
    /// に対応する分類（engine 側に発生経路はなく、写像の集約のみ）。
    AuthInvalid => ("28P01", "AUTH_INVALID"),
    /// テナント帰属不一致（`42501`）。[`crate::tenant::TenantWriteError::Forbidden`]
    /// の写像。
    ForbiddenTenantMismatch => ("42501", "FORBIDDEN_TENANT_MISMATCH"),
    /// 参照したテーブルがカタログ未存在（`42P01`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::UndefinedTable`] の写像。
    TableNotFound => ("42P01", "TABLE_NOT_FOUND"),
    /// 指定した行が存在しない（`P0002`）。[`crate::tenant::TenantWriteError::NotFound`]
    /// の写像。
    RowNotFound => ("P0002", "ROW_NOT_FOUND"),
    /// 一意制約の衝突（`23505`）。行キー `(tenant_id, id)` の衝突
    /// （[`crate::tenant::TenantWriteError::IdConflict`]・
    /// [`crate::sql::allowlist::SqlSurfaceError::IdConflict`]）と、`operation_id` の
    /// 重複（TASK-93 の台帳）が共通で属する分類。原因を限定した命名にすると
    /// 別原因の衝突を誤った意味論で運ぶため、`23505` の意味そのもので命名する。
    UniqueViolation => ("23505", "UNIQUE_VIOLATION"),
    /// `USING OPERATION_ID` 句の省略（`23502`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::MissingOperationId`]・
    /// [`crate::tenant::TenantWriteError::MissingOperationId`] の写像。
    MissingOperationId => ("23502", "MISSING_OPERATION_ID"),
    /// untrusted 入力のサイズがアロケーション前の上限を超過（`54000`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::PayloadTooLarge`]・wire-server の
    /// `framing::SQLSTATE_PROGRAM_LIMIT_EXCEEDED` に対応する。
    PayloadTooLarge => ("54000", "PAYLOAD_TOO_LARGE"),
    /// 接続数上限超過（`53300`）。wire-server の `limits::SQLSTATE_TOO_MANY_CONNECTIONS`
    /// に対応する分類（engine 側に発生経路はなく、写像の集約のみ）。
    ConnectionLimitExceeded => ("53300", "CONNECTION_LIMIT_EXCEEDED"),
    /// 未対応のプロトコル機能（`0A000`）。wire-server の
    /// `handshake::SQLSTATE_FEATURE_NOT_SUPPORTED`・`protocol_dispatch` に対応する分類
    /// （engine 側に発生経路はなく、写像の集約のみ）。
    FeatureNotSupported => ("0A000", "FEATURE_NOT_SUPPORTED"),
    /// 受理範囲外の SQL 構文（構文解析失敗・AST 許可リスト外。`42601`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::UnsupportedSyntax`] の写像。
    UnsupportedSqlSyntax => ("42601", "UNSUPPORTED_SQL_SYNTAX"),
    /// プロトコル違反（`08P01`）。wire-server の `framing::SQLSTATE_PROTOCOL_VIOLATION`
    /// に対応する分類（engine 側に発生経路はなく、写像の集約のみ）。
    ProtocolViolation => ("08P01", "PROTOCOL_VIOLATION"),
    /// 予期しない内部エラー（`XX000`）。クライアントへは詳細を運ばない
    /// （[`WireError::internal`] 参照）。
    /// [`crate::sql::allowlist::SqlSurfaceError::Internal`]・
    /// [`crate::tenant::TenantWriteError::Catalog`]／`Storage` の写像。
    InternalError => ("XX000", "INTERNAL_ERROR"),
    /// 数値演算が表現範囲を超過（`22003`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::NumericOutOfRange`]（SQL-13 の集計関数）
    /// の写像。
    NumericOutOfRange => ("22003", "NUMERIC_OUT_OF_RANGE"),
    /// 台帳（TASK-93）に記録済みの `operation_id` へ、内容が異なる書き込みが再送された
    /// （`22023`）。TASK-101（RECOVER-10）が追加。ハッシュ一致の証明が取れない場合は
    /// 常にこちら側へ倒す（fail-closed。commit 済み確定の根拠にしない）。他のいかなる
    /// 分類（特に `23505`）にも写像しないことは対象ビヘイビア ERR-3（TASK-154）が
    /// 確定契約とし、`tests/error_format_err3.rs` で検証する。
    /// [`crate::tenant::TenantWriteError::OperationIdContentMismatch`]・
    /// [`crate::sql::allowlist::SqlSurfaceError::OperationIdContentMismatch`] の写像。
    OperationIdContentMismatch => ("22023", "OPERATION_ID_CONTENT_MISMATCH"),
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

/// 切り詰めたことを示す省略記号。長さは [`MAX_MESSAGE_LEN`] の内数として確保する。
const ELLIPSIS: &str = "...";

/// 文字境界で安全に切り詰める（マルチバイト文字の途中で切らない）。添字直接アクセス
/// をせず `get()` で明示的に処理する（`.claude/rules/coding-rust.md`「untrusted 入力の
/// 扱い」）。省略記号を含めた返値全体が [`MAX_MESSAGE_LEN`] バイトを超えないよう、
/// prefix の上限から省略記号分を差し引く（上限が「メッセージ全体の長さ」を意味する
/// 契約を実装側でも守る。codex-review P1 指摘対応）。
fn truncate_message(s: &str) -> String {
    if s.len() <= MAX_MESSAGE_LEN {
        return s.to_string();
    }
    let mut end = MAX_MESSAGE_LEN.saturating_sub(ELLIPSIS.len());
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    match s.get(..end) {
        Some(prefix) => format!("{prefix}{ELLIPSIS}"),
        None => ELLIPSIS.to_string(),
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
