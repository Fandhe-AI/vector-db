//! `self_hnsw` 構成（Issue #658）向けの探査用テーブル投入ツール（手動・ベンチ専用。
//! CI 非配線）。
//!
//! crossdb ベンチの `scripts/crossdb_bench/self_db.py` が hnsw 構成の非 vacuous 確認
//! （`EXPLAIN SELECT ... USING PLAN(...)` の `engine:`/`hnsw_params:`/`ann_plan:` 行
//! 検証）を行うために、`EXPLAIN` の前段 `core.rs::dictionary_required_columns` が
//! 要求する非 nullable TEXT の `path`/`body` 列を持つ最小テーブル `plan_probe` を
//! self_db.py の作業コピー redb へ投入する。crossdb fixture の `docs` テーブルには
//! `path` 列が無いため `docs` に対する `EXPLAIN` は使えず、本ツールが別テーブルとして
//! 用意する（production コード無変更・テスト・ベンチ資産）。
//!
//! `crossdb_plan_probe <db> <dim>` で起動する。`<db>` は self_db.py が用意する作業
//! コピー redb（元 fixture ではない）へのパス。`<dim>` はクエリ fixture（queries200.jsonl
//! 等）の embedding 次元に合わせる（self_db.py の `rng_dim`。列型 `VECTOR(dim)`
//! 不一致による投入拒否を避けるため）。
//!
//! 冪等: `plan_probe` テーブルが既存でスキーマが一致すれば再利用し、固定
//! `operation_id`（`plan-probe-<i>`）の重複投入は `DuplicateOperationId` として
//! スキップする（self_db.py が起動のたびに投入を呼んでも安全）。スキーマ不一致・
//! 内容不一致は fail-closed に非 0 終了する（`seed_docs.rs::create_or_verify_table`
//! と同方針）。
//!
//! 引数はローカル運用者（ベンチハーネス）からの入力であり wire 経由の untrusted
//! 入力ではないため、解析失敗は `expect` で即終了させる（`seed_docs.rs` と同方針）。

use engine::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use engine::tenant::TenantWriteError;

/// probe 用に投入する固定行数。`EXPLAIN` は検索本体を実行しない契約
/// （`sql/explain.rs`）のため、行内容そのものに意味は無く
/// `dictionary_required_columns` の要求列（`path`/`body`）を満たすことだけが目的。
const PROBE_ROWS: usize = 3;

fn usage() -> ! {
    eprintln!("usage: crossdb_plan_probe <db> <dim>");
    std::process::exit(2);
}

/// テーブルを作成する。既存の場合はカタログのスキーマが期待値と完全一致するときだけ
/// 再利用し、不一致なら fail-closed に終了する（`seed_docs.rs::create_or_verify_table`
/// と同方針。`insert_typed_row` は embedding 次元しか検証しないため、スカラー列の
/// 型・順序の不一致をここで塞ぐ）。
fn create_or_verify_table(storage: &Storage, schema: &TableSchema) -> bool {
    match storage.create_table(schema) {
        Ok(()) => false,
        Err(CatalogError::TableAlreadyExists(_)) => {
            let existing = storage
                .get_table_schema(&schema.name)
                .expect("get_table_schema");
            if &existing != schema {
                eprintln!(
                    "error: table {} already exists with a different schema; \
                     use a new working copy db or the same <dim>",
                    schema.name
                );
                std::process::exit(2);
            }
            true
        }
        Err(e) => panic!("create table {}: {e}", schema.name),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(db), Some(dim)) = (args.get(1), args.get(2)) else {
        usage()
    };
    let dim: u32 = dim.parse().expect("dim");
    if dim == 0 {
        eprintln!("error: dim must be >= 1");
        std::process::exit(2);
    }

    let storage = Storage::open(db).expect("open");
    let schema = TableSchema::new(
        "plan_probe",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    if create_or_verify_table(&storage, &schema) {
        eprintln!("resuming: table plan_probe exists; already-recorded rows are skipped");
    }

    // probe テーブルは self_db.py が起動する wire-server 側の既定認証（tenant-a・
    // Public のみ可視）から `EXPLAIN` 実行時に見えれば足りるため、他 fixture
    // （`docs`）の投入方針と揃え tenant-a・Public で固定する。
    let ctx = PolicyContext::new("tenant-a").expect("ctx");
    for i in 0..PROBE_ROWS {
        let mut embedding = vec![0.0f32; dim as usize];
        // 全ゼロベクトルは正規化・非有限値検査に触れかねないため、決定的だが
        // 非ゼロな値にする（`EXPLAIN` は検索本体を実行しないので値そのものに
        // 意味は無い）。
        embedding[i % dim as usize] = 1.0;
        let op = OperationId::parse(&format!("plan-probe-{i}")).expect("valid operation_id");
        let values = [
            Value::Vector(embedding),
            Value::Text(format!("plan_probe/doc-{i}.md")),
            Value::Text(format!("plan probe content {i}")),
        ];
        match engine::tenant::insert_typed_row(
            &storage,
            "plan_probe",
            &ctx,
            i as u64 + 1,
            Visibility::Public,
            &values,
            &op,
        ) {
            Ok(()) => {}
            Err(TenantWriteError::DuplicateOperationId) => {
                eprintln!("skip: row {i} already recorded");
            }
            Err(TenantWriteError::OperationIdContentMismatch) => {
                eprintln!(
                    "error: row {i} was recorded with different content; \
                     use a new working copy db or the same <dim>"
                );
                std::process::exit(2);
            }
            Err(e) => panic!("insert_typed_row: {e}"),
        }
    }
    eprintln!("done: plan_probe table ready ({PROBE_ROWS} rows, dim {dim})");
}
