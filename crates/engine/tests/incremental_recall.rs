//! 増分インデックス（ファイル形 `INSERT` → チャンク化 → `Embedder` によるベクトル化 →
//! 置換書き込み）の回帰テスト（TASK-121、対象ビヘイビア: INDEX-1, INDEX-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-121・`docs/spec/04-behavior/indexing.md` INDEX-1,
//! INDEX-2）。
//!
//! **位置づけ**: 本テストは TASK-121（対象ビヘイビア: INDEX-1, INDEX-2。上記は
//! ポインタ参照のみ）に関連する、本リポ独自の回帰テストである。README で公開済みの
//! 要点（ファイル形 INSERT → チャンク化 → ベクトル化 → 同一パス置換書き込み、および
//! 追加ファイルが検索で取得可能になること）に基づいて、本テストが独自に設定した
//! 基準（測定方法・閾値・アサーション）を検証するものであり、private spec の受け入れ
//! 基準の転記・具体化ではない。
//!
//! `tests/incremental_index.rs`（TASK-120）は検索反映・置換セマンティクス・
//! fail-closed 経路を単発挿入で検証し、`tests/incremental_write_perf.rs`（TASK-143・
//! PERSIST-2）は `Storage::put_batch` **単体**（ストレージ層のみ）の増分/全体再構築比を
//! 検証する。本ファイルはその間を埋め、`execute_insert_sql` のファイル形挿入
//! **パイプライン全体**（チャンク化＋埋め込み＋行バッファ構築＋書き込み。
//! `InsertOutcome.incremental` が返す `IndexTiming`）を対象に、既存コーパスが積まれた
//! 状態での「1 ファイル追加の性能」の代替回帰指標（`index1_*`）と「検索への反映」
//! （INDEX-2）を固定する。
//!
//! 「増分コストが既存コーパス規模 N に依存しない」ことは本テストが検証する性質では
//! ない。実装上、`tenant::replace_typed_rows_by_text_key`（同一パスの既存行を特定
//! するための置換キー探索）はテナント名前空間内の既存行を線形走査するため、増分
//! 1 ファイルの所要時間は既存コーパス規模に対して線形に増加する。この線形走査
//! コストの解消（キー探索の索引化）は redb スキーマ・障害回復台帳の不変条件に関わる
//! engine 側の設計変更であり、本テストの回帰検知パラメータ調整では解消できない
//! 別 Issue の対象とする。

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

/// ベースラインコーパスの規模（既存ファイル数）。ノイズに対するマージンと CI での
/// 実行時間のバランスを取ったテスト固有のパラメータ（spec 所定の値ではない）。
const BASELINE_FILES: usize = 64;

/// 1 ファイルあたりの行数（`lines_per_chunk=2` と合わせて 1 ファイル = 2 チャンク）。
const LINES_PER_FILE: usize = 4;

/// ノイズ対策として、増分・全体再構築それぞれを複数回計測し中央値を取る回数。
const MEASUREMENT_ROUNDS: usize = 3;

/// 判定閾値の分母（増分側は全体再構築側の `1 / RATIO_THRESHOLD_DENOM` 以下の時間で
/// 完了すること）。本テスト固有の回帰検知パラメータであり、`tests/incremental_write_perf.rs`
/// （TASK-143・PERSIST-2）と数値・判定式の形が一致するのは実装が同じ書き込み経路を
/// 共有するためである。
const RATIO_THRESHOLD_DENOM: u32 = 10;

fn generic_body(index: usize) -> String {
    (0..LINES_PER_FILE)
        .map(|line| format!("line {line} of file {index}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 既存ファイル数 `BASELINE_FILES` のコーパスを構築したうえで、1 ファイルを増分挿入
/// する所要時間を `MEASUREMENT_ROUNDS` 回計測し、中央値を返す。
///
/// 各ラウンドは素のベースライン DB（既存ファイル数固定）を新規ファイルへ複製してから
/// 計測することで、計測時点の既存コーパス規模を毎ラウンド揃える（前のラウンドの増分
/// 挿入で規模が変わらないようにする）。ウォームアップ（ファイルシステムキャッシュ等の
/// 初回コストの除外）も同様に複製先で行い、`baseline_path` 自体は変更しない。
fn measure_single_file_incremental_insert() -> Duration {
    let baseline_path = unique_db_path("recall-baseline");
    let _baseline_cleanup = CleanupGuard(baseline_path.clone());
    {
        let core = new_core_with_documents_table(&baseline_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for i in 0..BASELINE_FILES {
            let sql = insert_file_sql(
                "documents",
                &format!("corpus/baseline-{i:04}.txt"),
                &generic_body(i),
                &format!("op-baseline-{i:04}"),
            );
            core.execute_insert_sql(&write_ctx, &sql)
                .expect("baseline file insert should succeed");
        }
    }

    // ウォームアップ。
    {
        let warmup_path = unique_db_path("recall-warmup");
        let _warmup_cleanup = CleanupGuard(warmup_path.clone());
        std::fs::copy(&baseline_path, &warmup_path).expect("copy baseline for warmup");
        let core = open_core_on_existing_table(&warmup_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let sql = insert_file_sql(
            "documents",
            "corpus/warmup-new.txt",
            &generic_body(BASELINE_FILES),
            "op-warmup-new",
        );
        core.execute_insert_sql(&write_ctx, &sql)
            .expect("warmup incremental insert should succeed");
    }

    // 増分挿入（1 ファイル）の所要時間を複数回計測する。
    let mut durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS {
        let round_path = unique_db_path(&format!("recall-incremental-round-{round}"));
        let _round_cleanup = CleanupGuard(round_path.clone());
        std::fs::copy(&baseline_path, &round_path).expect("copy pristine baseline for round");
        let core = open_core_on_existing_table(&round_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/incremental-round-{round}.txt"),
            &generic_body(BASELINE_FILES + round),
            &format!("op-incremental-round-{round}"),
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

/// `BASELINE_FILES + 1` 件を空 DB から順次挿入する「全体再構築」の総所要時間を
/// `MEASUREMENT_ROUNDS` 回計測し、中央値を返す（増分側と同じ総ファイル数を毎ラウンド
/// 書き込むことで比較条件を揃える）。一括投入構文（INDEX-4）は本移行時点で未実装
/// （ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-4。spec 側の検討状況はここに
/// 転記しない）のため、SQL 表層で実現可能な「空 DB への逐次 `INSERT`」を全体再構築の
/// 代替として用いる。
fn measure_full_rebuild() -> Duration {
    let mut durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS {
        let full_path = unique_db_path(&format!("recall-full-rebuild-{round}"));
        let _full_cleanup = CleanupGuard(full_path.clone());
        let core = new_core_with_documents_table(&full_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut total = Duration::ZERO;
        for i in 0..=BASELINE_FILES {
            let sql = insert_file_sql(
                "documents",
                &format!("corpus/full-{round}-{i:04}.txt"),
                &generic_body(i),
                &format!("op-full-{round}-{i:04}"),
            );
            let outcome = core
                .execute_insert_sql(&write_ctx, &sql)
                .expect("full rebuild file insert (measured) should succeed");
            let incremental = outcome
                .incremental
                .expect("file-form insert sets incremental");
            total = total
                .checked_add(timing_total(incremental.timing))
                .expect("full rebuild duration sum must not overflow");
        }
        durations.push(total);
    }

    median(durations)
}

// --- INDEX-1 関連: 増分性能の本テスト固有の回帰基準（既存コーパスへの 1 ファイル
// 追加 vs 全体再構築） ---------------------------------------------------------
//
// 対象時間はサーバー内部処理（チャンク化・埋め込み・行バッファ構築・redb 書き込み。
// `IndexTiming.write` のドキュメンテーションコメント参照）のみで、wire プロトコル
// 転送時間は含めない（`InsertOutcome.incremental.timing` を直接使うため SQL 解析・
// 束縛の時間もほぼ含まれない）。
//
// 判定は「増分 1 ファイルの処理時間」対「全体再構築（`BASELINE_FILES + 1` 件を
// 空 DB から構築）の総処理時間」の比を、`tests/incremental_write_perf.rs`
// （PERSIST-2）と同じ形（`t_inc * RATIO_THRESHOLD_DENOM <= t_full`）で判定する
// （`RATIO_THRESHOLD_DENOM` のドキュメンテーションコメント参照）。本テストは
// 「増分コストが既存コーパス規模に依存しないこと」までは固定しない
// （モジュールドキュメント参照）。
#[test]
fn index1_incremental_indexing_completes_within_ratio_threshold_of_full_rebuild() {
    let t_inc = measure_single_file_incremental_insert();
    let t_full = measure_full_rebuild();

    let ratio = t_inc.as_secs_f64() / t_full.as_secs_f64().max(f64::EPSILON);

    // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
    println!(
        "index1 incremental indexing perf: t_inc={t_inc:?} t_full={t_full:?} \
         (baseline_files={BASELINE_FILES}) ratio={ratio:.4}"
    );

    // `Duration` 同士の乗算で比較し、浮動小数の除算誤差を判定から排除する
    // （`ratio` はログ出力にのみ使い、判定には使わない）。
    assert!(
        t_inc.saturating_mul(RATIO_THRESHOLD_DENOM) <= t_full,
        "incremental single-file insert ({t_inc:?}) must complete within \
         1/{RATIO_THRESHOLD_DENOM} of full rebuild ({t_full:?}) of baseline_files={BASELINE_FILES}; \
         ratio={ratio:.4}"
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
