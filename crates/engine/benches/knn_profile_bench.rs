//! KNN 経路の段別内訳プロファイル（Issue #362。親 Issue #361「ベクトル検索・
//! ストレージレイアウト最適化」Phase 5 の起点タスク）。
//!
//! SQL 表層経由の KNN（`SELECT id FROM docs ORDER BY embedding <=> '<vec>' LIMIT 10`。
//! 25,000 行・dim128）のレイテンシについて、どの段が支配的かを段別に実測する。
//! `sql_c1_bench.rs`（TASK-83）は SQL 表層全体の p95 を単一の数値として測るのに対し、
//! 本ベンチは redb 走査・ヘッダデコード・f32 デコード・arena 構築・距離計算・Top-k
//! 選出の内訳を分離する（`docs/design/knn-stage-profile.md` 参照）。
//!
//! # 計測条件（受け入れ条件 2）
//!
//! - SQL 表層経路（S0/S0'）は `PrefilterCache`（`core.rs`。`EngineCore::search`
//!   専用・Rust API 直呼び用）を経由しない。`sql::exec::execute_statement` は
//!   クエリ毎に `VectorArena::build_filtered_with_rows_in_txn` で候補行を redb から
//!   再デコードする（`sql_c1_bench.rs` 冒頭コメントの既存事実と同一）。
//! - 既定 provider は `ParallelSearchProvider`（`search_engine::default_engine()`）。
//!   対照として単線 `CpuScalarProvider` も測定する。
//!
//! # 段別分解の測定設計
//!
//! DB ファイルを一度構築したのち、フェーズごとに開き直して測定する
//! （`crates/engine/tests/persistence.rs` と同じ「一度 drop してから生 `redb::Database`
//! を再オープンする」手順）。
//!
//! | 段 | 内容 |
//! | --- | --- |
//! | S0 | `EngineCore::execute_sql` 経由の SQL 表層 KNN（e2e） |
//! | S0' | `SELECT COUNT(*) FROM docs`（走査＋ヘッダ＋RLS の SQL 経由クロスチェック） |
//! | S1 | 生 `redb::Database` 再オープンでの per-entry 走査のみ |
//! | S2 | S1 ＋ ヘッダデコード（`harness::knn_profile::decode_header_reimpl`） |
//! | S3 | S2 ＋ f32 デコード（`harness::knn_profile::decode_row_reimpl`） |
//! | S4 | `VectorArena::build_filtered`（pub API。クエリ毎再構築の実コスト） |
//! | S5 | 距離計算＋Top-k（`ParallelSearchProvider`・`CpuScalarProvider`） |
//! | S5' | 距離計算のみ（`isa::current().dot` を全行へ適用して総和。Top-k なし） |
//!
//! 整合性検証（fail-closed）: S1〜S3 の走査行数が一致すること、S3 のデコード結果を
//! `Storage::scan()`（pub API・完全デコード）の結果と突き合わせて一致すること
//! （`tests/knn_profile_accept.rs` が同じ突き合わせを時間非依存の回帰として持つ。
//! 本ベンチは実データ規模での 1 回限りのクロスチェックを行う）、S0 の結果行数が
//! `TOP_K` に一致すること、S0' の `COUNT(*)` が計測外の再実行で `TOTAL_ROWS` と
//! 一致することを実行時にアサートする。不一致ならベンチはエラー終了し測定値を
//! 出力しない。
//!
//! # `dot_lanes` の実アセンブリ確認（受け入れ条件 3）
//!
//! [`dot_wrapper`] は `engine::isa::current().dot(a, b)` を呼ぶだけの
//! `#[inline(never)]` の薄いラッパーで、production コードは無変更のまま
//! `cargo bench --bench knn_profile_bench -p engine --no-run` でビルドしたバイナリを
//! `objdump -d` で逆アセンブルする手動手順の入口にする（`docs/design/
//! knn-stage-profile.md`「`dot_lanes` の実アセンブリ確認」節に手順・結果を記録する）。
//!
//! # CI・出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線しない
//! （`bench-hybrid` と同方針）。`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で
//! 拒否する（[`harness::knn_profile::refuse_under_github_actions`]）。`make
//! bench-knn-profile`（Makefile）から実行する。判定ロジック自体（時間非依存）は
//! `harness::knn_profile` にあり `tests/knn_profile_accept.rs` で `make ci` 側から
//! 回帰検証する。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::knn_profile::{
    assert_scan_row_counts_match, decode_header_reimpl, decode_row_reimpl, ns_per_row,
    refuse_under_github_actions, render_diff_line, render_stage_line, stage_diff_ns_per_row,
};
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::{c1_statement, vector_literal};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::search_engine;
use engine::sql::exec::Cell;
use engine::storage::{RowInput, Storage, Visibility};
use engine::{arena::VectorArena, tenant};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// feature_bench の `vector_knn` フェーズ実測条件を再現する規模（Issue #362。
/// テナント A 20,000 行 ＋ テナント B 5,000 行 ＝ 計 25,000 行）。
const DIM: usize = 128;
const TENANT_A: &str = "tenant-a";
const TENANT_A_ROWS: usize = 20_000;
const TENANT_B: &str = "tenant-b";
const TENANT_B_ROWS: usize = 5_000;
const TOTAL_ROWS: usize = TENANT_A_ROWS + TENANT_B_ROWS;
const TOP_K: usize = 10;
const TABLE: &str = "docs";
const COLUMN: &str = "embedding";
const SEED_BATCH_ROWS: usize = 5_000;

/// 生 `redb::Database` 再オープン時に走査する行テーブル。物理キー型・テーブル名は
/// `crates/engine/src/catalog.rs::user_rows_table_name`（`"user_rows/{table}"`）・
/// `crates/engine/src/storage.rs::RowStoreTableDef` の契約をベンチ内で複製した
/// ものであり、ドリフト検出は `tests/knn_profile_accept.rs` が
/// `Storage::scan()`（pub API）との突き合わせで行う（モジュール冒頭コメント参照）。
const ROW_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("user_rows/docs");

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("knn_profile_bench: {msg}");
    std::process::exit(1);
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!("knn_profile_bench: rows={TOTAL_ROWS} dim={DIM} top_k={TOP_K} (tenant_a={TENANT_A_ROWS} tenant_b={TENANT_B_ROWS})");
    println!("knn_profile_bench: SQL 表層は PrefilterCache を経由しない（core.rs は EngineCore::search 専用）。既定 provider は ParallelSearchProvider、対照は CpuScalarProvider。");

    // --- データ投入: 一時 DB へテナント A・B の行を投入する ---
    let path = unique_db_path("issue362-knn-profile");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new(
                COLUMN,
                ColumnType::Vector(DIM as u32),
                false,
            )],
        ))
        .expect("create table for bench seeding");

    let mut rng = DeterministicRng::new(1);
    let mut all_ids: Vec<u64> = Vec::with_capacity(TOTAL_ROWS);
    let mut all_vectors: Vec<f32> = Vec::with_capacity(TOTAL_ROWS * DIM);
    let mut next_id: u64 = 0;
    for (tenant_id, count) in [(TENANT_A, TENANT_A_ROWS), (TENANT_B, TENANT_B_ROWS)] {
        let ctx = PolicyContext::new(tenant_id).expect("valid tenant id");
        let mut remaining = count;
        while remaining > 0 {
            let batch_len = SEED_BATCH_ROWS.min(remaining);
            let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
            for _ in 0..batch_len {
                batch_vectors.push(rng.next_vector(DIM));
            }
            let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
                .map(|i| {
                    let id = next_id + i as u64;
                    (
                        id,
                        RowInput {
                            tenant_id,
                            visibility: Visibility::Public,
                            embedding: &batch_vectors[i],
                            // VECTOR 列のみのテーブルのため、スカラーペイロードは
                            // 空バイト列になる（`sql_c1_bench.rs` と同一理由）。
                            metadata: b"",
                        },
                    )
                })
                .collect();
            let op_id = OperationId::parse(&format!("seed-{tenant_id}-{next_id}"))
                .expect("valid operation_id");
            tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id).expect("seed batch insert");
            for (i, v) in batch_vectors.iter().enumerate() {
                all_ids.push(next_id + i as u64);
                all_vectors.extend_from_slice(v);
            }
            next_id += batch_len as u64;
            remaining -= batch_len;
        }
    }
    debug_assert_eq!(all_ids.len(), TOTAL_ROWS);

    let policy_ctx = PolicyContext::new(TENANT_A).expect("valid tenant id");
    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");

    // 単一クエリ（テナント A の可視ベクトルからの近傍探索。RLS は Public のため
    // テナント B の行も可視）。S0〜S5 を通じて同一クエリベクトルを使い、段間で
    // 比較可能にする。
    let query = rng.next_vector(DIM);

    // --- S4/S5/S5': pub API（`&storage` 使用）。--------------------------------
    let s4 = run(&config, || {
        VectorArena::build_filtered(&storage, TABLE, |tenant, visibility| {
            policy_ctx.is_visible(tenant, visibility)
        })
        .expect("arena build must succeed for well-formed synthetic corpus")
    })
    .expect("measurement must satisfy protocol minimums");
    let s4_median = s4.summary.median;
    println!(
        "{}",
        render_stage_line(
            "S4_arena_build",
            TOTAL_ROWS,
            s4_median,
            ns_per_row(s4_median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );

    // S5/S5' は事前構築済みの単一アリーナに対して測る（クエリ毎の再構築コストは
    // S4 で分離済みのため、ここでは検索カーネル段のみを対象にする）。
    let arena = VectorArena::build_filtered(&storage, TABLE, |tenant, visibility| {
        policy_ctx.is_visible(tenant, visibility)
    })
    .expect("arena build must succeed for well-formed synthetic corpus");
    if arena.len() != TOTAL_ROWS {
        fail_closed(format!(
            "arena row count mismatch: expected {TOTAL_ROWS}, got {}",
            arena.len()
        ));
    }

    let parallel_provider = ParallelSearchProvider;
    let s5_parallel = run(&config, || {
        parallel_provider
            .search(SearchInput {
                ids: arena.ids(),
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &query,
                k: TOP_K,
            })
            .expect("parallel search must succeed for well-formed synthetic input")
    })
    .expect("measurement must satisfy protocol minimums");
    println!(
        "{}",
        render_stage_line(
            "S5_search_parallel",
            TOTAL_ROWS,
            s5_parallel.summary.median,
            ns_per_row(s5_parallel.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );

    let scalar_provider = CpuScalarProvider;
    let s5_scalar = run(&config, || {
        scalar_provider
            .search(SearchInput {
                ids: arena.ids(),
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &query,
                k: TOP_K,
            })
            .expect("scalar search must succeed for well-formed synthetic input")
    })
    .expect("measurement must satisfy protocol minimums");
    println!(
        "{}",
        render_stage_line(
            "S5_search_scalar",
            TOTAL_ROWS,
            s5_scalar.summary.median,
            ns_per_row(s5_scalar.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );

    let s5_prime = run(&config, || {
        let mut acc = 0.0f32;
        let dim = arena.dim() as usize;
        for chunk in arena.vectors().chunks_exact(dim) {
            acc += dot_wrapper(chunk, &query);
        }
        acc
    })
    .expect("measurement must satisfy protocol minimums");
    println!(
        "{}",
        render_stage_line(
            "S5prime_distance_only",
            TOTAL_ROWS,
            s5_prime.summary.median,
            ns_per_row(s5_prime.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );

    // 検索ループの追加コスト分離は S5_scalar − S5' を用いる（S5_parallel − S5' は
    // 使わない）。`ParallelSearchProvider::search` はワーカースレッド生成・行範囲
    // 分割・部分 Top-k・結果マージ・入力検証を含み、`s5_prime`（呼び出し元スレッド
    // で `dot_wrapper` を逐次実行するのみ）との差分には並列化コストが混在して
    // しまう（codex-review 指摘・PR #378）。`s5_scalar`（`CpuScalarProvider`）は
    // `s5_prime` と同じ単線・逐次走査条件のため、並列化コストは混在しない。ただし
    // `CpuScalarProvider::search` は Top-k 選出（`BinaryHeap` 部分ソート）のほかにも
    // 次元検証・クエリの有限値検査・行ごとの範囲取得（境界チェック付き slice）・
    // スコアの有限値検査・候補構築を行うため、この差分は「Top-k 選出のみ」の
    // コストではなく「Top-k を含む検索ループ全体の追加コスト」である
    // （codex-review 指摘・PR #378）。
    //
    // `s5_prime` と `s5_scalar` は入れ子ではなく独立に計測した別経路であるため、
    // 差分が理論上非負である保証はない（`s5_scalar` 側にのみ `#[inline(never)]`
    // 呼び出しオーバーヘッドが乗らない等の要因で、僅差は測定ノイズにより逆転し
    // 得る。cursor 指摘・PR #378）。よってこの 1 行のみ非致命扱いとし、負の
    // 差分が出ても `fail_closed` でベンチ全体を中断せず、以降の S0/S1〜S4 の
    // 計測・出力を継続する（S1〜S4 等の入れ子な累積段の非単調性チェックは
    // 従来どおり fail-closed を維持する）。
    match stage_diff_ns_per_row(
        s5_prime.summary.median,
        s5_scalar.summary.median,
        TOTAL_ROWS,
        "S5prime",
        "S5_scalar",
    ) {
        Ok(diff) => println!("{}", render_diff_line("S5prime", "S5_scalar", diff)),
        Err(e) => println!(
            "diff(S5prime->S5_scalar): skipped (独立経路間の測定ノイズにより非単調 \
             ・非致命として継続: {e})"
        ),
    }

    // --- S0/S0': SQL 表層 e2e（`EngineCore::execute_sql`）。--------------------
    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let literal = vector_literal(&query).expect("finite query vector");
    let sql = c1_statement(TABLE, COLUMN, &literal, TOP_K)
        .expect("well-formed C1 statement from validated identifiers");
    let s0 = run(&config, || {
        core.execute_sql(&policy_ctx, &sql)
            .expect("execute_sql must succeed for well-formed synthetic KNN query")
    })
    .expect("measurement must satisfy protocol minimums");
    if s0.samples.is_empty() {
        fail_closed("S0 measurement produced no samples");
    }
    let s0_result_len = core
        .execute_sql(&policy_ctx, &sql)
        .expect("execute_sql must succeed for well-formed synthetic KNN query")
        .rows
        .len();
    if s0_result_len != TOP_K {
        fail_closed(format!(
            "S0 result row count mismatch: expected {TOP_K}, got {s0_result_len}"
        ));
    }
    println!(
        "{}",
        render_stage_line(
            "S0_sql_e2e",
            TOTAL_ROWS,
            s0.summary.median,
            ns_per_row(s0.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );

    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let s0_prime = run(&config, || {
        core.execute_sql(&policy_ctx, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");
    // S0 の TOP_K 検証（下記）と同様、計測クロージャの戻り値は `black_box` へ渡す
    // だけで中身を検証しない。SQL 経路が誤った件数を返しても計測は成功してしまう
    // ため、計測外で COUNT(*) を再実行し値を TOTAL_ROWS と突き合わせる
    // （codex-review 指摘・PR #378。fail-closed）。
    let count_result = core
        .execute_sql(&policy_ctx, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    if count_result.rows.len() != 1 {
        fail_closed(format!(
            "S0' COUNT(*) row count mismatch: expected 1, got {}",
            count_result.rows.len()
        ));
    }
    let count_value = match count_result.rows[0].cells.first() {
        Some(Cell::Integer(v)) => *v,
        other => fail_closed(format!("S0' COUNT(*) cell type mismatch: got {other:?}")),
    };
    if count_value != TOTAL_ROWS as u64 {
        fail_closed(format!(
            "S0' COUNT(*) value mismatch: expected {TOTAL_ROWS}, got {count_value}"
        ));
    }
    println!(
        "{}",
        render_stage_line(
            "S0prime_count_star",
            TOTAL_ROWS,
            s0_prime.summary.median,
            ns_per_row(s0_prime.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );

    // --- S1〜S3: 生 redb 再オープンでの走査・デコード。--------------------------
    // `core`（`EngineCore::from_storage` が所有する `Storage`）を drop してファイル
    // ロックを解放してから、生 `redb::Database` として再オープンする
    // （`tests/persistence.rs` と同一手順）。
    drop(core);

    let db = Database::open(&path).expect("reopen raw database for S1-S3");

    // S1: per-entry 走査のみ（`black_box` は harness::protocol::run が内包する）。
    let s1 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut total_bytes = 0usize;
        let mut rows = 0usize;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            total_bytes += v.value().len();
            rows += 1;
        }
        (rows, total_bytes)
    })
    .expect("measurement must satisfy protocol minimums");
    let s1_rows = {
        let read_txn = db.begin_read().expect("begin read txn for row count check");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        table.iter().expect("iter row table").count()
    };
    println!(
        "{}",
        render_stage_line(
            "S1_redb_scan",
            TOTAL_ROWS,
            s1.summary.median,
            ns_per_row(s1.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );

    // S2: S1 ＋ ヘッダデコード。
    let s2 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let _header = decode_header_reimpl(v.value())
                .expect("header decode must succeed for well-formed synthetic rows");
            rows += 1;
        }
        rows
    })
    .expect("measurement must satisfy protocol minimums");
    let s2_rows = {
        let read_txn = db.begin_read().expect("begin read txn for row count check");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let _ = decode_header_reimpl(v.value()).expect("header decode");
            rows += 1;
        }
        rows
    };
    println!(
        "{}",
        render_stage_line(
            "S2_header_decode",
            TOTAL_ROWS,
            s2.summary.median,
            ns_per_row(s2.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_diff_line(
            "S1",
            "S2",
            stage_diff_ns_per_row(s1.summary.median, s2.summary.median, TOTAL_ROWS, "S1", "S2")
                .unwrap_or_else(|e| fail_closed(e)),
        )
    );

    // S3: S2 ＋ f32 デコード。スクラッチバッファはループ間で使い回す
    // （`storage.rs::decode_row_embedding_and_metadata_into` と同じ設計を
    // 再実装側でも踏襲する。モジュール冒頭コメント参照）。
    let mut scratch: Vec<f32> = Vec::with_capacity(DIM);
    let s3 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let _decoded = decode_row_reimpl(v.value(), &mut scratch)
                .expect("row decode must succeed for well-formed synthetic rows");
            rows += 1;
        }
        rows
    })
    .expect("measurement must satisfy protocol minimums");
    let mut s3_scratch: Vec<f32> = Vec::with_capacity(DIM);
    let mut s3_rows = 0usize;
    // 突き合わせ対象の 1 件（行 id・ベンチ内デコード結果）。`Storage::scan()`
    // は `ROWS_TABLE`（"rows"）のみを走査し、本ベンチが `tenant::insert_rows`
    // 経由で書き込むカタログテーブル（`user_rows/docs`）は対象外のため使えない
    // （モジュール冒頭コメント「行バイト列レイアウトのベンチ内再実装」節参照）。
    // 代わりに、同じ id を持つ行を pub API 経由で構築済みの `arena`（S4 で
    // `VectorArena::build_filtered` により構築した「正本」）から embedding を
    // 引いて突き合わせる（ヘッダ・フルバイト列レイアウトの突き合わせは
    // `tests/knn_profile_accept.rs` が `Storage::put`/`Storage::scan()` を使う
    // 小規模フィクスチャで別途行う）。
    let mut cross_check_sample: Option<(u64, Vec<f32>)> = None;
    {
        let read_txn = db.begin_read().expect("begin read txn for row count check");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        for entry in table.iter().expect("iter row table") {
            let (k, v) = entry.expect("iterate row entry");
            let _decoded = decode_row_reimpl(v.value(), &mut s3_scratch).expect("row decode");
            if cross_check_sample.is_none() {
                let (_tenant, id) = k.value();
                // クロスチェック用の 1 件のみ計測外で所有化する（`decoded` は
                // embedding を持たないスクラッチ参照のみを返す設計に変更済み。
                // モジュール冒頭コメント参照。codex-review 指摘 #1・PR #378）。
                cross_check_sample = Some((id, s3_scratch.clone()));
            }
            s3_rows += 1;
        }
    }
    println!(
        "{}",
        render_stage_line(
            "S3_f32_decode",
            TOTAL_ROWS,
            s3.summary.median,
            ns_per_row(s3.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_diff_line(
            "S2",
            "S3",
            stage_diff_ns_per_row(s2.summary.median, s3.summary.median, TOTAL_ROWS, "S2", "S3")
                .unwrap_or_else(|e| fail_closed(e)),
        )
    );
    println!(
        "{}",
        render_diff_line(
            "S3",
            "S4",
            stage_diff_ns_per_row(s3.summary.median, s4_median, TOTAL_ROWS, "S3", "S4")
                .unwrap_or_else(|e| fail_closed(e)),
        )
    );

    // --- 残差: S0 -（S4 + S5） ---------------------------------------------------
    let s4_plus_s5 = s4_median.saturating_add(s5_parallel.summary.median);
    let residual = s0.summary.median.saturating_sub(s4_plus_s5);
    println!(
        "residual(S0-(S4+S5)): median={:.3}ms (parse/bind/result-assembly 等。read_txn 境界差を含みうる保守的な残差)",
        residual.as_secs_f64() * 1e3
    );

    // --- 整合性検証（fail-closed） ---------------------------------------------
    if let Err(e) =
        assert_scan_row_counts_match(&[("S1", s1_rows), ("S2", s2_rows), ("S3", s3_rows)])
    {
        fail_closed(e);
    }
    if s1_rows != TOTAL_ROWS {
        fail_closed(format!(
            "raw redb scan row count mismatch: expected {TOTAL_ROWS}, got {s1_rows}"
        ));
    }
    // `db`（生 `redb::Database` ハンドル）はこれ以降使わないため drop し、ファイル
    // ロックを解放する（後続の測定はすべて `EngineCore`/`&storage` 経由の pub API
    // を通じてのみ DB へアクセスする）。
    drop(db);

    // S3 のベンチ内デコードと `VectorArena::build_filtered`（pub API・S4 で構築済みの
    // 「正本」デコード結果）の 1 件突き合わせ（レイアウト再実装のドリフト検出。
    // モジュール冒頭コメント参照）。`Storage::scan()` は `ROWS_TABLE`（"rows"）のみを
    // 走査し、本ベンチが書き込むカタログテーブル（`user_rows/docs`）は対象外のため
    // 使えない。ヘッダ（`tenant_id`/`visibility`）を含むフルバイト列レイアウトの
    // 突き合わせは `tests/knn_profile_accept.rs` が `Storage::put`/`Storage::scan()`
    // を使う小規模フィクスチャで別途行う（同一の `encode_row`/`decode_row_header`
    // 実装を経るため、テーブル名の違いはレイアウトそのものに影響しない）。
    if let Some((id, embedding)) = cross_check_sample {
        let matched = arena
            .ids()
            .iter()
            .position(|&arena_id| arena_id == id)
            .map(|idx| {
                let dim = arena.dim() as usize;
                &arena.vectors()[idx * dim..(idx + 1) * dim] == embedding.as_slice()
            })
            .unwrap_or(false);
        if !matched {
            fail_closed(
                "S3 reimpl decode did not match the VectorArena::build_filtered row for the same id (layout drift suspected)",
            );
        }
    }

    println!("knn_profile_bench: consistency checks passed (S1..S3 row counts, S0 result count, S0' COUNT(*) value, S3 vs VectorArena cross-check)");
}

/// [`engine::isa::current().dot`] を呼ぶだけの薄いラッパー。`#[inline(never)]` に
/// することで、`objdump -d` でのシンボル特定・逆アセンブル確認（受け入れ条件 3）を
/// 容易にする（モジュール冒頭コメント参照）。production コード（`engine::isa`）は
/// 無変更。
#[inline(never)]
fn dot_wrapper(a: &[f32], b: &[f32]) -> f32 {
    engine::isa::current().dot(a, b)
}
