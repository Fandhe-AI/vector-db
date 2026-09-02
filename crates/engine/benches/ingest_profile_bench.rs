//! ingest 経路（`engine::tenant::insert_rows` → `insert_rows_unchecked`）の段別
//! 内訳プロファイル（Issue #396）。書き込み経路には既存の専用ベンチが無く
//! （`examples/feature_bench.rs` の `ingest` フェーズが唯一の計測手段で、e2e の
//! 合計レイテンシしか出さない）、`insert_rows_unchecked` の内部段（所有権検査・
//! バッチ内 id 重複検出・`begin_write`・content_hash・台帳記録・encode・redb
//! insert・世代更新・commit）のどれが支配的かの実測が存在しない。本ベンチは
//! Issue #356（hybrid 段別）・#362（KNN 段別）と同型の手動専用・情報提供専用
//! ベンチとして、その内訳を実測する。
//!
//! # 測定設計
//!
//! `insert_rows_unchecked` の内部段はいずれも `pub(crate)` で、独立コンパイル
//! 単位であるベンチから直接計測できない（`harness::ingest_profile` 冒頭コメント
//! 参照）。そのため 2 つの手段を組み合わせる:
//!
//! - **E0**: `engine::tenant::insert_rows`（pub API。`examples/feature_bench.rs`
//!   と同じ入口）を丸ごと計測する e2e 計測。
//! - **I1〜I8**: 生 `redb::Database`（既存 dev-dependency）を用いた計装レプリカ。
//!   `insert_rows_unchecked` の各段を同じテーブル構成（`user_rows/docs`・
//!   `op_ledger`・`last_op`・`table_generation`）・同じ順序で再現し、段ごとに
//!   `Instant` で区切って計測する。
//!
//! | 段 | 内容 |
//! | --- | --- |
//! | I1 | 所有権検査（`PolicyContext::is_owner` 全件）＋ バッチ内 id 重複検出 |
//! | I2 | `begin_write` |
//! | I3 | content_hash（全行の encode 再実装 ＋ SHA-256 再実装） |
//! | I4 | 台帳記録（`op_ledger` get+insert・`last_op` insert） |
//! | I5 | encode（行ループ内。実経路は I3 と I5 で **encode が 2 回**走る） |
//! | I6 | redb insert（`insert` の戻り値が `None` であることを検査） |
//! | I7 | 世代更新（`table_generation` get→checked_add→insert） |
//! | I8 | `commit` |
//! | 残差 | E0 − Σ(I1..I8)。スキーマ取得（カタログデコード）・`commit_boundary`
//! ガード・抽象化コストに相当する |
//!
//! E0・I1〜I8 いずれも、warmup 20 バッチ ＋ 計測 20 バッチ ＝ 40 バッチ
//! （既定 1,000 行/バッチ ＝ 40,000 行）を同一テーブルへ連続投入する成長テーブル
//! 条件で計測する（`harness::protocol::MeasurementConfig` の下限検証を利用）。
//! E0・レプリカは別々の一時 DB へ書き込み、入力データ（id 範囲・可視性・埋め込み・
//! metadata）はバッチ番号から `DeterministicRng` で毎回再生成し両者で一致させる
//! （メモリに保持しない）。
//!
//! # 整合性検証（fail-closed）
//!
//! 1. 計測後、E0 DB・レプリカ DB の `user_rows/docs` 全エントリがバイト単位で一致し、
//!    件数が投入バッチ数 × 行数と一致すること（encode 再実装のドリフト検出）。
//! 2. E0 DB の `op_ledger` エントリ（計測フェーズの各バッチ分）を復号し、
//!    [`content_hash_insert_batch_reimpl`] の結果と一致すること（content_hash
//!    再実装のドリフト検出）。
//! 3. E0 DB・レプリカ DB それぞれの `table_generation["docs"]` が投入バッチ数と
//!    一致すること（世代更新モデルの検証）。
//! 4. I6 段の `insert` 戻り値が常に `None`（新規行）であること。
//!
//! いずれかが不一致ならベンチはエラー終了し測定値を出力しない。
//!
//! # env による可変化（R2）
//!
//! - `BENCH_INGEST_PROFILE_ROWS`: 1 バッチの行数。既定 1,000・許容 1..=10,000。
//! - `BENCH_INGEST_PROFILE_DIM`: 次元。既定 128・許容 1..=4,096。
//!
//! # CI・出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線しない。
//! `GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する。`make
//! bench-ingest-profile`（Makefile）から実行する。判定ロジック自体（時間非依存）は
//! `harness::ingest_profile` にあり `tests/ingest_profile_accept.rs` で
//! `make ci` 側から回帰検証する。redb は本クレートの既存 dev-dependency
//! （`=4.2.0`）を再利用し、新規依存の追加は行わない。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::ingest_profile::{
    content_hash_insert_batch_reimpl, decode_ledger_entry_v2_reimpl, encode_row_reimpl,
    last_op_entry_reimpl, ledger_entry_v2_reimpl, ns_per_row, parse_bounded_env,
    refuse_under_github_actions, render_stage_line, residual_ns_per_row, sum_durations, StageId,
    StageSamples,
};
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;
use harness::stats;

use std::collections::HashSet;
use std::time::Instant;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-ingest";
const TABLE: &str = "docs";
const COLUMN: &str = "embedding";

/// レプリカが直接操作するテーブル定義（`crates/engine/src/catalog.rs::
/// user_rows_table_name` (`"user_rows/{table}"`)・`storage.rs::RowStoreTableDef`・
/// `recovery/ledger.rs::OP_LEDGER_TABLE`/`LAST_OP_TABLE`・
/// `catalog.rs::TABLE_GENERATION_TABLE` の契約をベンチ内で複製したもの。ドリフト
/// 検出は `tests/ingest_profile_accept.rs` が正本との突き合わせで行う
/// （`harness::ingest_profile` 冒頭コメント参照）。
const ROW_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("user_rows/docs");
const OP_LEDGER_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("op_ledger");
const LAST_OP_TABLE: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("last_op");
const TABLE_GENERATION_TABLE: TableDefinition<&str, u64> = TableDefinition::new("table_generation");

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("ingest_profile_bench: {msg}");
    std::process::exit(1);
}

/// 1 バッチ分の投入データ（id・可視性・embedding・metadata）。呼び出し元が
/// `DeterministicRng::new(seed_base + batch_idx)` から毎回再生成し、E0・レプリカの
/// 双方で同一内容になるようにする（メモリに保持しない。モジュール冒頭コメント参照）。
struct Batch {
    ids: Vec<u64>,
    is_public: Vec<bool>,
    embeddings: Vec<Vec<f32>>,
    metadata: Vec<Vec<u8>>,
}

fn make_batch(
    schema: &TableSchema,
    seed_base: u64,
    batch_idx: u64,
    rows: usize,
    dim: usize,
) -> Batch {
    let mut rng = DeterministicRng::new(seed_base.wrapping_add(batch_idx));
    let start_id = batch_idx * rows as u64 + 1;
    let mut ids = Vec::with_capacity(rows);
    let mut is_public = Vec::with_capacity(rows);
    let mut embeddings = Vec::with_capacity(rows);
    let mut metadata = Vec::with_capacity(rows);
    for i in 0..rows {
        let id = start_id + i as u64;
        ids.push(id);
        // feature_bench と同様、10 件に 1 件を Private にする。
        is_public.push(!id.is_multiple_of(10));
        let embedding = rng.next_vector(dim);
        let body = format!(
            "ingest profile bench row id={id} batch={batch_idx} filler text for metadata payload sizing"
        );
        let encoded = encode_scalar_columns(
            schema,
            &[Value::Vector(embedding.clone()), Value::Text(body)],
        )
        .expect("encode_scalar_columns for ingest profile batch");
        embeddings.push(embedding);
        metadata.push(encoded);
    }
    Batch {
        ids,
        is_public,
        embeddings,
        metadata,
    }
}

fn schema(dim: usize) -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new(COLUMN, ColumnType::Vector(dim as u32), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    let rows = match parse_bounded_env(
        "BENCH_INGEST_PROFILE_ROWS",
        std::env::var("BENCH_INGEST_PROFILE_ROWS").ok().as_deref(),
        1_000,
        1,
        10_000,
    ) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let dim = match parse_bounded_env(
        "BENCH_INGEST_PROFILE_DIM",
        std::env::var("BENCH_INGEST_PROFILE_DIM").ok().as_deref(),
        128,
        1,
        4_096,
    ) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!("ingest_profile_bench: rows_per_batch={rows} dim={dim} tenant={TENANT} table={TABLE}");

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let warmup = config.warmup_iterations() as u64;
    let measured = config.measured_iterations() as u64;
    let total_batches = warmup + measured;

    let table_schema = schema(dim);
    let ctx = PolicyContext::new(TENANT).expect("valid tenant id");

    // --- E0: pub API 経由の e2e 計測 --------------------------------------------
    let e2e_path = unique_db_path("issue396-ingest-profile-e2e");
    let _e2e_guard = CleanupGuard(e2e_path.clone());
    let e2e_summary = {
        let e2e_storage = Storage::open(&e2e_path).expect("open e2e storage");
        e2e_storage
            .create_table(&table_schema)
            .expect("create e2e table");

        for batch_idx in 0..warmup {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            insert_e2e_batch(&e2e_storage, &ctx, &batch, batch_idx, "warmup");
        }
        let mut e2e_samples = Vec::with_capacity(measured as usize);
        for batch_idx in warmup..total_batches {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            let elapsed = insert_e2e_batch(&e2e_storage, &ctx, &batch, batch_idx, "measured");
            e2e_samples.push(elapsed);
        }
        if e2e_samples.is_empty() {
            fail_closed("E0 measurement produced no samples");
        }
        let summary = stats::summarize(&e2e_samples).expect("E0 summarize must succeed");
        // `e2e_storage`（内部に redb の書き込み可能ハンドルを保持）をここで明示的に
        // drop してファイルロックを解放する。redb は書き込み可能ハンドルを同一
        // プロセスから同時に複数開けない契約（`redb::DatabaseError::
        // DatabaseAlreadyOpen`）のため、後続の整合性検証（`write_txn_read_only_open`
        // による生 `redb::Database::open` 再オープン）と同時生存させられない
        // （`knn_profile_bench.rs` と同じ理由・同じ対処）。
        drop(e2e_storage);
        summary
    };

    // --- レプリカ: 生 redb による段別計装 -----------------------------------
    let replica_path = unique_db_path("issue396-ingest-profile-replica");
    let _replica_guard = CleanupGuard(replica_path.clone());
    {
        // レプリカ用テーブルは初回 open_table（書き込みモード）で自動作成される
        // （`redb::WriteTransaction::open_table` の契約。`user_rows_table_name`
        // ドキュメント参照）ため、`Storage::create_table` は経由しない
        // （レプリカはカタログ層を再現しない設計。モジュール冒頭コメント参照）。
        let replica_db = Database::create(&replica_path).expect("create replica db");

        for batch_idx in 0..warmup {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            run_replica_batch(&replica_db, &batch, batch_idx, None);
        }
        let mut stage_samples = StageSamples::new();
        for batch_idx in warmup..total_batches {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            run_replica_batch(&replica_db, &batch, batch_idx, Some(&mut stage_samples));
        }

        // --- 段別集計・出力 --------------------------------------------------
        let mut stage_medians = Vec::with_capacity(StageId::ALL.len());
        for stage in StageId::ALL {
            let samples = stage_samples.samples_for(stage);
            let summary = stats::summarize(samples).unwrap_or_else(|e| {
                fail_closed(format!("stage {:?} summarize failed: {e}", stage))
            });
            let npr = ns_per_row(summary.median, rows).unwrap_or_else(|e| fail_closed(e));
            println!(
                "{}",
                render_stage_line(stage.label(), rows, summary.median, npr)
            );
            stage_medians.push(summary.median);
        }
        let stage_sum = sum_durations(&stage_medians);
        let stage_sum_npr = ns_per_row(stage_sum, rows).unwrap_or_else(|e| fail_closed(e));
        println!(
            "stage(SUM_I1_I8): rows={rows} median={:.3}ms ns_per_row={stage_sum_npr:.1}",
            stage_sum.as_secs_f64() * 1e3
        );
        let e0_npr = ns_per_row(e2e_summary.median, rows).unwrap_or_else(|e| fail_closed(e));
        println!(
            "stage(E0_insert_rows): rows={rows} median={:.3}ms ns_per_row={e0_npr:.1}",
            e2e_summary.median.as_secs_f64() * 1e3
        );
        match residual_ns_per_row(e2e_summary.median, stage_sum, rows) {
                Ok(residual_npr) => println!(
                    "residual(E0-SUM): ns_per_row={residual_npr:.1} (schema fetch / commit_boundary guard / abstraction overhead)"
                ),
                Err(e) => println!(
                    "residual(E0-SUM): n/a (Σ(I1..I8) の中央値が E0 の中央値を上回った。独立計測どうしの比較のため測定ノイズにより逆転しうる: {e})"
                ),
            }

        // --- 整合性検証 1: user_rows/docs のバイト単位一致 -----------------------
        // `redb::Database` を drop すると内部状態が「closed」になり、それより前に
        // 取得済みの `ReadTransaction` も後続操作が `StorageError::DatabaseClosed`
        // で失敗する（`redb::db.rs` の実装契約）。そのため `e2e_db` はこのブロックの
        // 終わりまで生存させる（`write_txn_read_only_open` のような「開いて
        // ReadTransaction だけ返す」ヘルパー関数には切り出さない）。
        let e2e_db = Database::open(&e2e_path).expect("reopen E0 db read-only for integrity check");
        let e2e_read = e2e_db.begin_read().expect("begin_read on E0 db");
        let replica_read = replica_db
            .begin_read()
            .expect("begin_read on replica db for integrity check");
        let e2e_rows = collect_row_table(&e2e_read);
        let replica_rows = collect_row_table(&replica_read);
        let expected_count = (total_batches as usize) * rows;
        if e2e_rows.len() != expected_count {
            fail_closed(format!(
                "E0 row count mismatch: expected {expected_count}, got {}",
                e2e_rows.len()
            ));
        }
        if replica_rows.len() != expected_count {
            fail_closed(format!(
                "replica row count mismatch: expected {expected_count}, got {}",
                replica_rows.len()
            ));
        }
        if e2e_rows != replica_rows {
            fail_closed(
                    "user_rows/docs entries differ between E0 and replica DBs (encode_row_reimpl drift)",
                );
        }
        println!(
            "integrity: user_rows/docs byte-identical across E0/replica (count={expected_count})"
        );

        // --- 整合性検証 3: table_generation ----------------------------------
        // E0 側は `Storage::create_table`（DDL）自体も `bump_table_generation_in_txn`
        // を 1 回呼ぶ（`catalog.rs::bump_table_generation_in_txn` ドキュメント
        // 「呼び出し元」列挙参照）ため、投入バッチ数 + 1 になる。レプリカは
        // カタログ層（`create_table`）を再現しない設計（モジュール冒頭コメント
        // 「レプリカ用テーブルは...」参照）のため投入バッチ数のまま。
        let e2e_gen = read_table_generation(&e2e_read);
        let replica_gen = read_table_generation(&replica_read);
        let expected_e2e_gen = total_batches + 1;
        if e2e_gen != expected_e2e_gen {
            fail_closed(format!(
                "E0 table_generation mismatch: expected {expected_e2e_gen}, got {e2e_gen}"
            ));
        }
        if replica_gen != total_batches {
            fail_closed(format!(
                "replica table_generation mismatch: expected {total_batches}, got {replica_gen}"
            ));
        }
        println!(
            "integrity: table_generation == {expected_e2e_gen} (E0, includes create_table) / {total_batches} (replica)"
        );

        // --- 整合性検証 2: 計測フェーズの op_ledger エントリ ↔ content_hash 再実装 --
        let ledger_table = e2e_read
            .open_table(OP_LEDGER_TABLE)
            .expect("open op_ledger table on E0 db");
        let last_op_table = e2e_read
            .open_table(LAST_OP_TABLE)
            .expect("open last_op table on E0 db");
        for batch_idx in warmup..total_batches {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            let op_label = format!("ingest-profile-e2e-{batch_idx}");
            let key = (TENANT, TABLE, op_label.as_str());
            let stored = ledger_table
                .get(key)
                .expect("read op_ledger entry")
                .unwrap_or_else(|| {
                    fail_closed(format!("op_ledger entry missing for batch {batch_idx}"))
                });
            let stored_hash = decode_ledger_entry_v2_reimpl(stored.value())
                .unwrap_or_else(|e| fail_closed(format!("op_ledger entry decode failed: {e}")));
            let encoded_rows: Vec<Vec<u8>> = (0..rows)
                .map(|i| {
                    encode_row_reimpl(
                        TENANT,
                        batch.is_public[i],
                        &batch.embeddings[i],
                        &batch.metadata[i],
                    )
                    .expect("encode_row_reimpl for content_hash cross-check")
                })
                .collect();
            let rows_for_hash: Vec<(u64, &[u8])> = batch
                .ids
                .iter()
                .zip(encoded_rows.iter())
                .map(|(id, enc)| (*id, enc.as_slice()))
                .collect();
            let recomputed = content_hash_insert_batch_reimpl(&rows_for_hash)
                .expect("content_hash_insert_batch_reimpl for cross-check");
            if recomputed != stored_hash {
                fail_closed(format!(
                    "content_hash mismatch for batch {batch_idx} (content_hash reimpl drift)"
                ));
            }
            let last_op_value = last_op_table
                .get((TENANT, TABLE))
                .expect("read last_op entry")
                .unwrap_or_else(|| fail_closed("last_op entry missing"));
            if batch_idx == total_batches - 1
                && last_op_value.value() != last_op_entry_reimpl(&op_label)
            {
                fail_closed("last_op entry does not match last committed batch's operation_id");
            }
        }
        println!("integrity: op_ledger content_hash matches content_hash_insert_batch_reimpl for all measured batches");
    }

    println!("ingest_profile_bench: OK");
}

/// E0: `insert_rows` 1 バッチ分を計測する。`op_id` はレプリカ側の台帳キーと
/// 対応させるため `format!("ingest-profile-e2e-{batch_idx}")` に固定する
/// （整合性検証 2 が同じラベルで `op_ledger` を照会する）。
fn insert_e2e_batch(
    storage: &Storage,
    ctx: &PolicyContext,
    batch: &Batch,
    batch_idx: u64,
    _phase: &str,
) -> std::time::Duration {
    let inputs: Vec<(u64, RowInput<'_>)> = batch
        .ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                *id,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: if batch.is_public[i] {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    },
                    embedding: &batch.embeddings[i],
                    metadata: &batch.metadata[i],
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("ingest-profile-e2e-{batch_idx}"))
        .expect("valid operation id for E0 batch");
    let start = Instant::now();
    tenant::insert_rows(storage, TABLE, ctx, &inputs, &op_id).expect("insert_rows for E0 batch");
    start.elapsed()
}

/// レプリカ 1 バッチ分を `insert_rows_unchecked` と同順序（I1..I8）で再現する。
/// `stage_samples` が `Some` のときだけ段別 `Instant` を記録する（warmup 時は
/// `None` で計測しない）。
fn run_replica_batch(
    db: &Database,
    batch: &Batch,
    batch_idx: u64,
    mut stage_samples: Option<&mut StageSamples>,
) {
    let rows = batch.ids.len();

    // I1: 所有権検査 ＋ バッチ内 id 重複検出。
    let t = Instant::now();
    for is_public in &batch.is_public {
        // 自テナントのみを生成しているため常に true。`is_owner` を実際に呼び、
        // production の判定コストを再現する（Value を消費しないため副作用なし）。
        let ctx = PolicyContext::new(TENANT).expect("valid tenant id");
        let _ = ctx.is_owner(TENANT);
        std::hint::black_box(is_public);
    }
    let mut seen_ids: HashSet<u64> = HashSet::new();
    seen_ids.try_reserve(rows).expect("reserve id set");
    for id in &batch.ids {
        if !seen_ids.insert(*id) {
            fail_closed(format!("unexpected duplicate id in synthetic batch: {id}"));
        }
    }
    record(&mut stage_samples, StageId::Precheck, t.elapsed());

    // I2: begin_write。
    let t = Instant::now();
    let write_txn = db.begin_write().expect("begin_write for replica batch");
    record(&mut stage_samples, StageId::BeginWrite, t.elapsed());

    // I3: content_hash（全行 encode 再実装 ＋ SHA-256）。
    let t = Instant::now();
    let hash_encoded: Vec<Vec<u8>> = (0..rows)
        .map(|i| {
            encode_row_reimpl(
                TENANT,
                batch.is_public[i],
                &batch.embeddings[i],
                &batch.metadata[i],
            )
            .expect("encode_row_reimpl for I3")
        })
        .collect();
    let rows_for_hash: Vec<(u64, &[u8])> = batch
        .ids
        .iter()
        .zip(hash_encoded.iter())
        .map(|(id, enc)| (*id, enc.as_slice()))
        .collect();
    let hash = content_hash_insert_batch_reimpl(&rows_for_hash).expect("content_hash for I3");
    record(&mut stage_samples, StageId::ContentHash, t.elapsed());

    // I4: 台帳記録。
    let t = Instant::now();
    let op_label = format!("ingest-profile-e2e-{batch_idx}");
    {
        let mut ledger_table = write_txn
            .open_table(OP_LEDGER_TABLE)
            .expect("open op_ledger table for I4");
        let key = (TENANT, TABLE, op_label.as_str());
        if ledger_table
            .get(key)
            .expect("read op_ledger for I4")
            .is_some()
        {
            fail_closed(format!(
                "unexpected duplicate op_ledger key for batch {batch_idx}"
            ));
        }
        ledger_table
            .insert(key, ledger_entry_v2_reimpl(&hash).as_slice())
            .expect("insert op_ledger entry for I4");
    }
    {
        let mut last_op_table = write_txn
            .open_table(LAST_OP_TABLE)
            .expect("open last_op table for I4");
        last_op_table
            .insert((TENANT, TABLE), last_op_entry_reimpl(&op_label).as_slice())
            .expect("insert last_op entry for I4");
    }
    record(&mut stage_samples, StageId::Ledger, t.elapsed());

    // I5: encode（行ループ内。I3 と同じ内容を再度 encode する。モジュール冒頭
    // コメント「encode が 2 回」参照）。
    let t = Instant::now();
    let row_encoded: Vec<Vec<u8>> = (0..rows)
        .map(|i| {
            encode_row_reimpl(
                TENANT,
                batch.is_public[i],
                &batch.embeddings[i],
                &batch.metadata[i],
            )
            .expect("encode_row_reimpl for I5")
        })
        .collect();
    record(&mut stage_samples, StageId::Encode, t.elapsed());

    // I6: redb insert（`insert_unique_row` 相当。戻り値が `None` であることを検査）。
    let t = Instant::now();
    {
        let mut row_table = write_txn
            .open_table(ROW_TABLE)
            .expect("open user_rows/docs table for I6");
        for (id, encoded) in batch.ids.iter().zip(row_encoded.iter()) {
            let prev = row_table
                .insert((TENANT, *id), encoded.as_slice())
                .expect("insert row for I6");
            if prev.is_some() {
                fail_closed(format!(
                    "unexpected existing row for id={id} (I6 uniqueness check)"
                ));
            }
        }
    }
    record(&mut stage_samples, StageId::RedbInsert, t.elapsed());

    // I7: 世代更新。
    let t = Instant::now();
    {
        let mut gen_table = write_txn
            .open_table(TABLE_GENERATION_TABLE)
            .expect("open table_generation table for I7");
        let current = gen_table
            .get(TABLE)
            .expect("read table_generation for I7")
            .map(|v| v.value())
            .unwrap_or(0);
        let next = current
            .checked_add(1)
            .expect("table_generation counter overflow");
        gen_table
            .insert(TABLE, next)
            .expect("insert table_generation for I7");
    }
    record(&mut stage_samples, StageId::GenerationBump, t.elapsed());

    // I8: commit。
    let t = Instant::now();
    write_txn.commit().expect("commit for I8");
    record(&mut stage_samples, StageId::Commit, t.elapsed());
}

fn record(
    stage_samples: &mut Option<&mut StageSamples>,
    stage: StageId,
    duration: std::time::Duration,
) {
    if let Some(samples) = stage_samples.as_mut() {
        samples.push(stage, duration);
    }
}

fn collect_row_table(read_txn: &redb::ReadTransaction) -> Vec<((String, u64), Vec<u8>)> {
    let table = read_txn
        .open_table(ROW_TABLE)
        .expect("open user_rows/docs table for integrity check");
    let mut out = Vec::new();
    for entry in table.iter().expect("iterate user_rows/docs table") {
        let (key_guard, value_guard) = entry.expect("row entry");
        let (tenant, id) = key_guard.value();
        out.push(((tenant.to_string(), id), value_guard.value().to_vec()));
    }
    out.sort();
    out
}

fn read_table_generation(read_txn: &redb::ReadTransaction) -> u64 {
    match read_txn.open_table(TABLE_GENERATION_TABLE) {
        Ok(t) => t.get(TABLE).ok().flatten().map(|v| v.value()).unwrap_or(0),
        Err(redb::TableError::TableDoesNotExist(_)) => 0,
        Err(e) => fail_closed(format!(
            "open table_generation for integrity check failed: {e}"
        )),
    }
}
