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
//!   専用・Rust API 直呼び用）を経由しない。ただし `sql::exec::execute_statement`
//!   の `VectorArena` はテーブル単位世代整合キャッシュ（`sql/arena_cache.rs::
//!   SqlArenaCache`。Issue #363）を経由するため、「クエリ毎に候補行を redb から
//!   再デコードする e2e コスト」は S0-cold（毎サンプル新規 `EngineCore` で
//!   `SqlArenaCache` を空の状態から測る）でのみ表れる。S0-hot は同一
//!   `EngineCore` を使い回し `SqlArenaCache` ヒット時のオーバーヘッドを測る
//!   （下記「`SqlArenaCache` と S0 の cold/hot 分離」節参照）。
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
//! | S0-cold | `EngineCore::execute_sql` 経由の SQL 表層 KNN（e2e。サンプルごとに
//! 新規 `Storage::open` + `EngineCore::from_storage` で `SqlArenaCache`〔Issue #363〕
//! を空の状態から測る。下記「`SqlArenaCache` と S0 の cold/hot 分離」節参照） |
//! | S0-hot | S0-cold と同じクエリを単一 `EngineCore` へ 40 回以上繰り返し、初回
//! 以降は `SqlArenaCache` がヒットする状態（キャッシュヒット時のオーバーヘッド） |
//! | S0' | `SELECT COUNT(*) FROM docs`（走査＋ヘッダ＋RLS の SQL 経由クロスチェック。
//! 集計クエリのため `SqlArenaCache`〔`VectorArena` 専用〕の対象外） |
//! | S1 | 生 `redb::Database` 再オープンでの per-entry 走査のみ |
//! | S2 | S1 ＋ ヘッダデコード（`harness::knn_profile::decode_header_reimpl`） |
//! | S3 | S2 ＋ f32 デコード（`harness::knn_profile::decode_row_reimpl`） |
//! | S4 | `VectorArena::build_filtered`（pub API。クエリ毎再構築の実コスト） |
//! | S5 | 距離計算＋Top-k（`ParallelSearchProvider`・`CpuScalarProvider`） |
//! | S5' | 距離計算のみ（`isa::current().dot` を全行へ適用して総和。Top-k なし） |
//!
//! ## `SqlArenaCache` と S0 の cold/hot 分離
//!
//! Issue #363 で SQL 表層 `SELECT`（`USING PLAN` 展開経由を含む）の `VectorArena` が
//! テーブル単位世代整合キャッシュ（`sql/arena_cache.rs::SqlArenaCache`）を経由する
//! ようになったため、同一 `EngineCore` へ同一クエリを繰り返すだけでは初回以降
//! ほぼ全呼び出しがキャッシュヒットになり、「クエリ毎に候補行を redb から
//! 再デコードする e2e コスト」を S0 が表さなくなる（codex-review P1-1 指摘・
//! PR #378）。`SqlArenaCache`／`EngineCore` に既存の invalidate/clear 相当の
//! 公開 API は無く、世代を進めるダミー DML はコーパスのデータを変えてしまうため
//! 使わない。代わりに S0-cold は、warmup・計測フェーズの各サンプルごとに計測
//! 区間の外側（`Instant::now()` の前）で新規 `Storage::open` + `EngineCore::
//! from_storage` を構築し、常に空の `SqlArenaCache` から `execute_sql` を計測する
//! （`harness::protocol::run`/`run_bounded_retain` は setup を計測区間の外側に
//! 置けない〔クロージャ 1 本に setup と計測を束ねる契約〕ため、本段のみ独自の
//! warmup/計測ループを持ち `harness::stats::summarize` で統計手順を揃える）。
//! S0-hot は従来どおり単一 `EngineCore` を使い回し、キャッシュヒット時の
//! オーバーヘッドを別段として測る。
//!
//! 整合性検証（fail-closed）: S1〜S3 の走査行数が一致すること、S3 のデコード結果を
//! `Storage::scan()`（pub API・完全デコード）の結果と突き合わせて一致すること
//! （`tests/knn_profile_accept.rs` が同じ突き合わせを時間非依存の回帰として持つ。
//! 本ベンチは実データ規模での 1 回限りのクロスチェックを行う）、S0-cold・S0-hot
//! それぞれの結果行数が `TOP_K` に一致すること、S0' の `COUNT(*)` が計測外の
//! 再実行で `TOTAL_ROWS` と一致することを実行時にアサートする。不一致ならベンチは
//! エラー終了し測定値を出力しない。
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
    KnnProfileError,
};
use harness::protocol::{run, run_bounded_retain, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::{c1_statement, vector_literal};
use harness::stats;

use std::hint::black_box;
use std::time::{Duration, Instant};

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

/// S0-cold／S0-hot が使う `EngineCore` を `knn_engine`（`BENCH_KNN_PROFILE_ENGINE`。
/// Issue #413）に応じて構築する。S1〜S5' は生 redb 走査・provider 直呼び等の
/// エンジン非依存経路のため本関数を使わない。
fn build_core_for(knn_engine: harness::bench_engine::BenchEngine, storage: Storage) -> EngineCore {
    match knn_engine {
        harness::bench_engine::BenchEngine::BruteForce => {
            EngineCore::from_storage(storage, search_engine::default_engine())
        }
        harness::bench_engine::BenchEngine::Hnsw => {
            let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid HnswParams::default()");
            EngineCore::from_storage_with_engine(storage, kind)
        }
    }
}

/// 段間差分（[`stage_diff_ns_per_row`]）の結果を出力する。S1〜S4・S5prime-S5_scalar
/// はいずれも段ごとに独立した `run` 呼び出し（別トランザクション・別 warmup/
/// サンプル列）による中央値どうしの比較であり、測定ノイズにより逆転しうる
/// （`Err(KnnProfileError::NonMonotonicStages)`）。この逆転は行数不一致・レイアウト
/// ドリフト等の整合性検証（`fail_closed` で維持）とは性質が異なるため、ベンチを
/// 失敗させず「未確定（n/a）」として出力を継続する（codex-review P2-2 指摘・
/// PR #378）。
fn print_stage_diff_or_unconfirmed(from: &str, to: &str, diff: Result<f64, KnnProfileError>) {
    match diff {
        Ok(value) => println!("{}", render_diff_line(from, to, value)),
        Err(e) => println!(
            "diff({from}->{to}): n/a (独立計測どうしの中央値比較のため測定ノイズにより \
             逆転・未確定として継続: {e})"
        ),
    }
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    // ANN opt-in（Issue #413）。`BENCH_KNN_PROFILE_ENGINE` は S0-cold／S0-hot
    // （SQL 表層 e2e）の `EngineCore` 構築にのみ効く。S1〜S5' は構造的にエンジン
    // 非依存（生 redb 走査・`VectorArena::build_filtered`・provider 直呼び）の
    // ため変更しない（S5 を hnsw として提示しない。モジュール冒頭コメント参照）。
    let knn_engine = match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_ENGINE")
        .and_then(|raw| harness::bench_engine::parse_engine(raw.as_deref()))
    {
        Ok(e) => e,
        Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_ENGINE: {e}")),
    };

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!("knn_profile_bench: rows={TOTAL_ROWS} dim={DIM} top_k={TOP_K} (tenant_a={TENANT_A_ROWS} tenant_b={TENANT_B_ROWS}) engine={}", knn_engine.token());
    println!("knn_profile_bench: SQL 表層は PrefilterCache を経由しない（core.rs は EngineCore::search 専用）が SqlArenaCache（Issue #363）は経由する。S0-cold は毎サンプル新規 EngineCore で SqlArenaCache を空の状態から測る。既定 provider は ParallelSearchProvider、対照は CpuScalarProvider。BENCH_KNN_PROFILE_ENGINE=hnsw のときは S0-cold/S0-hot の EngineCore を hnsw opt-in（Issue #403 B 案）で構築する（S1〜S5' は非対象）。");

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
            next_id += batch_len as u64;
            remaining -= batch_len;
        }
    }
    // 投入件数の検証は `debug_assert!` ではなく明示チェックにする。ベンチは
    // release ビルドで動かすため `debug_assert!` は既定でコンパイルから除去され、
    // 検査が事実上無効化される（codex-review 指摘・PR #378）。投入済みベクトルを
    // 別バッファへ複製して件数検証する必要はない（`next_id` の到達値で足りる）。
    if next_id as usize != TOTAL_ROWS {
        fail_closed(format!(
            "seeded row count mismatch: expected {TOTAL_ROWS}, got {next_id}"
        ));
    }

    let policy_ctx = PolicyContext::new(TENANT_A).expect("valid tenant id");
    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");

    // 単一クエリ（テナント A の可視ベクトルからの近傍探索。RLS は Public のため
    // テナント B の行も可視）。S0〜S5 を通じて同一クエリベクトルを使い、段間で
    // 比較可能にする。
    let query = rng.next_vector(DIM);

    // --- S4/S5/S5': pub API（`&storage` 使用）。--------------------------------
    // S4 は「arena 構築の実コスト」のみを対象とする。`harness::protocol::run` は
    // 戻り値を `black_box` 通過後、計測区間の内側で drop する契約
    // （`protocol.rs` モジュールコメント・Issue #302）のため、クロージャが
    // `VectorArena`（ヒープ確保を伴う）をそのまま返すと解放コストが構築コストへ
    // 混入する（codex-review 指摘・PR #378）。`run_bounded_retain`
    // （`retain_capacity = 0`）を使い、`elapsed()` 取得後・計測区間の外側で
    // 毎回即座に drop することで、同時に生存する `VectorArena` を常に 0 件に
    // 保ちつつ解放コストも計測区間から除く（codex-review 指摘・PR #378 追記。
    // `mem::replace` によるリングバッファ入れ替えは入れ替え自体が計測区間の内側で
    // 発生していたため、解放コスト除外の要求を満たしていなかった）。
    let (s4, _retained) = run_bounded_retain(&config, 0, || {
        VectorArena::build_filtered(&storage, TABLE, |tenant, visibility| {
            policy_ctx.is_visible(tenant, visibility)
        })
        .expect("arena build must succeed for well-formed synthetic corpus")
    })
    .expect("measurement must satisfy protocol minimums");
    let s4_median = s4.summary.median;

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

    let s5_prime = run(&config, || {
        let mut acc = 0.0f32;
        let dim = arena.dim() as usize;
        for chunk in arena.vectors().chunks_exact(dim) {
            acc += dot_wrapper(chunk, &query);
        }
        acc
    })
    .expect("measurement must satisfy protocol minimums");

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
    // 得る。cursor 指摘・PR #378）。よってこの差分のみ非致命扱いとし、負の
    // 差分が出ても `fail_closed` でベンチ全体を中断せず、以降の S0/S1〜S4 の
    // 計測を継続する（S1〜S4 等の入れ子な累積段の非単調性チェックは従来どおり
    // fail-closed を維持する）。出力自体も他の段と同様、全計測・整合性検証が
    // 完了してからまとめて行う（codex-review 指摘・PR #378。fail-closed 契約に
    // 反して整合性検証前に測定値を出力しないため、ここでは結果を変数へ保持する
    // のみで println! しない）。
    let s5prime_vs_scalar_diff = stage_diff_ns_per_row(
        s5_prime.summary.median,
        s5_scalar.summary.median,
        TOTAL_ROWS,
        "S5prime",
        "S5_scalar",
    );

    let literal = vector_literal(&query).expect("finite query vector");
    let sql = c1_statement(TABLE, COLUMN, &literal, TOP_K)
        .expect("well-formed C1 statement from validated identifiers");

    // `storage`（S4/S5 で使い終えた元ハンドル）を明示的に drop し、redb のファイル
    // ロックを解放する。redb は書き込み可能ハンドルを同時に複数開けない契約
    // （`redb::DatabaseError::DatabaseAlreadyOpen`）のため、直後の S0-cold 測定が
    // 毎回開く一時ハンドルと同時生存させられない（P1-1 是正・PR #378
    // codex-review 指摘）。
    drop(storage);

    // --- S0-cold: SqlArenaCache を毎回空の状態から測る e2e 計測。----------------
    // モジュール冒頭コメント「`SqlArenaCache` と S0 の cold/hot 分離」節参照。
    // `harness::protocol::run` 系は setup（DB オープン）と計測を単一クロージャに
    // 束ねる契約のため使えず、本段のみ独自の warmup/計測ループを持つ。
    for _ in 0..config.warmup_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for S0-cold warmup");
        let cold_core = build_core_for(knn_engine, cold_storage);
        black_box(
            cold_core
                .execute_sql(&policy_ctx, &sql)
                .expect("execute_sql must succeed for well-formed synthetic KNN query"),
        );
    }
    let mut s0_cold_samples: Vec<Duration> =
        Vec::with_capacity(config.measured_iterations() as usize);
    let mut s0_cold_last_result_len: Option<usize> = None;
    for _ in 0..config.measured_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for S0-cold measurement");
        let cold_core = build_core_for(knn_engine, cold_storage);
        let start = Instant::now();
        let result = black_box(
            cold_core
                .execute_sql(&policy_ctx, &sql)
                .expect("execute_sql must succeed for well-formed synthetic KNN query"),
        );
        let elapsed = start.elapsed();
        s0_cold_samples.push(elapsed);
        // 計測区間の外側でのみ結果行数を観測する（`run`/`run_bounded_retain` と
        // 同じく、計測区間そのものへは戻り値の検証を混ぜない）。
        s0_cold_last_result_len = Some(result.rows.len());
    }
    if s0_cold_samples.is_empty() {
        fail_closed("S0-cold measurement produced no samples");
    }
    if s0_cold_last_result_len != Some(TOP_K) {
        fail_closed(format!(
            "S0-cold result row count mismatch: expected {TOP_K}, got {s0_cold_last_result_len:?}"
        ));
    }
    let s0_cold_summary =
        stats::summarize(&s0_cold_samples).expect("S0-cold summarize must succeed");

    // --- S0-hot/S0': SQL 表層 e2e（単一 `EngineCore` を使い回すホットパス）。----
    let storage = Storage::open(&path).expect("reopen storage for S0-hot/S0'");
    let core = build_core_for(knn_engine, storage);
    let s0_hot = run(&config, || {
        core.execute_sql(&policy_ctx, &sql)
            .expect("execute_sql must succeed for well-formed synthetic KNN query")
    })
    .expect("measurement must satisfy protocol minimums");
    if s0_hot.samples.is_empty() {
        fail_closed("S0-hot measurement produced no samples");
    }
    let s0_hot_result_len = core
        .execute_sql(&policy_ctx, &sql)
        .expect("execute_sql must succeed for well-formed synthetic KNN query")
        .rows
        .len();
    if s0_hot_result_len != TOP_K {
        fail_closed(format!(
            "S0-hot result row count mismatch: expected {TOP_K}, got {s0_hot_result_len}"
        ));
    }

    // 非 vacuous 確認（Issue #413。`feature_bench.rs` と同じ原則）。hnsw opt-in
    // 時は S0-hot 測定後に索引が実際に構築・使用されたことを固定し、満たさなけ
    // れば `fail_closed` する。brute_force では統計を出力しない
    // （`hnsw_index_cache_stats()` は常に全欄 0）。
    if matches!(knn_engine, harness::bench_engine::BenchEngine::Hnsw) {
        let s = core.hnsw_index_cache_stats();
        println!(
            "knn_profile_bench: hnsw_stats builds={} build_failures={} hits={} misses={} fallbacks={} entries={}",
            s.builds, s.build_failures, s.hits, s.misses, s.fallbacks, s.entries
        );
        if s.builds == 0 || s.hits == 0 {
            fail_closed(format!(
                "ANN non-vacuous check failed: builds={} hits={}",
                s.builds, s.hits
            ));
        }
    }

    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let s0_prime = run(&config, || {
        core.execute_sql(&policy_ctx, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");
    // S0-hot の TOP_K 検証（上記）と同様、計測クロージャの戻り値は `black_box` へ
    // 渡すだけで中身を検証しない。SQL 経路が誤った件数を返しても計測は成功して
    // しまうため、計測外で COUNT(*) を再実行し値を TOTAL_ROWS と突き合わせる
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

    // --- S1〜S3: 生 redb 再オープンでの走査・デコード。--------------------------
    // `core`（`EngineCore::from_storage` が所有する `Storage`）を drop してファイル
    // ロックを解放してから、生 `redb::Database` として再オープンする
    // （`tests/persistence.rs` と同一手順）。
    drop(core);

    let db = Database::open(&path).expect("reopen raw database for S1-S3");

    // S1: per-entry 走査のみ（`black_box` は harness::protocol::run が内包する）。
    // 段間差分（S2 - S1 等）を正しく「ヘッダデコード等の追加コストのみ」に
    // 保つため、S1 のループ本体は S2/S3 と同じ「行数を数えるだけ」に揃える
    // （バイト数集計等 S1 固有の処理を追加しない。S1 ⊆ S2 ⊆ S3 の入れ子を
    // 崩すと差分が歪む。codex-review 指摘・PR #378）。
    //
    // S1〜S3 は全段で「同じ型（u64）の累積器へ、行あたり同じ回数（2 回）だけ
    // `std::hint::black_box` 経由で `wrapping_add` する」という同形・同量の
    // 観測処理を持つ（S1 は行データを読まないため加算対象は定数、S2 はヘッダ
    // 由来値、S3 は f32 由来値のビット列。観測処理自体の演算コストを全段で
    // 揃えることで、S2−S1／S3−S2 の差分にヘッダデコード・f32 デコード以外の
    // コスト〔checksum 演算の非対称性〕が混入しないようにする。
    // codex-review P2-1 指摘・PR #378）。
    let s1 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let _entry = entry.expect("iterate row entry");
            // S2/S3 と同形の観測処理（モジュール冒頭コメント・上記コメント参照）。
            // S1 は行バイト列を読まないため加算対象は定数だが、S2/S3 と同じ
            // 回数・同じ型の演算を `black_box` 経由で行い、段間差分から
            // checksum 演算コスト自体を相殺する。
            checksum = checksum.wrapping_add(std::hint::black_box(1u64));
            checksum = checksum.wrapping_add(std::hint::black_box(1u64));
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");
    let s1_rows = {
        let read_txn = db.begin_read().expect("begin read txn for row count check");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        table.iter().expect("iter row table").count()
    };

    // S2: S1 ＋ ヘッダデコード。ヘッダデコード結果（`tenant_id`・`is_public`）は
    // 単に破棄せず、行数と組み合わせた値依存の checksum へ混入して返す（`tenant_id`
    // の長さ・`is_public` フラグはいずれもデコード結果に実際に依存する軽量な値の
    // ため、release 最適化でヘッダデコード自体が未観測として除去されるのを防ぐ。
    // codex-review P1-2 指摘・PR #378。S3 と同種の是正）。
    //
    // S1 と同形の観測処理（上記「S1〜S3 は全段で…」コメント参照）: 行あたり
    // 常に 2 回、`black_box` 経由で `u64` 累積器へ加算する。分岐（`if is_public`）
    // による加算回数の増減を避けるため、`is_public` は `bool as u64`（0/1）へ
    // 変換してから無条件に加算する（codex-review P2-1 指摘・PR #378）。
    let s2 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let (tenant_id, is_public, _offset) = decode_header_reimpl(v.value())
                .expect("header decode must succeed for well-formed synthetic rows");
            checksum = checksum.wrapping_add(std::hint::black_box(tenant_id.len() as u64));
            checksum = checksum.wrapping_add(std::hint::black_box(is_public as u64));
            rows += 1;
        }
        (rows, checksum)
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
    let diff_s1_s2 =
        stage_diff_ns_per_row(s1.summary.median, s2.summary.median, TOTAL_ROWS, "S1", "S2");

    // S3: S2 ＋ f32 デコード。スクラッチバッファはループ間で使い回す
    // （`storage.rs::decode_row_embedding_and_metadata_into` と同じ設計を
    // 再実装側でも踏襲する。モジュール冒頭コメント参照）。デコード結果
    // （`scratch`）は行数のみのカウントへ捨てず、先頭・末尾要素を checksum へ
    // 加算して返す。`_decoded`（`ReimplDecodedRow`）自体は embedding を保持しない
    // 借用参照のみのため checksum の元にならず、`scratch`（f32 デコードの実出力）
    // を直接使う必要がある（release 最適化で f32 デコード自体が未観測として
    // 除去されるのを防ぐ。codex-review P1-2 指摘・PR #378）。
    //
    // S1/S2 と同形の観測処理（上記「S1〜S3 は全段で…」コメント参照）: 行あたり
    // 常に 2 回、`black_box` 経由で `u64` 累積器へ加算する。S1/S2 と型を揃える
    // ため、f32 由来値は `f32::to_bits`（ビット列の再解釈。丸め等の追加浮動小数点
    // 演算を持ち込まない）で `u32` へ変換したうえで `u64` へ拡張する
    // （codex-review P2-1 指摘・PR #378）。
    let mut scratch: Vec<f32> = Vec::with_capacity(DIM);
    let s3 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let _decoded = decode_row_reimpl(v.value(), &mut scratch)
                .expect("row decode must succeed for well-formed synthetic rows");
            let first_bits = scratch.first().copied().unwrap_or(0.0).to_bits() as u64;
            let last_bits = scratch.last().copied().unwrap_or(0.0).to_bits() as u64;
            checksum = checksum.wrapping_add(std::hint::black_box(first_bits));
            checksum = checksum.wrapping_add(std::hint::black_box(last_bits));
            rows += 1;
        }
        (rows, checksum)
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
    let diff_s2_s3 =
        stage_diff_ns_per_row(s2.summary.median, s3.summary.median, TOTAL_ROWS, "S2", "S3");
    let diff_s3_s4 = stage_diff_ns_per_row(s3.summary.median, s4_median, TOTAL_ROWS, "S3", "S4");

    // --- 残差: S0-cold -（S4 + S5） -----------------------------------------------
    // S0-hot はキャッシュヒット時のオーバーヘッドを測る別段のため、段別分解・残差の
    // 基準には使わない（S0-cold が「クエリ毎に候補行を再デコードする e2e コスト」を
    // 表す段。モジュール冒頭コメント「`SqlArenaCache` と S0 の cold/hot 分離」節
    // 参照。P1-1 是正・PR #378 codex-review 指摘）。
    let s4_plus_s5 = s4_median.saturating_add(s5_parallel.summary.median);
    let residual = s0_cold_summary.median.saturating_sub(s4_plus_s5);

    // --- 整合性検証（fail-closed） ---------------------------------------------
    // モジュール冒頭コメントの契約（「不一致ならベンチはエラー終了し測定値を
    // 出力しない」）どおり、S1〜S4・S0-cold/S0-hot/S0' の全測定・全差分計算を終えた
    // この時点で全チェックを完了させ、以降で初めて結果を出力する（codex-review 指摘・
    // PR #378: 従来は各段の println! を測定直後に行っていたため、後段の
    // 整合性チェック〔行数不一致・レイアウトドリフト〕で fail_closed する場合でも
    // 失敗前に出力済みの測定値が有効な結果として残ってしまっていた）。
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
    // S1〜S4 の段間差分は、各段が入れ子（S1 ⊆ S2 ⊆ S3 ⊆ S4）の処理を行う設計では
    // あるものの、実測は段ごとに独立した `run` 呼び出し（別トランザクション・別
    // warmup/サンプル列）であり、中央値どうしの比較である以上は測定ノイズにより
    // 逆転しうる（S5prime-S5_scalar と同じ理由。上記コメント参照。codex-review
    // P2-2 指摘・PR #378: 独立計測である事実を見落として fail_closed していた
    // ため、以前は測定ノイズだけでベンチ全体が失敗しえた）。よってこれらの差分は
    // 整合性検証（行数不一致・レイアウトドリフト等。fail-closed を維持）とは
    // 区別し、非致命扱いとする。逆転した場合は該当差分のみ「未確定」として出力し、
    // 他の測定値はそのまま出力を継続する。

    // --- 出力: 全測定・全整合性検証を完了したここまでの間、測定値は一切
    // println! していない（fail-closed 契約。上記「整合性検証」節参照）。以降は
    // 検証済みの結果をまとめて出力するのみで、新たな fail_closed 分岐は持たない。
    println!(
        "{}",
        render_stage_line(
            "S4_arena_build",
            TOTAL_ROWS,
            s4_median,
            ns_per_row(s4_median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S5_search_parallel",
            TOTAL_ROWS,
            s5_parallel.summary.median,
            ns_per_row(s5_parallel.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S5_search_scalar",
            TOTAL_ROWS,
            s5_scalar.summary.median,
            ns_per_row(s5_scalar.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S5prime_distance_only",
            TOTAL_ROWS,
            s5_prime.summary.median,
            ns_per_row(s5_prime.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    print_stage_diff_or_unconfirmed("S5prime", "S5_scalar", s5prime_vs_scalar_diff);
    println!(
        "{}",
        render_stage_line(
            "S0_cold_sql_e2e",
            TOTAL_ROWS,
            s0_cold_summary.median,
            ns_per_row(s0_cold_summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S0_hot_sql_e2e",
            TOTAL_ROWS,
            s0_hot.summary.median,
            ns_per_row(s0_hot.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S0prime_count_star",
            TOTAL_ROWS,
            s0_prime.summary.median,
            ns_per_row(s0_prime.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S1_redb_scan",
            TOTAL_ROWS,
            s1.summary.median,
            ns_per_row(s1.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S2_header_decode",
            TOTAL_ROWS,
            s2.summary.median,
            ns_per_row(s2.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    print_stage_diff_or_unconfirmed("S1", "S2", diff_s1_s2);
    println!(
        "{}",
        render_stage_line(
            "S3_f32_decode",
            TOTAL_ROWS,
            s3.summary.median,
            ns_per_row(s3.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    print_stage_diff_or_unconfirmed("S2", "S3", diff_s2_s3);
    print_stage_diff_or_unconfirmed("S3", "S4", diff_s3_s4);
    println!(
        "residual(S0-(S4+S5)): median={:.3}ms (parse/bind/result-assembly 等。read_txn 境界差を含みうる保守的な残差)",
        residual.as_secs_f64() * 1e3
    );

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
