//! `docs/design/crossdb-bench.md` で self が最劣後する 3 フェーズ（`agg_count`・
//! `rls_isolation`・`vector_knn_where`）の段別内訳プロファイル（Issue #464。
//! 親 Issue #456・ルート Issue #455）。
//!
//! production コード（`crates/engine/src/`）は無変更のテスト・ベンチ・docs 専任
//! タスク。段の定義・測定設計・fail-closed 整合性検証は
//! `benches/harness/scan_stage_profile.rs` 冒頭コメント・
//! `docs/design/scan-stage-profile.md` を正本とする。
//!
//! # コーパス（crossdb モデルの再現）
//!
//! tenant-a `TENANT_A_ROWS`（既定 23,000）行 `Visibility::Public`・tenant-b
//! `TENANT_B_ROWS`（既定 2,000）行 `Visibility::Private`（`BENCH_SCAN_PROFILE_SCALE`
//! で両方とも倍率適用）。`PolicyContext::new(tenant)`（`Visibility::Public` のみ
//! 許可）で `agg_count`（ctx=tenant-a）・`rls_isolation`（ctx=tenant-b）を測ると、
//! いずれも可視集合はテナント横断の Public 行（tenant-a の全行）のみになり、
//! 同一走査・同一 `COUNT(*)` を返す（`docs/design/crossdb-bench.md` の実測
//! 3,547µs/3,552µs の近さと整合）。`lang` 列は 5 値輪番（`ja` ≒ 20%）。
//!
//! # 出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線
//! しない。`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する。
//! `make bench-scan-stage-profile`（Makefile）から実行する。

#[allow(dead_code)]
mod harness;

use harness::bench_engine;
use harness::env_report::EnvReport;
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::scan_stage_profile::{
    assert_scan_row_counts_match, build_lang_filter, classify_against_bands,
    decode_dim_and_metadata_reimpl, is_visible, matches_lang_filter, median_of, min_of, ns_per_row,
    parse_rounds, parse_scale, reference_band, refuse_under_github_actions, render_bucket_line,
    render_diff_line, render_stage_line, scan_scalar_columns, stage_diff_ns_per_row,
    step_ratio_pct, verify_row_key_tenant_reimpl,
};
use harness::stats;

use std::hint::black_box;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine;
use engine::sql::exec::Cell;
use engine::storage::{RowInput, Storage, Visibility};
use engine::{arena::VectorArena, tenant};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 128;
const TENANT_A: &str = "tenant-a";
const TENANT_A_ROWS_BASE: u64 = 23_000;
const TENANT_B: &str = "tenant-b";
const TENANT_B_ROWS_BASE: u64 = 2_000;
const TOP_K: usize = 10;
const TABLE: &str = "docs";
const SEED_BATCH_ROWS: u64 = 5_000;
const LANGS: &[&str] = &["ja", "en", "fr", "de", "es"];
const TARGET_LANG: &str = "ja";

/// 生 `redb::Database` 再オープン時に走査する行テーブル（`knn_profile_bench.rs::
/// ROW_TABLE` と同一の再実装。ドリフト検出は `tests/scan_stage_profile_accept.rs`
/// が `Storage::scan()` との突き合わせで行う）。
const ROW_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("user_rows/docs");

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("scan_stage_profile_bench: {msg}");
    std::process::exit(1);
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// A1〜A5 の 1 ラウンド分測定（戻り値: 中央値 5 本・可視行数）。
struct RoundResultA {
    a1: Duration,
    a2: Duration,
    a3: Duration,
    a4: Duration,
    a5: Duration,
    visible_rows: usize,
}

fn measure_a_series(
    db: &Database,
    config: &MeasurementConfig,
    ctx: &PolicyContext,
    schema: &TableSchema,
) -> RoundResultA {
    // A1: per-entry 走査のみ（ヘッダを読まない）。
    let a1 = run(config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let _entry = entry.expect("iterate row entry");
            checksum = checksum.wrapping_add(std::hint::black_box(1u64));
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");

    // A2: A1 ＋ ヘッダデコード（tenant_id・visibility）。
    let a2 = run(config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let (tenant_id, is_public, _offset) =
                harness::knn_profile::decode_header_reimpl(v.value())
                    .expect("header decode must succeed for well-formed synthetic rows");
            checksum = checksum.wrapping_add(std::hint::black_box(tenant_id.len() as u64));
            checksum = checksum.wrapping_add(std::hint::black_box(is_public as u64));
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");

    // A3: A2 ＋ RLS 判定（`PolicyContext::is_visible`）＋ TABLE-12 キー/ヘッダ
    // tenant 整合検査。不可視行は以降の追加処理を行わない（production と同じ
    // 「不可視行は dim/metadata を一切デコードしない」順序）。
    let a3 = run(config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut visible = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (k, v) = entry.expect("iterate row entry");
            let (tenant_id, is_public, _offset) =
                harness::knn_profile::decode_header_reimpl(v.value())
                    .expect("header decode must succeed for well-formed synthetic rows");
            let visibility = if is_public {
                Visibility::Public
            } else {
                Visibility::Private
            };
            if is_visible(ctx, tenant_id, visibility) {
                let (key_tenant, _id) = k.value();
                verify_row_key_tenant_reimpl(key_tenant, tenant_id)
                    .expect("row key tenant must match header tenant for well-formed rows");
                checksum = checksum.wrapping_add(std::hint::black_box(1u64));
                visible += 1;
            }
            rows += 1;
        }
        (rows, visible, checksum)
    })
    .expect("measurement must satisfy protocol minimums");
    let a3_visible = {
        let read_txn = db.begin_read().expect("begin read txn for visible count");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut visible = 0usize;
        for entry in table.iter().expect("iter row table") {
            let (k, v) = entry.expect("iterate row entry");
            let (tenant_id, is_public, _offset) =
                harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
            let visibility = if is_public {
                Visibility::Public
            } else {
                Visibility::Private
            };
            if is_visible(ctx, tenant_id, visibility) {
                let (key_tenant, _id) = k.value();
                verify_row_key_tenant_reimpl(key_tenant, tenant_id).expect("tenant match");
                visible += 1;
            }
        }
        visible
    };

    // A4: A3（RLS 判定＋キー/ヘッダ tenant 整合検査）＋ dim・metadata 借用デコード
    // （可視行のみ）。累積段契約（A3 ⊆ A4 ⊆ A5）を保つため、A3 が行う
    // `verify_row_key_tenant_reimpl` はここでも省略せず実行する（省略すると
    // A4−A3 の差分がデコード追加コストではなく整合性検査コスト分だけ過小に
    // 出てしまい、後続の性能改善の帰属を誤る）。
    let a4 = run(config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (k, v) = entry.expect("iterate row entry");
            let (tenant_id, is_public, _offset) =
                harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
            let visibility = if is_public {
                Visibility::Public
            } else {
                Visibility::Private
            };
            if is_visible(ctx, tenant_id, visibility) {
                let (key_tenant, _id) = k.value();
                verify_row_key_tenant_reimpl(key_tenant, tenant_id)
                    .expect("row key tenant must match header tenant for well-formed rows");
                let (dim, metadata) = decode_dim_and_metadata_reimpl(v.value())
                    .expect("dim/metadata decode must succeed for well-formed synthetic rows");
                checksum = checksum.wrapping_add(std::hint::black_box(dim as u64));
                checksum = checksum.wrapping_add(std::hint::black_box(metadata.len() as u64));
            }
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");

    // A5: A4（キー/ヘッダ tenant 整合検査を含む）＋
    // `row_codec::validate_scalar_columns`（可視行のみ）。A4 と同じ理由で
    // `verify_row_key_tenant_reimpl` を省略しない。
    let a5 = run(config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (k, v) = entry.expect("iterate row entry");
            let (tenant_id, is_public, _offset) =
                harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
            let visibility = if is_public {
                Visibility::Public
            } else {
                Visibility::Private
            };
            if is_visible(ctx, tenant_id, visibility) {
                let (key_tenant, _id) = k.value();
                verify_row_key_tenant_reimpl(key_tenant, tenant_id)
                    .expect("row key tenant must match header tenant for well-formed rows");
                let (dim, metadata) =
                    decode_dim_and_metadata_reimpl(v.value()).expect("dim/metadata decode");
                engine::row_codec::validate_scalar_columns(schema, metadata)
                    .expect("scalar column structure must be valid for well-formed synthetic rows");
                checksum = checksum.wrapping_add(std::hint::black_box(dim as u64));
            }
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");

    RoundResultA {
        a1: a1.summary.median,
        a2: a2.summary.median,
        a3: a3.summary.median,
        a4: a4.summary.median,
        a5: a5.summary.median,
        visible_rows: a3_visible,
    }
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    let rounds_raw = match bench_engine::read_env_var("BENCH_SCAN_PROFILE_ROUNDS") {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_ROUNDS: {e}")),
    };
    let rounds = match parse_rounds(rounds_raw.as_deref()) {
        Ok(r) => r,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_ROUNDS: {e}")),
    };
    let scale_raw = match bench_engine::read_env_var("BENCH_SCAN_PROFILE_SCALE") {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_SCALE: {e}")),
    };
    let scale = match parse_scale(scale_raw.as_deref()) {
        Ok(s) => s,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_SCALE: {e}")),
    };

    let tenant_a_rows = TENANT_A_ROWS_BASE * scale;
    let tenant_b_rows = TENANT_B_ROWS_BASE * scale;
    let total_physical_rows = (tenant_a_rows + tenant_b_rows) as usize;

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "scan_stage_profile_bench: rows={total_physical_rows} dim={DIM} rounds={rounds} scale={scale} (tenant_a={tenant_a_rows} public, tenant_b={tenant_b_rows} private)"
    );

    let schema = schema();
    let path = unique_db_path("issue464-scan-stage-profile");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    storage
        .create_table(&schema)
        .expect("create table for bench seeding");

    let mut rng = DeterministicRng::new(1);
    let mut next_id: u64 = 0;
    for (tenant_id, count, visibility) in [
        (TENANT_A, tenant_a_rows, Visibility::Public),
        (TENANT_B, tenant_b_rows, Visibility::Private),
    ] {
        let ctx =
            PolicyContext::with_visibilities(tenant_id, [Visibility::Public, Visibility::Private])
                .expect("valid tenant id");
        let mut remaining = count;
        while remaining > 0 {
            let batch_len = SEED_BATCH_ROWS.min(remaining) as usize;
            let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
            let mut batch_metadata: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
            for i in 0..batch_len {
                let id = next_id + i as u64;
                let v = rng.next_vector(DIM);
                let lang = LANGS[(id as usize) % LANGS.len()];
                let metadata = encode_scalar_columns(
                    &schema,
                    &[Value::Vector(v.clone()), Value::Text(lang.to_string())],
                )
                .expect("encode_scalar_columns for bench seeding");
                batch_vectors.push(v);
                batch_metadata.push(metadata);
            }
            let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
                .map(|i| {
                    let id = next_id + i as u64;
                    (
                        id,
                        RowInput {
                            tenant_id,
                            visibility,
                            embedding: &batch_vectors[i],
                            metadata: &batch_metadata[i],
                        },
                    )
                })
                .collect();
            let op_id = OperationId::parse(&format!("seed-{tenant_id}-{next_id}"))
                .expect("valid operation_id");
            tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id).expect("seed batch insert");
            next_id += batch_len as u64;
            remaining -= batch_len as u64;
        }
    }
    if next_id as usize != total_physical_rows {
        fail_closed(format!(
            "seeded row count mismatch: expected {total_physical_rows}, got {next_id}"
        ));
    }

    let ctx_a = PolicyContext::new(TENANT_A).expect("valid tenant id");
    let ctx_b = PolicyContext::new(TENANT_B).expect("valid tenant id");
    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let query = rng.next_vector(DIM);

    // --- W 系列の事前準備: 可視行（ctx_a）の metadata・embedding を計測外で捕捉する。
    // production の `on_visible_row` フックと同じ順序（RLS 段を通過した行だけへ到達）
    // を pub API（`VectorArena::build_filtered_with_rows`）で再現する。
    let mut captured_metadata: Vec<Vec<u8>> = Vec::new();
    let mut captured_ids: Vec<u64> = Vec::new();
    let arena = VectorArena::build_filtered_with_rows(
        &storage,
        TABLE,
        |tenant, visibility| ctx_a.is_visible(tenant, visibility),
        |_slot, id, _embedding, metadata| {
            captured_ids.push(id);
            captured_metadata.push(metadata.to_vec());
            Ok(true)
        },
    )
    .expect("arena build must succeed for well-formed synthetic corpus");
    if arena.len() != tenant_a_rows as usize {
        fail_closed(format!(
            "visible arena row count mismatch: expected {tenant_a_rows}, got {}",
            arena.len()
        ));
    }
    if captured_metadata.len() != arena.len() {
        fail_closed("captured metadata count does not match arena row count");
    }

    // 計測外で `lang = 'ja'` の期待一致件数を求める（W2 の整合性検証に使う）。
    // `expected_match_ids` は W1/W2 で実際に測定するコード経路（`scan_scalar_columns`
    // ＋`matches_lang_filter`）を一切通さず、シード時に確定している「id → lang」の
    // 対応規則（`LANGS[(id as usize) % LANGS.len()]`。上記シードループ参照）から
    // 直接導出する。`scan_scalar_columns`／`matches_lang_filter` 自体に退行が
    // 混入した場合でも、同じロジックで期待値を作ってしまうと W2 の整合性検証が
    // 常に一致してしまい検出できない（P2 指摘・codex-review）ため、検証対象の
    // ロジックから独立した根拠が必要。
    let filters = vec![build_lang_filter(&schema, "lang", TARGET_LANG)
        .expect("build_lang_filter must succeed for well-formed schema")];
    let expected_match_ids: Vec<u64> = captured_ids
        .iter()
        .copied()
        .filter(|id| LANGS[(*id as usize) % LANGS.len()] == TARGET_LANG)
        .collect();
    if expected_match_ids.is_empty() {
        fail_closed("no rows matched lang = 'ja' in the synthetic corpus (fixture misconfigured)");
    }

    // --- ラウンド輪番: A1〜A5・W1〜W4・R_dot を rounds 回ずつ測定する。--------------
    let mut a1_rounds = Vec::with_capacity(rounds as usize);
    let mut a2_rounds = Vec::with_capacity(rounds as usize);
    let mut a3_rounds = Vec::with_capacity(rounds as usize);
    let mut a4_rounds = Vec::with_capacity(rounds as usize);
    let mut a5_rounds = Vec::with_capacity(rounds as usize);
    let mut w1_rounds = Vec::with_capacity(rounds as usize);
    let mut w2_rounds = Vec::with_capacity(rounds as usize);
    let mut w3_rounds = Vec::with_capacity(rounds as usize);
    let mut w4_rounds = Vec::with_capacity(rounds as usize);
    let mut r_dot_rounds = Vec::with_capacity(rounds as usize);
    let mut a3_visible_counts: Vec<usize> = Vec::new();

    let db = {
        // `storage`（既存の書き込みハンドル）を保持したまま生 redb を同時に開くと
        // `DatabaseAlreadyOpen` になるため（`knn_profile_bench.rs` と同じ理由）、
        // ここでは drop せず読み取り専用の別ハンドルとして再利用可能な形にする
        // ため、A 系列測定の直前で明示的に `drop(storage)` してから開く。
        drop(storage);
        Database::open(&path).expect("reopen raw database for A series")
    };

    let parallel_provider = ParallelSearchProvider;

    for round in 0..rounds {
        let a = measure_a_series(&db, &config, &ctx_a, &schema);
        if a.visible_rows != tenant_a_rows as usize {
            fail_closed(format!(
                "round {round}: A3 visible row count mismatch: expected {tenant_a_rows}, got {}",
                a.visible_rows
            ));
        }
        a3_visible_counts.push(a.visible_rows);
        a1_rounds.push(a.a1);
        a2_rounds.push(a.a2);
        a3_rounds.push(a.a3);
        a4_rounds.push(a.a4);
        a5_rounds.push(a.a5);

        // rls_isolation（ctx_b）が agg_count（ctx_a）と同一走査であることを、
        // 毎ラウンド A3 の可視行数一致で確認する（COUNT(*) 値そのものは別途
        // e2e 検証する）。
        let b_visible = {
            let read_txn = db.begin_read().expect("begin read txn");
            let table = read_txn.open_table(ROW_TABLE).expect("open row table");
            let mut visible = 0usize;
            for entry in table.iter().expect("iter row table") {
                let (_k, v) = entry.expect("iterate row entry");
                let (tenant_id, is_public, _offset) =
                    harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
                let visibility = if is_public {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                if is_visible(&ctx_b, tenant_id, visibility) {
                    visible += 1;
                }
            }
            visible
        };
        if b_visible != a.visible_rows {
            fail_closed(format!(
                "round {round}: rls_isolation(ctx_b) visible row count {b_visible} does not match agg_count(ctx_a) {}",
                a.visible_rows
            ));
        }

        // W1: 可視行 metadata へ `scan_scalar_columns`。
        let w1 = run(&config, || {
            let mut checksum = 0u64;
            for metadata in &captured_metadata {
                let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
                checksum = checksum.wrapping_add(std::hint::black_box(scanned.len() as u64));
            }
            checksum
        })
        .expect("measurement must satisfy protocol minimums");
        w1_rounds.push(w1.summary.median);

        // W2: W1 ＋ `lang = 'ja'` 判定。
        let w2 = run(&config, || {
            let mut matched = 0u64;
            for metadata in &captured_metadata {
                let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
                if matches_lang_filter(&filters, &scanned) {
                    matched = matched.wrapping_add(std::hint::black_box(1u64));
                }
            }
            matched
        })
        .expect("measurement must satisfy protocol minimums");
        w2_rounds.push(w2.summary.median);
        let w2_match_count = {
            let mut matched = 0usize;
            for metadata in &captured_metadata {
                let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
                if matches_lang_filter(&filters, &scanned) {
                    matched += 1;
                }
            }
            matched
        };
        if w2_match_count != expected_match_ids.len() {
            fail_closed(format!(
                "round {round}: W2 lang='ja' match count mismatch: expected {}, got {w2_match_count}",
                expected_match_ids.len()
            ));
        }

        // W3: W2 一致行 embedding を連続 `Vec<f32>` へ複製する。
        let dim = arena.dim() as usize;
        let w3 = run(&config, || {
            let mut copied: Vec<f32> = Vec::with_capacity(expected_match_ids.len() * dim);
            for (idx, metadata) in captured_metadata.iter().enumerate() {
                let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
                if matches_lang_filter(&filters, &scanned) {
                    let vector = arena.vector(idx).expect("arena vector for captured index");
                    copied.extend_from_slice(vector);
                }
            }
            copied
        })
        .expect("measurement must satisfy protocol minimums");
        w3_rounds.push(w3.summary.median);

        // W4: 一致行のみへ Top-k 探索。
        let mut matched_ids: Vec<u64> = Vec::with_capacity(expected_match_ids.len());
        let mut matched_vectors: Vec<f32> = Vec::with_capacity(expected_match_ids.len() * dim);
        for (idx, metadata) in captured_metadata.iter().enumerate() {
            let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
            if matches_lang_filter(&filters, &scanned) {
                matched_ids.push(captured_ids[idx]);
                matched_vectors.extend_from_slice(arena.vector(idx).expect("arena vector"));
            }
        }
        if matched_ids != expected_match_ids {
            fail_closed(format!(
                "round {round}: W3/W4 matched id set diverged from independently derived expected_match_ids (predicate regression suspected)"
            ));
        }
        let w4 = run(&config, || {
            parallel_provider
                .search(SearchInput {
                    ids: &matched_ids,
                    vectors: &matched_vectors,
                    dim: dim as u32,
                    query: &query,
                    k: TOP_K,
                })
                .expect("search must succeed for well-formed synthetic input")
        })
        .expect("measurement must satisfy protocol minimums");
        w4_rounds.push(w4.summary.median);

        // R_dot: 参照区間（変更を含まない区間）。全可視行への逐次内積総和。
        let r_dot = run(&config, || {
            let mut acc = 0.0f32;
            for chunk in arena.vectors().chunks_exact(dim) {
                acc += engine::isa::current().dot(chunk, &query);
            }
            acc
        })
        .expect("measurement must satisfy protocol minimums");
        r_dot_rounds.push(r_dot.summary.median);
    }

    if let Err(e) = assert_scan_row_counts_match(&[
        ("A3_visible_round0", a3_visible_counts[0]),
        (
            "A3_visible_last",
            *a3_visible_counts.last().expect("at least one round"),
        ),
    ]) {
        fail_closed(e);
    }

    // `db`（生 `redb::Database` ハンドル・A 系列専用）を drop してファイルロックを
    // 解放する（以降の e2e 測定は `Storage::open`／`EngineCore` 経由の pub API の
    // みで DB へアクセスする。`knn_profile_bench.rs` と同じ理由で、生ハンドルと
    // 書き込み可能ハンドルの同時生存は redb の `DatabaseAlreadyOpen` を招く）。
    drop(db);

    // --- e2e（EngineCore::execute_sql）: W0c/W0h/W0n・COUNT(*)（A0a/A0b）。--------
    let sql_where = format!(
        "SELECT id FROM {TABLE} WHERE lang = '{TARGET_LANG}' ORDER BY embedding <=> '{}' LIMIT {TOP_K}",
        vector_literal(&query)
    );
    let sql_nowhere = format!(
        "SELECT id FROM {TABLE} ORDER BY embedding <=> '{}' LIMIT {TOP_K}",
        vector_literal(&query)
    );
    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");

    // W0c: 毎サンプル新規 `EngineCore` から測る（`SqlArenaCache` を空の状態から）。
    for _ in 0..config.warmup_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for W0-cold warmup");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        black_box(
            cold_core
                .execute_sql(&ctx_a, &sql_where)
                .expect("execute_sql must succeed for well-formed synthetic WHERE query"),
        );
    }
    let mut w0_cold_samples: Vec<Duration> =
        Vec::with_capacity(config.measured_iterations() as usize);
    let mut w0_cold_last_ids: Option<Vec<u64>> = None;
    for _ in 0..config.measured_iterations() {
        // 計測区間は `execute_sql` のみ（`Storage::open`／`EngineCore` 構築は
        // 含まない）。`docs/design/scan-stage-profile.md`「W0-cold − W0-hot で
        // `SqlArenaCache` の寄与を示す」という既存の解釈は、W0-hot（同一
        // `EngineCore` を使い回す `execute_sql` のみの区間）との差分が
        // `SqlArenaCache` のヒット/ミスにのみ帰属することを前提にしている。
        // ここに `Storage::open`／`EngineCore` 構築コストを含めると DB 起動
        // コストが混入し前提が崩れるため、この区間には含めない（PR #586
        // codex-review・Cursor Bugbot 指摘。Issue #479 で新設した `A0c-cold`
        // は `VisibleBitmapCache` のミス経路を測る別目的の測定点であり、
        // `Storage::open` を含めて測る設計は `A0c-cold` 側だけの事情。
        // W0-cold をそれに合わせて変更する必要はない）。
        let cold_storage = Storage::open(&path).expect("reopen storage for W0-cold measurement");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        let start = Instant::now();
        let result = black_box(
            cold_core
                .execute_sql(&ctx_a, &sql_where)
                .expect("execute_sql must succeed for well-formed synthetic WHERE query"),
        );
        w0_cold_samples.push(start.elapsed());
        w0_cold_last_ids = Some(
            result
                .rows
                .iter()
                .map(|row| match row.cells.first() {
                    Some(Cell::Integer(v)) => *v,
                    other => fail_closed(format!("W0-cold id cell type mismatch: got {other:?}")),
                })
                .collect(),
        );
    }
    if w0_cold_samples.is_empty() {
        fail_closed("W0-cold measurement produced no samples");
    }
    let w0_cold_summary = stats::summarize(&w0_cold_samples).expect("W0-cold summarize");
    let w0_cold_ids = w0_cold_last_ids.expect("W0-cold produced at least one sample");
    if w0_cold_ids.len() != TOP_K {
        fail_closed(format!(
            "W0-cold result row count mismatch: expected {TOP_K}, got {}",
            w0_cold_ids.len()
        ));
    }
    if w0_cold_ids
        .iter()
        .any(|id| !expected_match_ids.contains(id))
    {
        fail_closed("W0-cold returned an id outside the lang='ja' visible set (tenant/filter leak suspected)");
    }

    // A0c（Issue #479）: 毎サンプル新規 `Storage::open` ＋ `EngineCore`
    // （空の `VisibleBitmapCache`〔Issue #478〕）から `COUNT(*)` を測る。
    // 後段の A0a／A0b は同一 `EngineCore` を使い回すため 2 回目以降は必ず
    // 本キャッシュのヒット経路を測る（`sql/visible_cache.rs::execute_aggregate_with_cache`
    // が `user_rows/{table}` を一切開かない経路）。A0c はそのミス経路（走査に
    // 相乗りしたスナップショット構築を含む）を、before（キャッシュ非搭載）と
    // after（本キャッシュ搭載）の交互実測で比較できるようにするための対照値
    // （before では常に全行走査、after では構築コストを含むミス経路）。
    // `Storage::open`／`EngineCore` 構築コストを計測区間に含めるのは A0c
    // 自身の設計判断であり、W0-cold（`execute_sql` のみを計測。上記コメント
    // 参照）とは意図的に異なる区間を採る（PR #586 codex-review・Cursor
    // Bugbot 指摘。`docs/design/visible-bitmap-cache-verification.md` 参照）。
    // W0c の生 DB ハンドルは既に drop 済みだが、W0-hot/A0a/A0b 用の
    // `core`（同一 DB を開いたまま保持する）はまだ開いていないため、ここで
    // `Storage::open` の二重オープン（`DatabaseAlreadyOpen`）を避けられる。
    for _ in 0..config.warmup_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for A0-cold warmup");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        black_box(
            cold_core
                .execute_sql(&ctx_a, &count_sql)
                .expect("execute_sql must succeed for COUNT(*) query"),
        );
    }
    let mut a0c_samples: Vec<Duration> = Vec::with_capacity(config.measured_iterations() as usize);
    let mut a0c_last_value: Option<u64> = None;
    for _ in 0..config.measured_iterations() {
        // 計測区間は `Storage::open` を含む（本節冒頭のコメント・
        // `docs/design/visible-bitmap-cache-verification.md` の前提と一致させる）。
        let start = Instant::now();
        let cold_storage = Storage::open(&path).expect("reopen storage for A0-cold measurement");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        let result = black_box(
            cold_core
                .execute_sql(&ctx_a, &count_sql)
                .expect("execute_sql must succeed for COUNT(*) query"),
        );
        a0c_samples.push(start.elapsed());
        if result.rows.len() != 1 {
            fail_closed(format!(
                "A0-cold COUNT(*) row count mismatch: expected 1, got {}",
                result.rows.len()
            ));
        }
        a0c_last_value = Some(match result.rows[0].cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => fail_closed(format!(
                "A0-cold COUNT(*) cell type mismatch: got {other:?}"
            )),
        });
    }
    if a0c_samples.is_empty() {
        fail_closed("A0-cold measurement produced no samples");
    }
    let a0c_summary = stats::summarize(&a0c_samples).expect("A0-cold summarize");
    match a0c_last_value {
        Some(value) if value == tenant_a_rows => {}
        Some(value) => fail_closed(format!(
            "A0-cold COUNT(*) value mismatch: expected {tenant_a_rows}, got {value}"
        )),
        None => fail_closed("A0-cold measurement produced no COUNT(*) value"),
    }

    let storage = Storage::open(&path).expect("reopen storage for W0-hot/W0-nowhere/A0");
    let core = EngineCore::from_storage(storage, search_engine::default_engine());

    let w0_hot = run(&config, || {
        core.execute_sql(&ctx_a, &sql_where)
            .expect("execute_sql must succeed for well-formed synthetic WHERE query")
    })
    .expect("measurement must satisfy protocol minimums");
    let w0_hot_result = core
        .execute_sql(&ctx_a, &sql_where)
        .expect("execute_sql must succeed for well-formed synthetic WHERE query");
    if w0_hot_result.rows.len() != TOP_K {
        fail_closed(format!(
            "W0-hot result row count mismatch: expected {TOP_K}, got {}",
            w0_hot_result.rows.len()
        ));
    }
    // cold 側と同じ「返却 id が lang='ja' の可視集合に含まれる」検証を hot 側にも
    // 課す。cache ヒット経路（`SqlArenaCache`／`sql/hnsw_cache.rs` 等）はここでしか
    // 通過せず、キャッシュヒット時にフィルタが未適用のまま別の行が返っても件数
    // だけの確認では検出できない（P2 指摘）。
    let w0_hot_ids: Vec<u64> = w0_hot_result
        .rows
        .iter()
        .map(|row| match row.cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => fail_closed(format!("W0-hot id cell type mismatch: got {other:?}")),
        })
        .collect();
    if w0_hot_ids.iter().any(|id| !expected_match_ids.contains(id)) {
        fail_closed(
            "W0-hot returned an id outside the lang='ja' visible set (tenant/filter leak suspected, possibly via a stale cache hit)",
        );
    }

    let w0_nowhere = run(&config, || {
        core.execute_sql(&ctx_a, &sql_nowhere)
            .expect("execute_sql must succeed for well-formed synthetic KNN query")
    })
    .expect("measurement must satisfy protocol minimums");

    let a0a = run(&config, || {
        core.execute_sql(&ctx_a, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");
    let a0b = run(&config, || {
        core.execute_sql(&ctx_b, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");

    let count_a = core
        .execute_sql(&ctx_a, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    let count_b = core
        .execute_sql(&ctx_b, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    for (label, result) in [("ctx_a", &count_a), ("ctx_b", &count_b)] {
        if result.rows.len() != 1 {
            fail_closed(format!(
                "COUNT(*) row count mismatch ({label}): expected 1, got {}",
                result.rows.len()
            ));
        }
        let value = match result.rows[0].cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => fail_closed(format!(
                "COUNT(*) cell type mismatch ({label}): got {other:?}"
            )),
        };
        if value != tenant_a_rows {
            fail_closed(format!(
                "COUNT(*) value mismatch ({label}): expected {tenant_a_rows}, got {value}"
            ));
        }
    }

    // --- 出力: 全測定・全整合性検証を完了したここまでの間、測定値は一切
    // println! していない（fail-closed 契約。`knn_profile_bench.rs` と同方針）。
    let a1_min = min_of(&a1_rounds).expect("rounds >= 1");
    let a1_med = median_of(&a1_rounds).expect("rounds >= 1");
    let a2_min = min_of(&a2_rounds).expect("rounds >= 1");
    let a2_med = median_of(&a2_rounds).expect("rounds >= 1");
    let a3_min = min_of(&a3_rounds).expect("rounds >= 1");
    let a3_med = median_of(&a3_rounds).expect("rounds >= 1");
    let a4_min = min_of(&a4_rounds).expect("rounds >= 1");
    let a4_med = median_of(&a4_rounds).expect("rounds >= 1");
    let a5_min = min_of(&a5_rounds).expect("rounds >= 1");
    let a5_med = median_of(&a5_rounds).expect("rounds >= 1");
    let w1_min = min_of(&w1_rounds).expect("rounds >= 1");
    let w1_med = median_of(&w1_rounds).expect("rounds >= 1");
    let w2_min = min_of(&w2_rounds).expect("rounds >= 1");
    let w2_med = median_of(&w2_rounds).expect("rounds >= 1");
    let w3_min = min_of(&w3_rounds).expect("rounds >= 1");
    let w3_med = median_of(&w3_rounds).expect("rounds >= 1");
    let w4_min = min_of(&w4_rounds).expect("rounds >= 1");
    let w4_med = median_of(&w4_rounds).expect("rounds >= 1");
    let r_dot_min = min_of(&r_dot_rounds).expect("rounds >= 1");
    let r_dot_med = median_of(&r_dot_rounds).expect("rounds >= 1");
    let ref_band_pct = reference_band(&r_dot_rounds).unwrap_or(0.0) * 100.0;

    println!("--- per-round raw medians (ms) ---");
    for (round, (((((((((a1, a2), a3), a4), a5), w1), w2), w3), w4), r_dot)) in a1_rounds
        .iter()
        .zip(&a2_rounds)
        .zip(&a3_rounds)
        .zip(&a4_rounds)
        .zip(&a5_rounds)
        .zip(&w1_rounds)
        .zip(&w2_rounds)
        .zip(&w3_rounds)
        .zip(&w4_rounds)
        .zip(&r_dot_rounds)
        .enumerate()
    {
        println!(
            "round[{round}]: A1={:.3} A2={:.3} A3={:.3} A4={:.3} A5={:.3} W1={:.3} W2={:.3} W3={:.3} W4={:.3} R_dot={:.3}",
            a1.as_secs_f64() * 1e3,
            a2.as_secs_f64() * 1e3,
            a3.as_secs_f64() * 1e3,
            a4.as_secs_f64() * 1e3,
            a5.as_secs_f64() * 1e3,
            w1.as_secs_f64() * 1e3,
            w2.as_secs_f64() * 1e3,
            w3.as_secs_f64() * 1e3,
            w4.as_secs_f64() * 1e3,
            r_dot.as_secs_f64() * 1e3,
        );
    }

    println!("--- A series (median-of-R / min-of-R, ns/row over total physical rows) ---");
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A1_redb_scan",
            total_physical_rows,
            a1_med,
            ns_per_row(a1_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a1_min.as_secs_f64() * 1e3
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A2_header_decode",
            total_physical_rows,
            a2_med,
            ns_per_row(a2_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a2_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A1",
        "A2",
        a1_med,
        a2_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A3_rls_visible",
            total_physical_rows,
            a3_med,
            ns_per_row(a3_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a3_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A2",
        "A3",
        a2_med,
        a3_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A4_dim_meta_decode",
            total_physical_rows,
            a4_med,
            ns_per_row(a4_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a4_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A3",
        "A4",
        a3_med,
        a4_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A5_scalar_validate",
            total_physical_rows,
            a5_med,
            ns_per_row(a5_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a5_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A4",
        "A5",
        a4_med,
        a5_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "e2e(agg_count/A0a, ctx=tenant-a): median={:.3}ms",
        a0a.summary.median.as_secs_f64() * 1e3
    );
    println!(
        "e2e(rls_isolation/A0b, ctx=tenant-b): median={:.3}ms",
        a0b.summary.median.as_secs_f64() * 1e3
    );
    // A0c-cold は `rounds`（A1〜A5・W1〜W4・R_dot が使う「ラウンド」概念）の
    // 系列ではなく、`config.measured_iterations()`（本ベンチでは 20）個の
    // 生サンプルを直接集計している（`MeasurementConfig::new` 呼び出し・上の
    // `for _ in 0..config.measured_iterations()` ループ参照）。他系列の
    // 「min-of-R」（R = ラウンド数。既定 `BENCH_SCAN_PROFILE_ROUNDS`）と表記を
    // 揃えると集計対象が異なるにもかかわらず同じ略記になり誤解を招くため
    // （PR #586 codex-review 指摘）、ここでは実際のサンプル数を明記した
    // 「sample minimum (N=<count>)」表記を用いる。
    println!(
        "e2e(agg_count/A0c-cold, ctx=tenant-a, includes Storage::open): median={:.3}ms (sample minimum, N={}: {:.3}ms)",
        a0c_summary.median.as_secs_f64() * 1e3,
        a0c_samples.len(),
        a0c_samples
            .iter()
            .min()
            .expect("A0-cold has at least one sample")
            .as_secs_f64()
            * 1e3
    );

    let visible_rows = tenant_a_rows as usize;
    println!(
        "--- W series (median-of-R / min-of-R, ns/row over visible (tenant-a Public) rows) ---"
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "W1_scalar_scan",
            visible_rows,
            w1_med,
            ns_per_row(w1_med, visible_rows).expect("visible_rows > 0"),
        ),
        w1_min.as_secs_f64() * 1e3
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "W2_predicate",
            visible_rows,
            w2_med,
            ns_per_row(w2_med, visible_rows).expect("visible_rows > 0"),
        ),
        w2_min.as_secs_f64() * 1e3
    );
    report_diff("W1", "W2", w1_med, w2_med, visible_rows, ref_band_pct);
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "W3_arena_copy",
            visible_rows,
            w3_med,
            ns_per_row(w3_med, visible_rows).expect("visible_rows > 0"),
        ),
        w3_min.as_secs_f64() * 1e3
    );
    report_diff("W2", "W3", w2_med, w3_med, visible_rows, ref_band_pct);
    println!(
        "W4_provider_search: matched_rows={} median={:.3}ms (min-of-R={:.3}ms)",
        expected_match_ids.len(),
        w4_med.as_secs_f64() * 1e3,
        w4_min.as_secs_f64() * 1e3
    );
    println!(
        "R_dot_kernel_distance_only (reference band): median={:.3}ms min-of-R={:.3}ms reference_band={:.2}%",
        r_dot_med.as_secs_f64() * 1e3,
        r_dot_min.as_secs_f64() * 1e3,
        ref_band_pct
    );
    println!(
        "e2e(vector_knn_where/W0-cold): median={:.3}ms",
        w0_cold_summary.median.as_secs_f64() * 1e3
    );
    println!(
        "e2e(vector_knn_where/W0-hot): median={:.3}ms",
        w0_hot.summary.median.as_secs_f64() * 1e3
    );
    println!(
        "e2e(vector_knn/W0-nowhere, cache fast path): median={:.3}ms",
        w0_nowhere.summary.median.as_secs_f64() * 1e3
    );
    // 注意: `W0-nowhere` は可視行全体（tenant_a_rows 件）を候補集合として
    // dense 探索するのに対し、`W0-hot` は `lang='ja'` 一致行（約 20%）のみを
    // 候補集合とする。したがってこの raw diff には「SQL 表層内の WHERE 上乗せ」
    // だけでなく、dense 探索（距離計算・Top-k 選出）の候補集合サイズが異なる
    // ことによる処理量差も混入する（W3/W4 が示すとおり候補集合が小さいほど
    // 距離計算・Top-k コストは減る側に働くため、この raw diff を「WHERE 上乗せ」
    // として単純に W3（W1・W2 を含む累積値。W1+W2+W3 のような加算は W1 分の
    // scalar_scan コストを二重計上するため行わない）との比較・残差の帰属に
    // 使うことはできない。候補集合を揃えた比較は W 系列〔`W1`〜`W4`、いずれも一致行のみを対象〕・`R_dot`
    // （全可視行対象の参照区間）側で行う。P2 指摘・詳細は
    // `docs/design/scan-stage-profile.md`「W0-hot と W0-nowhere の候補集合差」
    // 節を参照）。
    let where_overhead_diff = w0_hot.summary.median.checked_sub(w0_nowhere.summary.median);
    match where_overhead_diff {
        Some(d) => println!(
            "diff(W0-nowhere->W0-hot, raw diff; conflates WHERE overhead with reduced dense candidate set size, see docs): {:.3}ms",
            d.as_secs_f64() * 1e3
        ),
        None => println!(
            "diff(W0-nowhere->W0-hot): n/a (独立計測どうしの中央値比較のため測定ノイズにより逆転・未確定)"
        ),
    }

    println!("scan_stage_profile_bench: consistency checks passed (A3 visible row count == tenant_a_rows for both ctx_a/ctx_b every round, W2 match count == expected, W0-cold/W0-hot result ids within lang='ja' visible set, COUNT(*) == tenant_a_rows for both contexts)");
}

fn report_diff(
    from_label: &str,
    to_label: &str,
    from: Duration,
    to: Duration,
    rows: usize,
    reference_band_pct: f64,
) {
    match stage_diff_ns_per_row(from, to, rows, "from", "to") {
        Ok(diff_ns_per_row) => {
            println!("{}", render_diff_line(from_label, to_label, diff_ns_per_row));
            if let Ok(step_pct) = step_ratio_pct(from, to) {
                let band = classify_against_bands(step_pct, reference_band_pct);
                println!(
                    "{}",
                    render_bucket_line(
                        &format!("{from_label}->{to_label}"),
                        diff_ns_per_row,
                        step_pct,
                        band
                    )
                );
            }
        }
        Err(e) => println!(
            "diff({from_label}->{to_label}): n/a (独立計測どうしの中央値比較のため測定ノイズにより逆転・未確定として継続: {e})"
        ),
    }
}

/// `f32` のベクトルを SQL のベクトルリテラル形式へ整形する
/// （`harness::sql_c1::vector_literal` はエラー型を返す `VectorLiteral` を返す
/// ラッパー付き型のため、本ベンチでは `WHERE` 節を含む独自 SQL 文字列組み立てに
/// あわせて薄い直接整形を用いる。値は決定的 RNG 由来の有限値のみのため、
/// 非有限値検証は不要）。
fn vector_literal(values: &[f32]) -> String {
    let parts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
    format!("[{}]", parts.join(","))
}
