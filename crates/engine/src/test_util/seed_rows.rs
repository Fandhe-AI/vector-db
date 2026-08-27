//! 結合テスト専用のシード投入ヘルパー（codex-review P0 指摘・PR #194 の後続整理）。
//!
//! 背景: `Storage::insert_rows_into_table`（テナント境界チェックを持たない生の
//! バッチ書き込み）を `pub(crate)` 化したのに伴い、結合テストのシードは
//! `engine::tenant::insert_rows`（`PolicyContext` 必須のガード付きバッチ API）へ
//! 移行した。ガード付き API は 1 バッチ内のテナント混在を `Forbidden` で拒否するため、
//! 複数テナントのコーパスを投入するテストはテナント単位に分割する必要がある。
//! その分割処理が 4 本の結合テストへ同一のまま複製されていたため、本ファイルへ
//! 一本化する（`temp_db.rs` と同じ整理方針。Issue #173・Bugbot Low 指摘）。
//!
//! 呼び出し文脈: 結合テスト（`tests/*.rs`）から
//! `#[path = "../src/test_util/seed_rows.rs"] mod seed_rows;` で取り込む。この取り込み
//! 方式では `crate::` がテストバイナリ自身を指すため、本ファイルは `crate::` を
//! 一切参照せず、公開 API（`engine::…`）だけに依存する（`temp_db.rs` が `std` のみに
//! 依存するのと同じ制約への対応）。そのため本ファイルは `lib.rs` の
//! `#[cfg(test)] mod test_util` へは登録せず（クレート内から `engine::` は解決できない）、
//! 結合テストからの `#[path]` 取り込み専用とする。
#![allow(dead_code)]

use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

/// プロセス内で `seed_rows_grouped_by_tenant` を呼ぶたびに一意な `operation_id`
/// suffix を払い出すためのカウンタ（TASK-94・RECOVER-3 対応。台帳の重複拒否が入った
/// ため、固定文字列の `operation_id` を同一テナント・同一テーブルへ複数回使うと
/// 2 回目以降が `23505` で失敗しうる。1 テスト 1 回呼び出しの現状では衝突しないが、
/// 将来の二重シードでも壊れないよう呼び出しごとに一意化しておく安全側の対応）。
static SEED_OP_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `rows` をテナントごとのバッチへ分割し、テナント境界付き API
/// （`engine::tenant::insert_rows`）で投入する。
///
/// 可視性は `Public`/`Private` 双方を許可した `PolicyContext` で投入するが、書き込み
/// 認可（`PolicyContext::is_owner`）は可視性ラベルを見ないため結果には影響しない
/// （対象ビヘイビア: RECOVER-4）。投入に失敗した場合はテストを落とす（シード失敗を
/// 黙って握り潰さない）。
pub fn seed_rows_grouped_by_tenant(storage: &Storage, table: &str, rows: &[(u64, RowInput<'_>)]) {
    let mut tenants: Vec<&str> = rows.iter().map(|(_, r)| r.tenant_id).collect();
    tenants.sort_unstable();
    tenants.dedup();
    for tenant in tenants {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let batch: Vec<(u64, RowInput<'_>)> = rows
            .iter()
            .filter(|(_, r)| r.tenant_id == tenant)
            .map(|(id, r)| {
                (
                    *id,
                    RowInput {
                        tenant_id: r.tenant_id,
                        visibility: r.visibility,
                        embedding: r.embedding,
                        metadata: r.metadata,
                    },
                )
            })
            .collect();
        let suffix = SEED_OP_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let op_id = OperationId::parse(&format!("seed-rows-grouped-by-tenant-{suffix}"))
            .expect("valid operation_id");
        engine::tenant::insert_rows(storage, table, &ctx, &batch, &op_id).expect("seed rows");
    }
}
