//! RLS 相当のテナント境界・可視性判定を担うモジュール（TASK-124・対象ビヘイビア: CORE-2）。
//!
//! `core.rs` の [`crate::core::VectorCore`] 実装（検索・行取得）は、テナント一致判定と
//! 可視性ラベル評価を必ず [`PolicyContext::is_visible`] の単一照合パスで行う。
//! 呼び出し側が独自にテナント文字列を比較する経路を作らないことで、判定ロジックの
//! 分岐を 1 箇所に集約し fail-closed を維持する（TASK-133 以降の RLS ポリシー評価は
//! このパスを拡張する前提で設計している）。

use std::collections::HashSet;

use crate::storage::Visibility;

/// [`PolicyContext::new`] の構築時検証で発生するエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    /// 空のテナント ID はテナント境界判定を曖昧にするため拒否する（fail-closed。
    /// `storage.rs::RowInput::tenant_id` の既存方針と整合）。
    EmptyTenantId,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::EmptyTenantId => write!(f, "policy context: tenant_id must not be empty"),
        }
    }
}

impl std::error::Error for PolicyError {}

/// 呼び出し元（プロトコル層 → `core.rs`）が渡すアクセス主体のテナント境界・可視性文脈。
///
/// テナント ID と許可可視性ラベル集合を同居させ、判定 API を [`Self::is_visible`] に
/// 集約する（CORE-2: 独立のテナント層を作らない設計）。既定は最も狭い許可
/// （`Public` のみ）で、`Private` を見せるには構築時に明示付与する。
#[derive(Debug, Clone)]
pub struct PolicyContext {
    tenant_id: String,
    allowed_visibilities: HashSet<AllowedVisibility>,
}

/// [`PolicyContext`] 内部でのみ使う可視性集合のキー型。`Visibility` は `Eq`/`Hash` を
/// 持たないため、判定に必要な最小限のラベル種別だけを表す内部型を介する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AllowedVisibility {
    Public,
    Private,
}

impl From<Visibility> for AllowedVisibility {
    fn from(v: Visibility) -> Self {
        match v {
            Visibility::Public => AllowedVisibility::Public,
            Visibility::Private => AllowedVisibility::Private,
        }
    }
}

impl PolicyContext {
    /// `Public` のみ可視の `PolicyContext` を構築する（既定・最小権限）。
    /// 空テナント ID は `Err`（fail-closed）。
    pub fn new(tenant_id: impl Into<String>) -> Result<Self, PolicyError> {
        Self::with_visibilities(tenant_id, [Visibility::Public])
    }

    /// 許可可視性ラベル集合を明示指定して構築する。`Private` を見せる呼び出し元は
    /// ここへ明示的に `Visibility::Private` を含める必要がある（黙示の昇格を許さない）。
    pub fn with_visibilities(
        tenant_id: impl Into<String>,
        visibilities: impl IntoIterator<Item = Visibility>,
    ) -> Result<Self, PolicyError> {
        let tenant_id = tenant_id.into();
        if tenant_id.is_empty() {
            return Err(PolicyError::EmptyTenantId);
        }
        Ok(Self {
            tenant_id,
            allowed_visibilities: visibilities
                .into_iter()
                .map(AllowedVisibility::from)
                .collect(),
        })
    }

    /// このコンテキストが属するテナント ID。
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    /// テナント一致判定と可視性ラベル評価を単一の照合パスで行う（CORE-2）。
    ///
    /// `row_tenant` が自テナントと一致し、かつ `row_visibility` が許可集合に含まれる
    /// 場合にのみ `true`。呼び出し側（検索カーネル・行取得）はこのメソッド以外で
    /// テナント比較を行わない。
    pub fn is_visible(&self, row_tenant: &str, row_visibility: Visibility) -> bool {
        row_tenant == self.tenant_id
            && self
                .allowed_visibilities
                .contains(&AllowedVisibility::from(row_visibility))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 対象ビヘイビア: CORE-2。同一テナント・Public は可視。
    #[test]
    fn same_tenant_public_is_visible() {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(ctx.is_visible("tenant-a", Visibility::Public));
    }

    // 対象ビヘイビア: CORE-2。他テナントは不可視（可視性ラベルに関わらず）。
    #[test]
    fn other_tenant_is_not_visible() {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert!(!ctx.is_visible("tenant-b", Visibility::Public));
        assert!(!ctx.is_visible("tenant-b", Visibility::Private));
    }

    // 対象ビヘイビア: CORE-2。Private は明示付与なしでは不可視（既定は最小権限）。
    #[test]
    fn private_requires_explicit_grant() {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(!ctx.is_visible("tenant-a", Visibility::Private));

        let ctx_with_private = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        assert!(ctx_with_private.is_visible("tenant-a", Visibility::Private));
    }

    // 対象ビヘイビア: CORE-2。空テナント ID は構築時に拒否する（fail-closed）。
    #[test]
    fn empty_tenant_id_is_rejected() {
        assert_eq!(
            PolicyContext::new("").unwrap_err(),
            PolicyError::EmptyTenantId
        );
    }
}
