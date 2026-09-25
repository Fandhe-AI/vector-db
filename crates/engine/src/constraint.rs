//! 行単位の宣言的制約——テナント内一意性制約（`PRIMARY KEY` 宣言〔Issue #903〕・
//! UNIQUE 制約〔Issue #905〕）と `CHECK` 制約〔Issue #906〕。いずれも TABLE-16・
//! TASK-204——を検査する単一の検査点（入口は [`enforce_row_constraints_in_txn`]）。
//!
//! `CHECK` 制約は書き込んだ各行を同一 write トランザクション内で読み戻し、
//! `sql::check_constraint::CompiledChecks`（`WHERE` と同じ束縛・評価器を再利用）で
//! 評価する。一意性制約より**先**に評価する（両方に違反する行は `23514`。
//! PostgreSQL の評価順序に倣う）。`CHECK` を宣言しないテーブルはコンパイル自体を
//! 行わない（コストゼロ）。
//!
//! 呼び出し元は `tenant.rs` の各書き込み関数（`insert_*_unchecked`・
//! `upsert_typed_rows_unchecked`・`update_row_unchecked`・
//! `update_row_columns_unchecked`・`update_rows_where_unchecked`・
//! `replace_typed_rows_by_text_key`）で、いずれも「`operation_id` 台帳への記録 →
//! 行の書き込み」の**後**・テーブル世代 bump・commit の**前**に同一 write
//! トランザクション内から呼ぶ契約とする（RECOVER-12・TABLE-16。台帳照合を先に
//! 行うことで、`operation_id` の再送判定（`23505`／`22023`）が本検査より優先
//! されることを保証する）。明示トランザクション（SQL-31・TASK-221）中の書き込みも
//! `tenant::WriteTarget::InTxn` が共有 write トランザクションを渡すため同じ
//! 検査点を通り、redb の write トランザクションは自身が書いた未 commit の行を
//! 読めるため、同一トランザクション内の先行文が書いた行も母集合に含まれる。
//! `catalog.rs` の生書き込み API（`#[cfg(test)]` 限定・production では到達不能）は
//! この検査点を経由しない（既知のギャップ。`docs/design/sql-primary-key.md` 参照）。
//!
//! 主キーも UNIQUE 制約も宣言しないテーブル（大多数）は検査対象のキーが 0 個に
//! なり、呼び出しは即座に成功する（コストゼロ）。
//!
//! # キーの種類と NULL の扱い
//!
//! - 主キー（`schema.primary_key()`）: 構成列は `nullable == false`
//!   （`validate_schema` が強制）であり、NULL・列欠落は内部矛盾として
//!   fail-closed に拒否する。
//! - UNIQUE 制約（`schema.unique_constraints()`）: NULLS DISTINCT。構成列の
//!   いずれかが NULL の行はその制約の検査対象外とする（NULL 同士は衝突しない）。
//!
//! 等価判定は型タグ＋長さ前置の正準キーバイト列で行い、許可型は主キーと共有する
//! 単一の許可リスト（`ColumnType::is_primary_key_allowed`）に限る。
//!
//! # 実装方式（既知の制約）
//!
//! 永続一意索引は導入せず、書き込み対象行を除いたテナント全行
//! （`(tenant_id, 0)..=(tenant_id, u64::MAX)` の物理キー範囲。可視性・
//! テーブル世代キャッシュのいずれも経由しない生の redb 走査）を 1 文あたり
//! 1 回だけ線形走査し、全キーの衝突をまとめて判定する。計算量はテナントの
//! 保有行数に比例するため、一意キーを宣言したテーブルへの書き込みは行数の
//! 多いテナントほど遅くなる（`docs/design/sql-primary-key.md`・
//! `docs/design/unique-constraint.md` 参照。永続索引化は将来の別課題）。
//! `tenant::enumerate_dml_candidates` が持つ [`crate::tenant`] 内部の総走査上限
//! （`MAX_SCANNED_ROWS`）は意図的に継承しない——継承すると、その上限を超える
//! 行数を既に保有するテナントが一意キー宣言テーブルへ一切書き込めなくなる
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
//! （TABLE-16・RLS-10 (c)）。違反時のエラー（`TenantWriteError::UniqueViolation`）
//! はキー値・列名・行 id・テナント名を含まない固定文言。

use crate::catalog::{CatalogError, ColumnType, TableSchema};
use crate::row_codec::ScalarRef;
use crate::tenant::TenantWriteError;
use redb::ReadableTable;
use std::collections::{HashMap, HashSet};

/// 一意キー 1 個分の NULL の扱い。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NullPolicy {
    /// 主キー: 構成列の NULL は内部矛盾（`nullable == false` が不変条件）。
    Reject,
    /// UNIQUE 制約: NULL を含む行はその制約の検査対象外（NULLS DISTINCT）。
    Skip,
}

/// 一意キー 1 個分の検査仕様（構成列の論理インデックスと NULL の扱い）。
struct KeySpec {
    indices: Vec<usize>,
    null_policy: NullPolicy,
}

/// `schema` が宣言する全一意キー（主キー → UNIQUE 制約の宣言順）の検査仕様と、
/// それらの構成列の和集合マスク（`row_codec::scan_scalar_columns_masked` へ
/// 渡す）を組み立てる。列名解決の失敗は `validate_schema`（`create_table`／
/// カタログ decode の両方が通す）が起こり得ないことを保証する不変条件だが、
/// 内部矛盾があっても panic せず fail-closed に拒否する。
fn key_specs(schema: &TableSchema) -> Result<(Vec<KeySpec>, Vec<bool>), CatalogError> {
    let resolve = |name: &String| -> Result<usize, CatalogError> {
        schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| {
                CatalogError::Invalid(
                    "unique key column not found in live schema columns".to_string(),
                )
            })
    };
    let mut specs: Vec<KeySpec> = Vec::new();
    if let Some(pk_cols) = schema.primary_key() {
        specs.push(KeySpec {
            indices: pk_cols.iter().map(resolve).collect::<Result<_, _>>()?,
            null_policy: NullPolicy::Reject,
        });
    }
    for constraint in schema.unique_constraints() {
        specs.push(KeySpec {
            indices: constraint
                .columns()
                .iter()
                .map(resolve)
                .collect::<Result<_, _>>()?,
            null_policy: NullPolicy::Skip,
        });
    }
    let mut mask = vec![false; schema.columns.len()];
    for spec in &specs {
        for &idx in &spec.indices {
            if let Some(slot) = mask.get_mut(idx) {
                *slot = true;
            }
        }
    }
    Ok((specs, mask))
}

/// `tenant.rs` の各書き込み関数が書き込み後・commit 前に呼ぶ唯一の入口
/// （TABLE-16・TASK-204）。`CHECK` 制約（Issue #906）→ 一意性制約（主キー・
/// UNIQUE）の順に検査する。`written_ids` の契約は [`enforce_unique_keys_in_txn`]
/// と同じ。いずれの制約も宣言しないテーブルは即座に成功する。
pub(crate) fn enforce_row_constraints_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
) -> Result<(), TenantWriteError> {
    enforce_check_constraints_in_txn(write_txn, table_name, schema, tenant_id, written_ids)?;
    enforce_unique_keys_in_txn(write_txn, table_name, schema, tenant_id, written_ids)
}

/// `CHECK` 制約（TABLE-16・TASK-204、Issue #906）の検査。`written_ids` の各行を
/// 同一 write トランザクション内で読み戻し（物理キーはサーバー側導出テナント
/// `tenant_id` で名前空間化済み。RLS-9・TABLE-12）、書き込まれた最終値
/// （UPSERT の `DO UPDATE`・`UPDATE` の SET 適用後の値を含む）に対して全 `CHECK`
/// を評価する。`CHECK` を宣言しないテーブルは即座に成功する。
///
/// 読み戻せない id（同一文内で後から削除された等）は検査対象外とする
/// （存在しない行は制約に違反し得ない。[`enforce_unique_keys_in_txn`] と同じ扱い）。
/// 違反時は [`TenantWriteError::CheckViolation`]（制約名のみ。行の値・id・
/// テナントは含まない）を返し、呼び出し元は `write_txn` を commit しない。
fn enforce_check_constraints_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
) -> Result<(), TenantWriteError> {
    if written_ids.is_empty() {
        return Ok(());
    }
    let Some(compiled) = crate::sql::check_constraint::CompiledChecks::compile(schema)? else {
        return Ok(());
    };
    let row_table_name = crate::catalog::user_rows_table_name(table_name);
    let row_table = write_txn
        .open_table(crate::catalog::user_rows_table_def(&row_table_name))
        .map_err(crate::catalog::map_row_table_error)?;
    let mut embedding: Vec<f32> = Vec::new();
    for &id in written_ids {
        let Some(guard) = row_table
            .get((tenant_id, id))
            .map_err(crate::catalog::CatalogError::from)?
        else {
            continue;
        };
        let buf = guard.value();
        let (_dim, metadata) =
            crate::storage::decode_row_embedding_and_metadata_into(buf, &mut embedding)?;
        compiled.enforce(schema, id, &embedding, metadata)?;
    }
    Ok(())
}

/// 一意性制約（主キー・UNIQUE）の検査点（[`enforce_row_constraints_in_txn`] から
/// 呼ばれる）。主キーも UNIQUE 制約も宣言しないテーブルは即座に成功する。
///
/// `written_ids` は今回の書き込みトランザクションで `user_rows/{table}` へ
/// 書き込んだ（または上書きした）行の `id` 集合。呼び出し元がこの txn の中で
/// 既に書き込み済みであることが前提で、本関数はそれらを同一 txn 内で読み戻す
/// （redb の write トランザクションは自身が書いた値を同一 txn 内で読める）。
///
/// 判定手順（全キーをまとめて扱い、テナント範囲の走査は 1 回だけ行う）:
/// 1. `written_ids` 各行について各キーの正準バイト列を計算し、`written_ids`
///    同士の重複（同一文内で 2 行が同じキー値を持つ）を検出する。
/// 2. 対象テナントの全行（`written_ids` を除く）を走査し、1. のいずれかと
///    一致するキー値を持つ行がないか調べる（自己更新の除外は `written_ids`
///    からの除外そのもので実現される。新旧値の比較は行わない）。
/// 3. いずれかで衝突が見つかった時点で [`TenantWriteError::UniqueViolation`]
///    を返す（最初の 1 件で打ち切り。副作用は呼び出し元が `write_txn` を
///    commit しないことで防ぐ）。
pub(crate) fn enforce_unique_keys_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
) -> Result<(), TenantWriteError> {
    if schema.primary_key().is_none() && schema.unique_constraints().is_empty() {
        return Ok(());
    }
    if written_ids.is_empty() {
        return Ok(());
    }
    let (specs, mask) = key_specs(schema)?;

    let row_table_name = crate::catalog::user_rows_table_name(table_name);
    let row_table = write_txn
        .open_table(crate::catalog::user_rows_table_def(&row_table_name))
        .map_err(crate::catalog::map_row_table_error)?;

    // 1. 今回書き込んだ行同士のキー値衝突を検出する（キーごとに独立した表）。
    let written_id_set: HashSet<u64> = written_ids.iter().copied().collect();
    let mut written_keys: Vec<HashMap<Vec<u8>, u64>> =
        specs.iter().map(|_| HashMap::new()).collect();
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
        let values = decode_key_columns(schema, &mask, buf)?;
        for (spec, keys) in specs.iter().zip(written_keys.iter_mut()) {
            let Some(key) = key_bytes(spec, &values).map_err(internal)? else {
                continue;
            };
            if let Some(existing_id) = keys.insert(key, id) {
                if existing_id != id {
                    return Err(TenantWriteError::UniqueViolation);
                }
            }
        }
    }
    if written_keys.iter().all(HashMap::is_empty) {
        return Ok(());
    }

    // 2. 対象テナントの残り全行（今回書き込んだ id を除く）を 1 回だけ走査する。
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
        let values = decode_key_columns(schema, &mask, buf)?;
        for (spec, keys) in specs.iter().zip(written_keys.iter()) {
            if keys.is_empty() {
                continue;
            }
            let Some(key) = key_bytes(spec, &values).map_err(internal)? else {
                continue;
            };
            if keys.contains_key(&key) {
                return Err(TenantWriteError::UniqueViolation);
            }
        }
    }

    Ok(())
}

/// [`crate::catalog::Storage::alter_table_add_unique_constraint`]（Rust API。
/// TABLE-16・TASK-204、Issue #905）が制約追加前に呼ぶ既存行の重複判定。
/// `row_table`（対象テーブルの行ストア全体）を物理キー順に走査し、テナントごと
/// に独立して `columns`（`schema` の生存列名。`validate_schema` で検証済み）の
/// 値の組の重複を探す。物理キー `(tenant_id, id)`（TABLE-12）の辞書順により
/// 同一テナントの行は常に連続するため、テナントが変わるたびに検査用キー集合を
/// リセットする（テナントを跨いだ同値は重複としない）。NULL を含む行は対象外
/// （NULLS DISTINCT）。キー構築は書き込み時の検査点
/// [`enforce_unique_keys_in_txn`] と同じ正準バイト列を使う。
pub(crate) fn table_has_duplicate_unique_key<T>(
    row_table: &T,
    schema: &TableSchema,
    columns: &[String],
) -> Result<bool, CatalogError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let indices: Vec<usize> = columns
        .iter()
        .map(|name| {
            schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| {
                    CatalogError::Invalid(format!(
                        "unique constraint references unknown column: {name}"
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    let spec = KeySpec {
        indices,
        null_policy: NullPolicy::Skip,
    };
    let mut mask = vec![false; schema.columns.len()];
    for &idx in &spec.indices {
        if let Some(slot) = mask.get_mut(idx) {
            *slot = true;
        }
    }

    let mut current_tenant: Option<String> = None;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for entry in row_table.iter()? {
        let (k, v) = entry?;
        let (key_tenant, _id) = k.value();
        if current_tenant.as_deref() != Some(key_tenant) {
            current_tenant = Some(key_tenant.to_string());
            seen.clear();
        }
        let buf = v.value();
        // 行ヘッダのテナントと物理キーのテナントの整合（TABLE-12）を確認して
        // から値を読む（不整合な行を別テナントの行として数えない）。
        let (row_tenant, _visibility, _offset) = crate::storage::decode_row_header(buf)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        crate::storage::verify_row_key_tenant(key_tenant, row_tenant)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        let (_dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(buf)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        let values = crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&mask))
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        let Some(key) =
            key_bytes(&spec, &values).map_err(|m| CatalogError::Invalid(m.to_string()))?
        else {
            continue;
        };
        if !seen.insert(key) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 内部矛盾（`validate_schema` を通過したスキーマからは到達しないはずの状態）を
/// 表す固定文言を [`TenantWriteError`] へ包む。
fn internal(message: &'static str) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::Invalid(message.to_string()))
}

/// 1 行分の物理バイト列 `buf` から一意キー構成列（`mask` で示された列のみ）を
/// 復元する。
fn decode_key_columns<'a>(
    schema: &TableSchema,
    mask: &[bool],
    buf: &'a [u8],
) -> Result<Vec<Option<ScalarRef<'a>>>, TenantWriteError> {
    let (_dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(buf)?;
    Ok(crate::row_codec::scan_scalar_columns_masked(
        schema,
        metadata,
        Some(mask),
    )?)
}

/// 復元済みの列値 `values`（論理列インデックスで添字付け）から、一意キー
/// `spec` の正準バイト列を組み立てる。各コンポーネントは `[type_tag: u8]
/// [len: u32 BE][payload]` の形で連結する（`len` を明示することで、可変長
/// コンポーネント（TEXT・BYTEA・ENUM）を並べたときの境界曖昧性——`("ab","c")`
/// と `("a","bc")` が同一バイト列になる事故——を構造的に排除する）。
///
/// 構成列のいずれかが NULL（または列欠落）の場合、UNIQUE 制約は `Ok(None)`
/// （検査対象外。NULLS DISTINCT）、主キーは内部矛盾として `Err`。
fn key_bytes(
    spec: &KeySpec,
    values: &[Option<ScalarRef<'_>>],
) -> Result<Option<Vec<u8>>, &'static str> {
    let mut out = Vec::new();
    for &idx in &spec.indices {
        let Some(value) = values.get(idx).and_then(|v| v.as_ref()) else {
            return match spec.null_policy {
                NullPolicy::Skip => Ok(None),
                // 主キー列は `nullable == false`（`validate_schema` が強制）で
                // あるべきため、NULL・列欠落はここへ到達しないはずの内部矛盾。
                // black-box に無視せず fail-closed に拒否する。
                NullPolicy::Reject => Err("primary key column value is missing or NULL"),
            };
        };
        push_canonical_component(&mut out, *value)?;
    }
    Ok(Some(out))
}

/// [`key_bytes`] が使う 1 コンポーネント分のエンコード。型タグは
/// [`ColumnType::is_primary_key_allowed`] が許可する型と 1 対 1 に対応する
/// （新しい許可型を追加する際はここも同時に拡張する契約）。
fn push_canonical_component(out: &mut Vec<u8>, value: ScalarRef<'_>) -> Result<(), &'static str> {
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
            return Err("unique key column has a type that is not allowed as a unique key");
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
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2, 3])
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
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2])
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
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[2])
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
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema_reloaded, "tenant-a", &[1])
            .expect("self-update with unchanged primary key must not conflict");
        write_txn.abort().expect("abort");
    }

    /// 生の行（テナント・id・スカラー値）を write トランザクション内で直接
    /// 書き込むテスト用ヘルパー（検査点を経由しない）。
    fn put_raw_row(
        write_txn: &redb::WriteTransaction,
        schema: &TableSchema,
        tenant: &str,
        id: u64,
        values: &[Value],
    ) {
        let mut table = write_txn
            .open_table(crate::catalog::user_rows_table_def(
                &crate::catalog::user_rows_table_name(&schema.name),
            ))
            .expect("open row table");
        let metadata =
            crate::row_codec::encode_scalar_columns(schema, values).expect("encode scalar columns");
        let row = crate::storage::RowInput {
            tenant_id: tenant,
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &metadata,
        };
        let encoded = crate::storage::encode_row(&row).expect("encode row");
        table
            .insert((tenant, id), encoded.as_slice())
            .expect("insert row");
    }

    fn unique_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("a", crate::catalog::ColumnType::Text, true),
                ColumnDef::new("b", crate::catalog::ColumnType::Text, true),
            ],
        )
        .with_unique_constraints(vec![crate::catalog::UniqueConstraint::new(vec![
            "a".to_string(),
            "b".to_string(),
        ])])
    }

    /// UNIQUE 制約は NULLS DISTINCT: 構成列のいずれかが NULL の行同士は衝突
    /// しない（主キーの NULL 拒否とは異なる扱い。Issue #905）。
    #[test]
    fn unique_constraint_skips_rows_with_null_component() {
        let (storage, _guard) = tmp_storage("constraint-unique-null");
        let schema = unique_schema();
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        for id in [1u64, 2u64] {
            put_raw_row(
                &write_txn,
                &schema,
                "tenant-a",
                id,
                &[Value::Text("same".to_string()), Value::Null],
            );
        }
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2])
            .expect("rows with a NULL component must not conflict");
        write_txn.abort().expect("abort");
    }

    /// 複合 UNIQUE 制約は全構成列が一致した場合のみ衝突し、既存行との衝突も
    /// 同一テナント内に限って検出する。
    #[test]
    fn composite_unique_constraint_detects_full_match_within_tenant_only() {
        let (storage, _guard) = tmp_storage("constraint-unique-composite");
        let schema = unique_schema();
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        let xy = [Value::Text("x".to_string()), Value::Text("y".to_string())];
        put_raw_row(&write_txn, &schema, "tenant-b", 1, &xy);
        put_raw_row(
            &write_txn,
            &schema,
            "tenant-a",
            1,
            &[Value::Text("x".to_string()), Value::Text("z".to_string())],
        );
        put_raw_row(&write_txn, &schema, "tenant-a", 2, &xy);
        // tenant-b の同値・tenant-a の部分一致とは衝突しない。
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[2])
            .expect("partial match and other tenants' values must not conflict");
        put_raw_row(&write_txn, &schema, "tenant-a", 3, &xy);
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[3])
            .expect_err("full match within the tenant must conflict");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }

    /// 制約追加前の既存行重複判定（`table_has_duplicate_unique_key`）は
    /// テナントごとに独立し、NULL を含む行を対象外とする。
    #[test]
    fn table_has_duplicate_unique_key_is_scoped_per_tenant_and_skips_nulls() {
        let (storage, _guard) = tmp_storage("constraint-unique-alter-scan");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("a", crate::catalog::ColumnType::Text, true)],
        );
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        let x = [Value::Text("x".to_string())];
        put_raw_row(&write_txn, &schema, "tenant-a", 1, &x);
        put_raw_row(&write_txn, &schema, "tenant-b", 1, &x);
        put_raw_row(&write_txn, &schema, "tenant-a", 2, &[Value::Null]);
        put_raw_row(&write_txn, &schema, "tenant-a", 3, &[Value::Null]);
        let columns = vec!["a".to_string()];
        {
            let table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            assert!(!table_has_duplicate_unique_key(&table, &schema, &columns)
                .expect("scan must succeed"));
        }
        put_raw_row(&write_txn, &schema, "tenant-b", 2, &x);
        {
            let table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            assert!(table_has_duplicate_unique_key(&table, &schema, &columns)
                .expect("scan must succeed"));
        }
        write_txn.abort().expect("abort");
    }
}
