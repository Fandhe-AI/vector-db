//! 増分インデックス（ファイル形 `INSERT` → チャンク化 → `Embedder` によるベクトル化 →
//! 置換書き込み）の**受け入れ基準**回帰テスト（TASK-121、対象ビヘイビア: INDEX-1, INDEX-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-121・`docs/spec/04-behavior/indexing.md`
//! INDEX-1, INDEX-2）。
//!
//! `tests/incremental_index.rs`（TASK-120）は検索反映・置換セマンティクス・
//! fail-closed 経路を単発挿入で検証し、`tests/incremental_write_perf.rs`（TASK-143・
//! PERSIST-2）は `Storage::put_batch` **単体**（ストレージ層のみ）の増分/全体再構築比を
//! 検証する。本ファイルはその間を埋め、`execute_insert_sql` のファイル形挿入
//! **パイプライン全体**（チャンク化＋埋め込み＋書き込み。`InsertOutcome.incremental`
//! が返す `IndexTiming`）を対象に、既存コーパスが積まれた状態での
//! 「1 ファイル追加の性能」（INDEX-1）と「検索への反映」（INDEX-2）を固定する。

use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{Embedder, HashingEmbedder};
use engine::incremental::{IncrementalConfig, IndexTiming};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 128;

/// 生成チャンク数を小さく固定するための `IncrementalConfig`（1 チャンク = 2 行。
/// `tests/incremental_index.rs` の `small_chunk_config` と同じ流儀）。
fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 2,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn documents_schema() -> TableSchema {
    TableSchema::new(
        "documents",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// 新規 DB ファイルへ `documents` テーブルを作成し、`EngineCore` を構築する
/// （ベースライン構築・全体再構築側の各ラウンドで使う。テーブルが未作成の状態から
/// 開始する）。
fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&documents_schema())
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

/// 既存 DB ファイル（`documents` テーブル作成済み・複製直後）を開いて `EngineCore` を
/// 構築する（増分側の各ラウンドで、ベースライン DB を複製したファイルに対して使う。
/// `Storage::open` の時間・テーブル作成の時間はいずれの側の計測対象にも含めない）。
fn open_core_on_existing_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage on copied db");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
}

fn vector_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

fn body_text_cells(result: &engine::sql::exec::QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => s.clone(),
            other => panic!("expected Cell::Text, got {other:?}"),
        })
        .collect()
}

/// `values` の中央値を返す（`values` は非空であること、呼び出し側で保証する）。
fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values[values.len() / 2]
}

/// `IndexTiming` の各段階（チャンク化・埋め込み・書き込み）の合計を返す
/// （オーバーフローを未定義動作にしないよう checked 加算する。coding-rust.md 参照）。
fn timing_total(timing: IndexTiming) -> Duration {
    timing
        .chunking
        .checked_add(timing.embedding)
        .and_then(|d| d.checked_add(timing.write))
        .expect("timing sum must not overflow")
}

/// ベースラインコーパスの「小」規模（既存ファイル数）。増分挿入コストが既存コーパス
/// 規模 `N` に依存しないことを直接検証するため、`BASELINE_FILES_LARGE` との 2 点で
/// 増分挿入時間を比較する（後述コメント参照。spec 所定の値ではないテスト固有の値）。
const BASELINE_FILES_SMALL: usize = 8;

/// ベースラインコーパスの「大」規模。`BASELINE_FILES_SMALL` の `CORPUS_SCALE_FACTOR`
/// 倍とする。
const BASELINE_FILES_LARGE: usize = BASELINE_FILES_SMALL * CORPUS_SCALE_FACTOR;

/// 「大」規模が「小」規模の何倍のコーパスかを表す係数。
const CORPUS_SCALE_FACTOR: usize = 8;

/// 1 ファイルあたりの行数（`lines_per_chunk=2` と合わせて 1 ファイル = 2 チャンク）。
const LINES_PER_FILE: usize = 4;

/// ノイズ対策として、各コーパス規模での増分挿入を複数回計測し中央値を取る回数。
const MEASUREMENT_ROUNDS: usize = 3;

/// 判定の許容係数（テスト固有の計測パラメータ。分母・実測値は spec 本文を転記しない）。
///
/// 「大」規模（`BASELINE_FILES_LARGE`）での増分 1 ファイル挿入時間は、「小」規模
/// （`BASELINE_FILES_SMALL`）での同時間の `GROWTH_SLACK_NUMERATOR` 倍以内に収まる
/// ことを要求する。増分コストが既存コーパス規模 `N` に依存しない（O(1) 相当）なら
/// この比は測定ノイズ程度（1 に近い）に収まる。一方、置換対象の走査が既存コーパス
/// 全体をスキャンしてしまう回帰（O(N) 相当）ではこの比は `CORPUS_SCALE_FACTOR`
/// （= 8）に近づくため、`GROWTH_SLACK_NUMERATOR` はその中間（ノイズは許容しつつ
/// 線形劣化は確実に検出できる値）に設定する。
const GROWTH_SLACK_NUMERATOR: u32 = 3;

fn generic_body(index: usize) -> String {
    (0..LINES_PER_FILE)
        .map(|line| format!("line {line} of file {index}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 既存ファイル数 `baseline_files` のコーパスを構築したうえで、1 ファイルを増分挿入
/// する所要時間を `MEASUREMENT_ROUNDS` 回計測し、中央値を返す。
///
/// 各ラウンドは素のベースライン DB（既存ファイル数固定）を新規ファイルへ複製してから
/// 計測することで、計測時点の既存コーパス規模を毎ラウンド揃える（前のラウンドの増分
/// 挿入で規模が変わらないようにする）。ウォームアップ（ファイルシステムキャッシュ等の
/// 初回コストの除外）も同様に複製先で行い、`baseline_path` 自体は変更しない。
fn measure_single_file_incremental_insert(baseline_files: usize, label: &str) -> Duration {
    let baseline_path = unique_db_path(&format!("recall-baseline-{label}"));
    let _baseline_cleanup = CleanupGuard(baseline_path.clone());
    {
        let core = new_core_with_documents_table(&baseline_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for i in 0..baseline_files {
            let sql = insert_file_sql(
                "documents",
                &format!("corpus/{label}-baseline-{i:04}.txt"),
                &generic_body(i),
                &format!("op-{label}-baseline-{i:04}"),
            );
            core.execute_insert_sql(&write_ctx, &sql)
                .expect("baseline file insert should succeed");
        }
    }

    // ウォームアップ。
    {
        let warmup_path = unique_db_path(&format!("recall-warmup-{label}"));
        let _warmup_cleanup = CleanupGuard(warmup_path.clone());
        std::fs::copy(&baseline_path, &warmup_path).expect("copy baseline for warmup");
        let core = open_core_on_existing_table(&warmup_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/{label}-warmup-new.txt"),
            &generic_body(baseline_files),
            &format!("op-{label}-warmup-new"),
        );
        core.execute_insert_sql(&write_ctx, &sql)
            .expect("warmup incremental insert should succeed");
    }

    // 増分挿入（1 ファイル）の所要時間を複数回計測する。
    let mut durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS {
        let round_path = unique_db_path(&format!("recall-incremental-{label}-round-{round}"));
        let _round_cleanup = CleanupGuard(round_path.clone());
        std::fs::copy(&baseline_path, &round_path).expect("copy pristine baseline for round");
        let core = open_core_on_existing_table(&round_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/{label}-incremental-round-{round}.txt"),
            &generic_body(baseline_files + round),
            &format!("op-{label}-incremental-round-{round}"),
        );
        let outcome = core
            .execute_insert_sql(&write_ctx, &sql)
            .expect("incremental file insert (measured) should succeed");
        let incremental = outcome
            .incremental
            .expect("file-form insert sets incremental");
        durations.push(timing_total(incremental.timing));
    }

    median(durations)
}

// --- INDEX-1: 増分性能（既存コーパス規模に依存しないことの直接検証） -------------
//
// 対象時間はサーバー内部処理（チャンク化・埋め込み・redb 書き込み）のみで、
// wire プロトコル転送時間は含めない（`InsertOutcome.incremental.timing` を直接使う
// ため SQL 解析・束縛の時間もほぼ含まれない）。
//
// 判定は「小規模コーパスでの増分 1 ファイル挿入時間」対「大規模コーパス（`小規模 ×
// CORPUS_SCALE_FACTOR` 件）での同時間」を直接比較する。以前は全体再構築の
// 「1 ファイルあたり平均コスト」との比（`t_inc * (N+1) <= t_full * slack`）で判定
// していたが、この比較は増分コストが既存コーパス規模 `N` に比例して劣化する場合でも
// 末尾/平均比がおよそ 2 倍にしかならず、緩い slack を通過してしまい「増分コストは
// 既存コーパス規模に依存しない」という契約を固定できていなかった（P1 指摘）。
// 複数のコーパス規模で単一ファイル追加時間を直接比較することで、規模依存の劣化を
// 確実に検出する。
#[test]
fn index1_incremental_indexing_completes_within_ratio_threshold_of_full_rebuild() {
    let t_inc_small = measure_single_file_incremental_insert(BASELINE_FILES_SMALL, "small");
    let t_inc_large = measure_single_file_incremental_insert(BASELINE_FILES_LARGE, "large");

    let ratio = t_inc_large.as_secs_f64() / t_inc_small.as_secs_f64().max(f64::EPSILON);

    // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
    println!(
        "index1 incremental indexing perf: t_inc_small={t_inc_small:?} \
         (baseline_files={BASELINE_FILES_SMALL}) t_inc_large={t_inc_large:?} \
         (baseline_files={BASELINE_FILES_LARGE}) corpus_scale_factor={CORPUS_SCALE_FACTOR} \
         ratio={ratio:.4}"
    );

    // `Duration` 同士の乗算で比較し、浮動小数の除算誤差を判定から排除する
    // （`ratio` はログ出力にのみ使い、判定には使わない）。
    assert!(
        t_inc_large <= t_inc_small.saturating_mul(GROWTH_SLACK_NUMERATOR),
        "incremental single-file insert time must not scale with existing corpus size: \
         at baseline_files={BASELINE_FILES_LARGE} ({t_inc_large:?}) must stay within \
         {GROWTH_SLACK_NUMERATOR}x of baseline_files={BASELINE_FILES_SMALL} ({t_inc_small:?}); \
         a linear scan regression would approach {CORPUS_SCALE_FACTOR}x. ratio={ratio:.4}"
    );
}

// --- INDEX-2: 結果整合性（既存コーパスが積まれた状態での増分追加の検索反映） -------

#[test]
fn index2_added_file_is_reflected_in_next_search_over_existing_corpus() {
    let path = unique_db_path("recall-index2-corpus");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    const CORPUS_FILES: usize = 10;
    let marker_for = |i: usize| format!("existingmarker{i:02}");
    let body_for = |i: usize| {
        format!(
            "corpus filler line one\n\
             corpus filler line two\n\
             unique {} token line\n\
             unique {} token line continued",
            marker_for(i),
            marker_for(i)
        )
    };

    // 既存コーパスを事前投入する。
    for i in 0..CORPUS_FILES {
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/existing-{i:02}.txt"),
            &body_for(i),
            &format!("op-existing-{i:02}"),
        );
        core.execute_insert_sql(&write_ctx, &sql)
            .expect("existing corpus file insert should succeed");
    }

    // 新規ファイルを増分挿入する（固有トークン zzzqq を含む）。
    let new_body = "alpha alpha filler line one\n\
                     alpha alpha filler line two\n\
                     charlie charlie unique marker zzzqq\n\
                     charlie charlie unique marker zzzqq continued";
    let outcome = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "corpus/new-file.txt", new_body, "op-new-file"),
        )
        .expect("incremental file insert should succeed");
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");
    assert_eq!(incremental.chunks_written, 2);
    assert_eq!(incremental.rows_replaced, 2);

    let embedder = HashingEmbedder::new(DIM).expect("valid dim");

    // 1. 新規ファイルの内容が、既存コーパスが積まれた状態でも密検索・ハイブリッド検索の
    //    上位（LIMIT を絞った結果集合）に反映されること。`HashingEmbedder` は決定的だが
    //    意味的ではない（`embedding.rs` モジュールドキュメント参照）ため、
    //    厳密な Top-1 一致ではなく上位集合中に含まれることを確認する
    //    （`tests/incremental_index.rs` と同じ流儀）。
    let query_text = "unique marker zzzqq";
    let query_vec = embedder
        .embed_batch(&[query_text])
        .expect("query embedding should succeed")
        .remove(0);
    let query_literal = vector_literal(&query_vec);

    let distance_sql =
        format!("SELECT body FROM documents ORDER BY embedding <=> {query_literal} LIMIT 3");
    let distance_result = core
        .execute_sql(&read_ctx, &distance_sql)
        .expect("distance search should succeed");
    let distance_bodies = body_text_cells(&distance_result);
    assert!(
        distance_bodies.iter().any(|b| b.contains("zzzqq")),
        "distance search top-3 should contain the new file's marker, got: {distance_bodies:?}"
    );

    let hybrid_sql = format!(
        "SELECT body FROM documents ORDER BY HYBRID(embedding, {query_literal}, body, '{query_text}') LIMIT 3"
    );
    let hybrid_result = core
        .execute_sql(&read_ctx, &hybrid_sql)
        .expect("hybrid search should succeed");
    let hybrid_bodies = body_text_cells(&hybrid_result);
    assert!(
        hybrid_bodies.iter().any(|b| b.contains("zzzqq")),
        "hybrid search top-3 should contain the new file's marker, got: {hybrid_bodies:?}"
    );

    // 2. 存在性チェック（ランキングに依存しない、より強い保証）: 新規ファイルのパスで
    //    直接引けること。
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let new_file_rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'corpus/new-file.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select by new path should succeed");
    assert_eq!(new_file_rows.rows.len(), 2);
    assert!(
        body_text_cells(&new_file_rows)
            .iter()
            .any(|b| b.contains("zzzqq")),
        "new file's chunks must be retrievable by path and contain the marker"
    );

    // 3. 増分追加後も既存ファイルが引き続き検索可能であること（増分反映が既存索引を
    //    破壊しないことを固定する）。既存コーパスの先頭・末尾ファイルを代表として確認する。
    for &i in &[0usize, CORPUS_FILES - 1] {
        let existing_path = format!("corpus/existing-{i:02}.txt");
        let existing_rows = core
            .execute_sql(
                &read_ctx,
                &format!(
                    "SELECT body FROM documents WHERE path = '{existing_path}' ORDER BY embedding <=> {zero_vec} LIMIT 100"
                ),
            )
            .expect("select by existing path should succeed");
        assert_eq!(existing_rows.rows.len(), 2);
        let marker = marker_for(i);
        assert!(
            body_text_cells(&existing_rows)
                .iter()
                .any(|b| b.contains(&marker)),
            "existing file {existing_path} content must survive the incremental add"
        );
    }
}
