//! `USING OPERATION_ID '<id>'` 文末句の値型・検証（TASK-80、対象ビヘイビア: SQL-10。
//! ポインタ: docs/spec/05-tasks.md TASK-80・docs/spec/04-behavior/sql-surface.md・
//! docs/spec/04-behavior/recovery.md RECOVER-1）。
//!
//! 責務境界: 書き込み系 SQL 文の文末専用句として `operation_id` を搬送する、
//! 本タスクで唯一の規範経路（セッション変数・コメント埋め込み等の別経路は設けない。
//! スコープは statement 単位に閉じる）。句の構文上のパース（`USING`・
//! `OPERATION_ID` キーワード・文字列リテラルの並び）は
//! `sql::allowlist::Parser::parse_operation_id_clause`（呼び出し元）が行い、本モジュールは
//! パース済み文字列値の意味論的検証（[`OperationId::parse`]）のみを担う。
//!
//! 台帳への永続化・重複拒否（`23505`）・内容不一致（`22023`）は本モジュールの
//! 管轄外（TASK-93・TASK-94・TASK-101 が [`OperationId`] を土台にする）。

use crate::sql::allowlist::SqlSurfaceError;

/// `operation_id` 値のバイト長上限。台帳キーとして持ち回る値の上限を
/// アロケーション前に固定する（`.claude/rules/security.md`「不安全な設計｜
/// 無制限リソース確保」対応）。
pub const MAX_OPERATION_ID_LEN: usize = 256;

/// 検証済みの `operation_id` 値。空文字・長さ超過・制御文字混入を排除済み。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationId(String);

impl OperationId {
    /// `raw` を検証して [`OperationId`] を構築する。
    ///
    /// - 空文字は「句の省略と同等」とみなし [`SqlSurfaceError::MissingOperationId`]
    ///   （`23502`）で拒否する（advisor 方針: `23502` は not-null 制約違反相当のため、
    ///   明示的に空値を渡す行為も「値が実質的に欠落している」として同じ扱いにする）。
    /// - [`MAX_OPERATION_ID_LEN`] バイト超過・制御文字（`char::is_control`）混入は
    ///   [`SqlSurfaceError::invalid_input`]（`22000`）。
    pub fn parse(raw: &str) -> Result<Self, SqlSurfaceError> {
        if raw.is_empty() {
            return Err(SqlSurfaceError::missing_operation_id());
        }
        if raw.len() > MAX_OPERATION_ID_LEN {
            return Err(SqlSurfaceError::invalid_input(format!(
                "USING OPERATION_ID value length {} exceeds limit {MAX_OPERATION_ID_LEN}",
                raw.len()
            )));
        }
        if raw.chars().any(char::is_control) {
            return Err(SqlSurfaceError::invalid_input(
                "USING OPERATION_ID value must not contain control characters",
            ));
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_normal_value() {
        let id = OperationId::parse("op-0001").expect("valid value");
        assert_eq!(id.as_str(), "op-0001");
    }

    #[test]
    fn rejects_empty_value_as_missing() {
        let err = OperationId::parse("").unwrap_err();
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_value_exceeding_max_len() {
        let raw = "x".repeat(MAX_OPERATION_ID_LEN + 1);
        let err = OperationId::parse(&raw).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn accepts_value_at_max_len_boundary() {
        let raw = "x".repeat(MAX_OPERATION_ID_LEN);
        assert!(OperationId::parse(&raw).is_ok());
    }

    #[test]
    fn rejects_value_with_control_character() {
        let err = OperationId::parse("op-000\u{0007}1").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }
}
