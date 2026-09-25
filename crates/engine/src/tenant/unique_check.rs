//! UNIQUE 制約（[`crate::catalog::UniqueConstraint`]。TABLE-16・TASK-204、
//! Issue #905）の書き込み時検査。`tenant.rs` の全書き込みプリミティブ
//! （INSERT／UPSERT／UPDATE 系）が単一の検査点として呼ぶ
//! [`check_in_txn`] を提供する。
//!
//! ## 設計（D1・D2。詳細は `docs/design/unique-constraint.md` 参照）
//!
//! 検査は書き込みトランザクション内で「自テナントの物理キー範囲
//! `(tenant_id, 0)..=(tenant_id, u64::MAX)`」（TABLE-12）を走査し、制約列のみを
//! [`crate::row_codec::scan_scalar_columns`] でデコードして行う。可視性
//! （`PolicyContext` の可視集合）では絞らない——`Public`／`Private` を問わず
//! テナント所有の全行が母集合になる（RLS-9・RLS-10 (c) と同じ「テナント境界は
//! 緩めない」原則で、可視集合という部分集合に基づく判定は不可視行の重複を
//! 見逃す fail-open になるため採用しない）。他テナントの範囲は構造的に一切
//! 読まないため、他テナントの値の存在は判定にも応答にも一切影響しない。
//!
//! メモリは**候補（今回書き込む行）側のキー集合**のみを保持し、既存行は
//! ストリーミングで照合する（O(候補数) メモリ。候補数は呼び出し元の
//! DML 行数上限で有界）。制約を持たないテーブルは呼び出し元が
//! [`crate::catalog::TableSchema::unique_constraints`] が空であることを確認した
//! 時点で本モジュールを呼ばない（走査コストを一切発生させない）。

use redb::ReadableTable;

use crate::catalog::{unique_constraint_key, CatalogError, TableSchema};
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::storage::{
    decode_row_dim_and_metadata_borrowed, decode_row_header, verify_row_key_tenant,
};
use crate::tenant::TenantWriteError;

/// 書き込み対象行 1 件ぶんの借用ビュー（[`check_in_txn`] の入力）。`id` は
/// UPDATE の自己比較（既存の自分自身の値との一致は違反にしない）・
/// バッチ内の `replaced_ids` 判定に使う。`metadata` は
/// [`crate::row_codec::encode_scalar_columns`] が出力するスカラー列ペイロード
/// （行全体のバイト列ではない）で、`row.metadata`（生 `RowInput` 経路）と
/// 完全に同じ形。
pub(crate) struct UniqueCandidate<'a> {
    pub id: u64,
    pub metadata: &'a [u8],
}

/// UNIQUE 制約の検査本体（TABLE-16・TASK-204、Issue #905）。呼び出し元
/// （`tenant.rs` の各 `*_unchecked` 系）は、台帳照合・追記
/// （[`crate::recovery::ledger::record_in_txn`]）の**直後**・行の実書き込みの
/// **直前**に本関数を呼ぶ契約とする（判定順序: 台帳照合 → 一意性検査 → 行
/// 書き込み → 世代 bump → commit。RECOVER-12・TABLE-16）。違反時は
/// `Err` を返すのみで write トランザクションには一切触れない——呼び出し元が
/// `?` で早期 return し `write_txn` を drop することで、台帳エントリを含め
/// 副作用ゼロを保つ（fail-closed）。
///
/// - `schema.unique_constraints()` が空なら即座に `Ok(())`（走査しない。
///   制約を持たないテーブルの既存ベンチ経路への影響をゼロにする）。
/// - `candidates`: 今回のトランザクションで書き込まれる行（挿入・置換後の
///   値）。バッチ内の重複（同一トランザクション内の複数候補が同じキーを
///   持つ）もここで検出する。
/// - `replaced_ids`: 今回の書き込みで**消える**既存行の `id`（UPDATE の対象
///   行自身・UPSERT の `DO UPDATE` 対象行等）。既存行走査でこれらの `id` を
///   スキップすることで、「自分自身の現在値と同じ値へ更新する」操作を
///   違反として誤検出しない。
pub(crate) fn check_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
    schema: &TableSchema,
    ctx: &PolicyContext,
    candidates: &[UniqueCandidate<'_>],
    replaced_ids: &[u64],
) -> Result<(), TenantWriteError> {
    if schema.unique_constraints().is_empty() {
        return Ok(());
    }
    let row_table_name = crate::catalog::user_rows_table_name(table);
    match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name)) {
        Ok(row_table) => check_against_table(&row_table, schema, ctx, candidates, replaced_ids),
        Err(redb::TableError::TableDoesNotExist(_)) => {
            // 初回書き込み前で行ストアが物理的に未作成（既存行 0 件）。
            Ok(())
        }
        Err(e) => Err(TenantWriteError::Catalog(CatalogError::from(e))),
    }
}

/// [`check_in_txn`] の本体。呼び出し元が既に行テーブルを開いている場合
/// （`upsert_typed_rows_unchecked` の read-merge-write ループ。同一 write
/// トランザクション内で同じ動的テーブルを 2 回 `open_table` すると redb が
/// エラーを返すため、既存ハンドルを再利用する必要がある）向けに公開する。
/// `row_table` への書き込み（`insert`）は呼び出し元がこの関数の戻り値
/// （`Ok`）を確認した**後**にのみ行う契約とする（本関数は読み取りのみ）。
pub(crate) fn check_against_table(
    row_table: &redb::Table<'_, (&'static str, u64), &'static [u8]>,
    schema: &TableSchema,
    ctx: &PolicyContext,
    candidates: &[UniqueCandidate<'_>],
    replaced_ids: &[u64],
) -> Result<(), TenantWriteError> {
    let constraints = schema.unique_constraints();
    if constraints.is_empty() {
        return Ok(());
    }

    // 候補側: バッチ内重複の検出（制約ごとに独立したキー空間を持つ。
    // 複数の制約を同じ `HashSet` で扱うと異なる制約由来のキーが衝突し得る
    // ため、制約数ぶんの `HashSet` を用意する）。
    let mut candidate_keys: Vec<std::collections::HashSet<Vec<u8>>> = Vec::new();
    candidate_keys
        .try_reserve_exact(constraints.len())
        .map_err(|_| {
            TenantWriteError::Storage(crate::storage::StorageError::Codec(
                "failed to reserve unique constraint candidate sets".to_string(),
            ))
        })?;
    for _ in constraints {
        candidate_keys.push(std::collections::HashSet::new());
    }

    for candidate in candidates {
        let values = row_codec::scan_scalar_columns(schema, candidate.metadata)
            .map_err(|e| TenantWriteError::Catalog(CatalogError::Invalid(e.to_string())))?;
        for (constraint, keys) in constraints.iter().zip(candidate_keys.iter_mut()) {
            let key = unique_constraint_key(schema, constraint, &values)
                .map_err(TenantWriteError::Catalog)?;
            let Some(key) = key else {
                continue;
            };
            if !keys.insert(key) {
                return Err(unique_violation());
            }
        }
    }

    // 既存行の走査（テナント所有の全行。TABLE-12）。`replaced_ids` と今回の
    // 候補 `id` はいずれも「今回のトランザクションで内容が確定するのは候補側」
    // であるため、既存行としては読み捨てる（自己比較を避ける）。
    let mut skip_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    skip_ids
        .try_reserve(candidates.len().saturating_add(replaced_ids.len()))
        .map_err(|_| {
            TenantWriteError::Storage(crate::storage::StorageError::Codec(
                "failed to reserve unique constraint skip set".to_string(),
            ))
        })?;
    for candidate in candidates {
        skip_ids.insert(candidate.id);
    }
    for id in replaced_ids {
        skip_ids.insert(*id);
    }

    let tenant = ctx.tenant_id();
    let range_start = std::ops::Bound::Included((tenant, 0u64));
    let range_end = std::ops::Bound::Included((tenant, u64::MAX));
    for entry in row_table
        .range::<(&str, u64)>((range_start, range_end))
        .map_err(|e| TenantWriteError::Catalog(CatalogError::from(e)))?
    {
        let (k, v) = entry.map_err(|e| TenantWriteError::Catalog(CatalogError::from(e)))?;
        let (key_tenant, id) = k.value();
        if key_tenant != tenant {
            // 閉区間により理論上到達しないが、defense-in-depth として
            // `enumerate_dml_candidates` と同じ判断を踏襲する。
            break;
        }
        if skip_ids.contains(&id) {
            continue;
        }
        let buf = v.value();
        let (row_tenant, _visibility, _offset) =
            decode_row_header(buf).map_err(TenantWriteError::Storage)?;
        verify_row_key_tenant(key_tenant, row_tenant).map_err(TenantWriteError::Storage)?;
        let (_dim, metadata) =
            decode_row_dim_and_metadata_borrowed(buf).map_err(TenantWriteError::Storage)?;
        let values = row_codec::scan_scalar_columns(schema, metadata)
            .map_err(|e| TenantWriteError::Catalog(CatalogError::Invalid(e.to_string())))?;
        for (constraint, keys) in constraints.iter().zip(candidate_keys.iter()) {
            let key = unique_constraint_key(schema, constraint, &values)
                .map_err(TenantWriteError::Catalog)?;
            let Some(key) = key else {
                continue;
            };
            if keys.contains(&key) {
                return Err(unique_violation());
            }
        }
    }

    Ok(())
}

/// UNIQUE 制約違反を [`TenantWriteError::UniqueViolation`] へ写像する唯一の
/// 生成点（文言・分類を一箇所に集約する）。
fn unique_violation() -> TenantWriteError {
    TenantWriteError::UniqueViolation
}
