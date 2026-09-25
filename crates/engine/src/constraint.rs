//! `PRIMARY KEY` 宣言（TABLE-16・TASK-204、Issue #903）のテナント内一意性制約を
//! 検査する単一の検査点。
//!
//! 呼び出し元は `tenant.rs` の各書き込み関数（`insert_*_unchecked`・
//! `upsert_typed_rows_unchecked`・`update_row_columns_unchecked`・
//! `update_rows_where_unchecked`・`replace_typed_rows_by_text_key`）で、
//! いずれも「`operation_id` 台帳への記録 → 行の書き込み」の**後**・テーブル
//! 世代 bump・commit の**前**に同一 write トランザクション内から呼ぶ契約とする
//! （RECOVER-12・TABLE-16。台帳照合を先に行うことで、`operation_id` の再送判定
//! （`23505`／`22023`）が本検査より優先されることを保証する）。`catalog.rs` の
//! 生書き込み API（`#[cfg(test)]` 限定・production では到達不能）はこの検査点を
//! 経由しない（既知のギャップ。`docs/design/sql-primary-key.md` 参照）。
//!
//! 主キーを宣言しないテーブル（大多数）は `schema.primary_key()` が `None` を
//! 返し、呼び出しは即座に成功する（コストゼロ）。
//!
//! # 実装方式（既知の制約）
//!
//! 永続一意索引は導入せず、書き込み対象行を除いたテナント全行
//! （`(tenant_id, 0)..=(tenant_id, u64::MAX)` の物理キー範囲。可視性・
//! テーブル世代キャッシュのいずれも経由しない生の redb 走査）を線形走査して
//! 主キー値の衝突を判定する。計算量はテナントの保有行数に比例するため、
//! 主キーを宣言したテーブルへの書き込みは行数の多いテナントほど遅くなる
//! （`docs/design/sql-primary-key.md` 参照。永続索引化は将来の別課題）。
//! `tenant::enumerate_dml_candidates` が持つ [`crate::tenant`] 内部の総走査上限
//! （`MAX_SCANNED_ROWS`）は意図的に継承しない——継承すると、その上限を超える
//! 行数を既に保有するテナントが主キー宣言テーブルへ一切書き込めなくなる
//! fail-closed 過ぎる制約になってしまうため。
//!
//! # テナント境界（RLS-9・RLS-10 (c)）
//!
//! 走査は常にサーバー側導出テナント（`ctx.tenant_id()` 由来の物理キー範囲）に
//! 閉じ、他テナントの行キー・値には一切触れない。判定母集合はテナントが所有する
//! **全行**（`Public`／`Private` を問わない。可視性フィルタで縮めない）とし、
//! 二次索引（`ScalarIndex`）・世代整合キャッシュのいずれも流用しない生の走査で
//! 判定する——キャッシュは「クエリ時点で可視だった行」を前提に構築されており、
//! 一意性制約はテナントが所有する不可視行との衝突も防がなければならないため
//! （TABLE-16・RLS-10 (c)）。

use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec::ScalarRef;
use crate::tenant::TenantWriteError;
use redb::ReadableTable;
use std::collections::{HashMap, HashSet};

/// 書き込み後・commit 前に呼ぶ唯一の検査点。`schema.primary_key()` が `None`
/// （主キー未宣言テーブル）なら即座に成功する。
///
/// `written_ids` は今回の書き込みトランザクションで `user_rows/{table}` へ
/// 書き込んだ（または上書きした）行の `id` 集合。呼び出し元がこの txn の中で
/// 既に書き込み済みであることが前提で、本関数はそれらを同一 txn 内で読み戻す
/// （redb の write トランザクションは自身が書いた値を同一 txn 内で読める）。
///
/// 判定手順:
/// 1. `written_ids` 各行の主キー値から正準バイト列を計算し、`written_ids` 同士の
///    重複（同一文内で 2 行が同じ主キー値を持つ）を検出する。
/// 2. 対象テナントの全行（`written_ids` を除く）を走査し、1. のいずれかと
///    一致する主キー値を持つ行がないか調べる（自己更新の除外は `written_ids`
///    からの除外そのもので実現される。新旧値の比較は行わない）。
/// 3. いずれかで衝突が見つかった時点で [`TenantWriteError::UniqueViolation`]
///    を返す（最初の 1 件で打ち切り。副作用は呼び出し元が `write_txn` を
///    commit しないことで防ぐ）。
pub(crate) fn enforce_primary_key_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
) -> Result<(), TenantWriteError> {
    let Some(pk_cols) = schema.primary_key() else {
        return Ok(());
    };
    if written_ids.is_empty() {
        return Ok(());
    }

    // 主キー列の論理インデックス（`schema.columns` に対する添字）を宣言順で
    // 解決する。`validate_schema`（`create_table`／カタログ decode の両方が
    // 通す）がこの解決が必ず成功することを保証する不変条件だが、内部矛盾が
    // あっても panic せず fail-closed に拒否する。
    let pk_indices: Vec<usize> = pk_cols
        .iter()
        .map(|name| {
            schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| {
                    TenantWriteError::Catalog(crate::catalog::CatalogError::Invalid(
                        "primary key column not found in live schema columns".to_string(),
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    let mut mask = vec![false; schema.columns.len()];
    for &idx in &pk_indices {
        if let Some(slot) = mask.get_mut(idx) {
            *slot = true;
        }
    }

    let row_table_name = crate::catalog::user_rows_table_name(table_name);
    let row_table = write_txn
        .open_table(crate::catalog::user_rows_table_def(&row_table_name))
        .map_err(crate::catalog::map_row_table_error)?;

    // 1. 今回書き込んだ行同士の主キー値衝突を検出する。
    let written_id_set: HashSet<u64> = written_ids.iter().copied().collect();
    let mut written_keys: HashMap<Vec<u8>, u64> = HashMap::with_capacity(written_id_set.len());
    for &id in &written_id_set {
        let Some(guard) = row_table
            .get((tenant_id, id))
            .map_err(crate::catalog::CatalogError::from)?
        else {
            // 呼び出し元は必ず同一 txn 内で先に書き込み済みのはずだが、内部
            // 不変条件の欠落があっても黙って読み飛ばさず、判定対象から
            // 除外するに留める（この行が存在しない以上、一意性判定の対象には
            // なり得ない）。
            continue;
        };
        let buf = guard.value();
        let key = primary_key_bytes(schema, &mask, buf)?;
        if let Some(existing_id) = written_keys.insert(key, id) {
            if existing_id != id {
                return Err(TenantWriteError::UniqueViolation);
            }
        }
    }
    if written_keys.is_empty() {
        return Ok(());
    }

    // 2. 対象テナントの残り全行（今回書き込んだ id を除く）を走査する。
    // 物理キーは `(tenant_id, id)` の辞書順であり、`(tenant, 0)..=(tenant,
    // u64::MAX)` の閉区間が対象テナントの物理キー空間の全域を過不足なく覆う
    // （`tenant::enumerate_dml_candidates` と同じ範囲構築。RLS-9・TABLE-12）。
    let range_start = std::ops::Bound::Included((tenant_id, 0u64));
    let range_end = std::ops::Bound::Included((tenant_id, u64::MAX));
    for entry in row_table
        .range::<(&str, u64)>((range_start, range_end))
        .map_err(crate::catalog::CatalogError::from)?
    {
        let (k, v) = entry.map_err(crate::catalog::CatalogError::from)?;
        let (key_tenant, id) = k.value();
        if key_tenant != tenant_id {
            // 閉区間により理論上到達しないが、defense-in-depth として維持する
            // （`enumerate_dml_candidates` と同じ判断）。
            break;
        }
        if written_id_set.contains(&id) {
            // 自己更新・自己挿入の除外は id 一致そのもので行う（新旧値の
            // 比較はしない。§モジュールドキュメント参照）。
            continue;
        }
        let buf = v.value();
        let key = primary_key_bytes(schema, &mask, buf)?;
        if written_keys.contains_key(&key) {
            return Err(TenantWriteError::UniqueViolation);
        }
    }

    Ok(())
}

/// 1 行分の物理バイト列 `buf` から主キー列（`mask` で示された列のみ）を復元し、
/// 正準バイト列へ変換する。各コンポーネントは `[type_tag: u8][len: u32 BE]
/// [payload]` の形で連結する（`len` を明示することで、可変長コンポーネント
/// （TEXT・BYTEA・ENUM）を並べたときの境界曖昧性——`("ab","c")` と `("a","bc")`
/// が同一バイト列になる事故——を構造的に排除する）。
fn primary_key_bytes(
    schema: &TableSchema,
    mask: &[bool],
    buf: &[u8],
) -> Result<Vec<u8>, TenantWriteError> {
    let (_dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(buf)?;
    let values = crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(mask))?;
    let mut out = Vec::new();
    for (idx, is_pk) in mask.iter().enumerate() {
        if !is_pk {
            continue;
        }
        let value = values.get(idx).and_then(|v| v.as_ref()).ok_or_else(|| {
            // 主キー列は `nullable == false`（`validate_schema` が強制）で
            // あるべきため、NULL・列欠落はここへ到達しないはずの内部矛盾。
            // black-box に無視せず fail-closed に拒否する。
            TenantWriteError::Catalog(crate::catalog::CatalogError::Invalid(
                "primary key column value is missing or NULL".to_string(),
            ))
        })?;
        push_canonical_component(&mut out, *value)?;
    }
    Ok(out)
}

/// [`primary_key_bytes`] が使う 1 コンポーネント分のエンコード。型タグは
/// [`ColumnType::is_primary_key_allowed`] が許可する型と 1 対 1 に対応する
/// （新しい許可型を追加する際はここも同時に拡張する契約）。
fn push_canonical_component(
    out: &mut Vec<u8>,
    value: ScalarRef<'_>,
) -> Result<(), TenantWriteError> {
    fn push_len_prefixed(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
        out.push(tag);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
    }

    match value {
        ScalarRef::Text(s) => {
            push_len_prefixed(out, ColumnType::Text.primary_key_tag(), s.as_bytes())
        }
        ScalarRef::Integer(i) => {
            push_len_prefixed(out, ColumnType::Integer.primary_key_tag(), &i.to_be_bytes())
        }
        ScalarRef::BigInt(i) => {
            push_len_prefixed(out, ColumnType::BigInt.primary_key_tag(), &i.to_be_bytes())
        }
        ScalarRef::Bool(b) => push_len_prefixed(
            out,
            ColumnType::Boolean.primary_key_tag(),
            &[if b { 1u8 } else { 0u8 }],
        ),
        ScalarRef::Date(d) => {
            push_len_prefixed(out, ColumnType::Date.primary_key_tag(), &d.to_be_bytes())
        }
        ScalarRef::Timestamp(t) => push_len_prefixed(
            out,
            ColumnType::Timestamp.primary_key_tag(),
            &t.to_be_bytes(),
        ),
        ScalarRef::Bytes(b) => push_len_prefixed(out, ColumnType::Bytea.primary_key_tag(), b),
        ScalarRef::Uuid(u) => {
            push_len_prefixed(out, ColumnType::Uuid.primary_key_tag(), u.as_bytes())
        }
        ScalarRef::Enum(s) => push_len_prefixed(
            out,
            // ENUM の型タグは語彙に依存しない固定値（列自体の型が
            // `ColumnType::Enum(Arc<EnumTypeDef>)` を持つが、ここでは変種
            // 判定のためのタグだけが必要なため語彙は参照しない）。
            9,
            s.as_bytes(),
        ),
        ScalarRef::Real(_)
        | ScalarRef::Double(_)
        | ScalarRef::Array(_)
        | ScalarRef::Json(_)
        | ScalarRef::Numeric(_) => {
            // `ColumnType::is_primary_key_allowed` が事前に拒否する型であり、
            // `validate_schema` を通過したスキーマからは到達しないはずの内部
            // 矛盾。値を黙って無視せず fail-closed に拒否する。
            return Err(TenantWriteError::Catalog(
                crate::catalog::CatalogError::Invalid(
                    "primary key column has a type that is not allowed as a primary key"
                        .to_string(),
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, TableSchema};
    use crate::policy::PolicyContext;
    use crate::row_codec::Value;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn tmp_storage(label: &str) -> (Storage, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn ctx(tenant: &'static str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id")
    }

    #[test]
    fn no_primary_key_is_a_no_op() {
        let (storage, _guard) = tmp_storage("constraint-no-pk");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "body",
                crate::catalog::ColumnType::Text,
                true,
            )],
        );
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        enforce_primary_key_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2, 3])
            .expect("no primary key must be a no-op");
        write_txn.abort().expect("abort");
    }

    #[test]
    fn detects_duplicate_within_same_batch() {
        let (storage, _guard) = tmp_storage("constraint-batch-dup");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        let write_txn = storage.begin_write_txn().expect("begin write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            for id in [1u64, 2u64] {
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[Value::Text("same-code".to_string())],
                )
                .expect("encode scalar columns");
                let row = crate::storage::RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[],
                    metadata: &metadata,
                };
                let encoded = crate::storage::encode_row(&row).expect("encode row");
                table
                    .insert(("tenant-a", id), encoded.as_slice())
                    .expect("insert row");
            }
        }
        let err = enforce_primary_key_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2])
            .expect_err("duplicate primary key within the same batch must be rejected");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }

    #[test]
    fn detects_conflict_against_existing_tenant_row() {
        let (storage, _guard) = tmp_storage("constraint-existing-row");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("dup-code".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-1").expect("op id"),
        )
        .expect("first insert must succeed");

        let write_txn = storage.begin_write_txn().expect("begin write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            let metadata = crate::row_codec::encode_scalar_columns(
                &schema,
                &[Value::Text("dup-code".to_string())],
            )
            .expect("encode scalar columns");
            let row = crate::storage::RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[],
                metadata: &metadata,
            };
            let encoded = crate::storage::encode_row(&row).expect("encode row");
            table
                .insert(("tenant-a", 2u64), encoded.as_slice())
                .expect("insert row");
        }
        let err = enforce_primary_key_in_txn(&write_txn, "docs", &schema, "tenant-a", &[2])
            .expect_err("conflict against an existing tenant row must be rejected");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }

    #[test]
    fn different_tenants_may_share_the_same_primary_key_value() {
        let (storage, _guard) = tmp_storage("constraint-cross-tenant");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("shared-code".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-a").expect("op id"),
        )
        .expect("tenant-a insert must succeed");

        // 別テナントは同じ主キー値を持つ行を問題なく挿入できる（RLS-9・TABLE-16）。
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-b"),
            1,
            Visibility::Public,
            &[Value::Text("shared-code".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-b").expect("op id"),
        )
        .expect("tenant-b insert must succeed even with the same primary key value");
    }

    #[test]
    fn self_update_does_not_conflict_with_its_own_previous_value() {
        let (storage, _guard) = tmp_storage("constraint-self-update");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("code-1".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-1").expect("op id"),
        )
        .expect("insert must succeed");

        // 同じ id への UPDATE（値は変えない）は自己衝突しない
        // （id ベースの自己除外。§モジュールドキュメント参照）。
        let write_txn = storage.begin_write_txn().expect("begin write");
        let schema_reloaded =
            crate::catalog::require_table_schema_write(&write_txn, "docs").expect("schema");
        enforce_primary_key_in_txn(&write_txn, "docs", &schema_reloaded, "tenant-a", &[1])
            .expect("self-update with unchanged primary key must not conflict");
        write_txn.abort().expect("abort");
    }
}
