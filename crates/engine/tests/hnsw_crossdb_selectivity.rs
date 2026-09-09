//! 選択率 33%（crossdb fixture 相当）での `ann_masked` 到達実測ハーネス
//! （Issue #659・親 #651）。
//!
//! 背景（ポインタのみ）: Issue #487 の可視比率スイープ（均等分散 `id % N`
//! マスク・一様乱数ベクトル）は 1/2〜1/10 の全点で `mask_splits_graph`
//! （連結性検査による plain scan 縮退）に落ち `ann_masked` を一度も観測
//! できなかった。Issue #658（PR）は `lang='ja'`（crossdb fixture・可視
//! 23,000 行中 7,621 行 ≒ 33.1%）が `full_scan_ratio`（既定 1/10）を超える
//! ため `mask_splits_graph` 縮退が有力な要因と**推測**したが、
//! `hnsw_index_cache_stats()` は wire 非露出のため確認できないまま本 Issue
//! へ申し送られた。本ファイルはその確認を担う。
//!
//! - A1: `default`（既定 `full_scan_ratio=1/10`）で `ann_masked` /
//!   `mask_splits_graph` / plain scan のどれを通るかをカウンタで確定する。
//! - A2: ACORN-1 opt-in（`acorn_max_visible_ratio`）の有無で到達可否が
//!   変わるかを比較する。
//! - A4: 既定エンジン（brute-force）対照の Recall@k（k=10・200。層 B は
//!   両方を計測し、k に応じた実質 Recall@k を記録する）が劣化しないこと
//!   を確認する。
//!
//! 判定規則（実測前に事前登録。`docs/design/hnsw-rls-cardinality-switch.md`
//! 「Issue #659」節と同一）:
//! - arm 分類は `crates/engine/benches/knn_profile_bench.rs::observed_arm_label`
//!   と同一の優先順位（acorn > subset > plain_scan_ratio > mask_splits_graph
//!   > masked_short > none）。全カウンタ 0 は vacuous として fail。
//! - `ann_masked` / `ann_masked_two_hop` に到達した arm は、同 arm の
//!   フィルタなし Recall@k に対し `filtered >= unfiltered - 0.02` である
//!   こと（フィルタ付き ANN がフィルタなし ANN より悪化しないことの検証）。
//! - 縮退（plain scan）した arm は既定エンジンと厳密一致（Recall 1.0）で
//!   あること。
//!
//! 層 A（常時実行・`make ci` 対象・fixture 不要）は分類関数・抽出器・
//! ハーネス本体（縮小フィクスチャ）を回帰として固定する。層 B
//! （`#[ignore]`・`make hnsw-crossdb-selectivity`・release・
//! `CROSSDB_DIR` 必須）は crossdb fixture の実体（`docs25k.redb`・
//! `queries200.jsonl`）で arm 表を標準出力へ記録する。
//!
//! production コード（`crates/engine/src/`）は無変更・テスト専任。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hnsw::{HnswParams, Ratio};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine::{self, SearchEngineKind};
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- arm 分類（`benches/knn_profile_bench.rs::observed_arm_label` の複製。
// bench 側関数は `benches/` 配下で結合テストから参照できないため、同一の
// 優先順位規則をテスト側にも複製する。ドキュメンテーションコメント冒頭
// 参照） ----------

/// カウンタの増分（before/after 差分）から到達した探索方式を分類する。
/// `crates/engine/benches/knn_profile_bench.rs::observed_arm_label` と
/// 完全に同じ優先順位を保つこと（層 A
/// `arm_label_precedence_matches_knn_profile_bench` で固定）。
fn observed_arm_label(
    acorn_searches_delta: u64,
    subset_searches_delta: u64,
    plain_scans_delta: u64,
    mask_splits_graph_delta: u64,
    masked_short_delta: u64,
) -> &'static str {
    if acorn_searches_delta > 0 {
        "ann_masked_two_hop"
    } else if subset_searches_delta > 0 {
        "ann_masked"
    } else if plain_scans_delta > 0 {
        "plain_scan_ratio"
    } else if mask_splits_graph_delta > 0 {
        "plain_scan_mask_split"
    } else if masked_short_delta > 0 {
        "plain_scan_masked_short"
    } else {
        "n/a (brute_force engine or vacuous)"
    }
}

// `HnswIndexCacheStats` は `sql::hnsw_cache` 内 `pub(crate)` のため結合テスト
// からは型名を書けない（`core.hnsw_index_cache_stats()` の戻り値は型推論に
// 任せる）。macro でフィールドアクセスのみを共有し、型を名指ししない。
macro_rules! arm_label_delta {
    ($before:expr, $after:expr) => {
        observed_arm_label(
            $after.acorn_searches.saturating_sub($before.acorn_searches),
            $after
                .subset_searches
                .saturating_sub($before.subset_searches),
            $after.plain_scans.saturating_sub($before.plain_scans),
            $after
                .mask_splits_graph
                .saturating_sub($before.mask_splits_graph),
            $after.masked_short.saturating_sub($before.masked_short),
        )
    };
}

// ---------- crossdb fixture（`crates/engine/examples/seed_docs.rs`）形式の
// query jsonl 行から `lang`・`embedding` を抽出する最小パーサ。
// engine に公開 JSON パーサは無く、`seed_docs::run_queries` の複製は
// ドリフト源になるため独立実装する。untrusted な wire 入力経路ではなく
// テスト専用のローカル fixture 読み取りのため fail-closed な panic で
// 不備を検出する（黙って skip しない） ----------

const MAX_QUERY_DIM: usize = 4096;

/// `"lang":"xx"` フィールドを抽出する。見つからない場合は panic。
fn extract_lang(line: &str) -> String {
    let key = "\"lang\":\"";
    let start = line
        .find(key)
        .unwrap_or_else(|| panic!("query line missing \"lang\" field: {line:?}"))
        + key.len();
    let rest = &line[start..];
    let end = rest
        .find('"')
        .unwrap_or_else(|| panic!("query line has unterminated \"lang\" value: {line:?}"));
    rest[..end].to_string()
}

/// `"embedding":[...]` フィールドを抽出し `f32` ベクトルへパースする。
/// 次元不一致・非有限値・欠落・上限超過は panic（fail-closed）。
fn extract_embedding(line: &str, expected_dim: usize) -> Vec<f32> {
    assert!(
        expected_dim <= MAX_QUERY_DIM,
        "expected_dim {expected_dim} exceeds MAX_QUERY_DIM {MAX_QUERY_DIM}"
    );
    let key = "\"embedding\":[";
    let start = line
        .find(key)
        .unwrap_or_else(|| panic!("query line missing \"embedding\" field: {line:?}"))
        + key.len();
    let rest = &line[start..];
    let end = rest
        .find(']')
        .unwrap_or_else(|| panic!("query line has unterminated \"embedding\" array: {line:?}"));
    let body = &rest[..end];
    let mut values = Vec::with_capacity(expected_dim.min(MAX_QUERY_DIM));
    for tok in body.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let v: f32 = tok
            .parse()
            .unwrap_or_else(|e| panic!("embedding component {tok:?} is not a valid f32: {e}"));
        assert!(
            v.is_finite(),
            "embedding component {v} is not finite (line: {line:?})"
        );
        assert!(
            values.len() < MAX_QUERY_DIM,
            "embedding has more than MAX_QUERY_DIM={MAX_QUERY_DIM} components"
        );
        values.push(v);
    }
    assert_eq!(
        values.len(),
        expected_dim,
        "embedding dim mismatch: expected {expected_dim}, got {} (line: {line:?})",
        values.len()
    );
    values
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

// ---------- 層 A ----------

#[test]
fn arm_label_precedence_matches_knn_profile_bench() {
    // acorn > subset > plain_scan_ratio > mask_splits_graph > masked_short > none
    assert_eq!(observed_arm_label(1, 1, 1, 1, 1), "ann_masked_two_hop");
    assert_eq!(observed_arm_label(0, 1, 1, 1, 1), "ann_masked");
    assert_eq!(observed_arm_label(0, 0, 1, 1, 1), "plain_scan_ratio");
    assert_eq!(observed_arm_label(0, 0, 0, 1, 1), "plain_scan_mask_split");
    assert_eq!(observed_arm_label(0, 0, 0, 0, 1), "plain_scan_masked_short");
    assert_eq!(
        observed_arm_label(0, 0, 0, 0, 0),
        "n/a (brute_force engine or vacuous)"
    );
}

#[test]
fn embedding_extractor_parses_seed_docs_query_line_and_rejects_malformed() {
    let line = r#"{"text":"t","lang":"ja","topic":"x","embedding":[0.5,-0.25,0]}"#;
    assert_eq!(extract_lang(line), "ja");
    assert_eq!(extract_embedding(line, 3), vec![0.5f32, -0.25, 0.0]);

    // dim 不一致
    let result = std::panic::catch_unwind(|| extract_embedding(line, 4));
    assert!(result.is_err(), "dim mismatch must panic");

    // embedding 欠落
    let missing = r#"{"text":"t","lang":"ja"}"#;
    let result = std::panic::catch_unwind(|| extract_embedding(missing, 3));
    assert!(result.is_err(), "missing embedding must panic");

    // 非数値混入
    let bad_num = r#"{"lang":"ja","embedding":[0.5,not_a_number,0]}"#;
    let result = std::panic::catch_unwind(|| extract_embedding(bad_num, 3));
    assert!(result.is_err(), "non-numeric component must panic");

    // 非有限値
    let non_finite = r#"{"lang":"ja","embedding":[0.5,NaN,0]}"#;
    let result = std::panic::catch_unwind(|| extract_embedding(non_finite, 3));
    assert!(
        result.is_err(),
        "NaN literal must fail to parse as f32 and panic"
    );

    // ] 欠落
    let unterminated = r#"{"lang":"ja","embedding":[0.5,0.25"#;
    let result = std::panic::catch_unwind(|| extract_embedding(unterminated, 2));
    assert!(result.is_err(), "unterminated array must panic");

    // lang 欠落
    let no_lang = r#"{"embedding":[0.1]}"#;
    let result = std::panic::catch_unwind(|| extract_lang(no_lang));
    assert!(result.is_err(), "missing lang must panic");
}

/// 縮小フィクスチャ（1,200 行・dim16・`lang` を `i % 3 == 0 -> "ja"` で
/// 約 1/3 に割り当て）で、ハーネス本体（warm-up → フィルタ付き DISTANCE →
/// arm 分類 → Recall 集計 → テナント非漏えい）が非 vacuous に動くことを
/// 固定する（crossdb fixture の実体ではないが、CI で層 B と同型の経路を
/// 常時検証する目的）。
#[test]
fn synthetic_lang_third_mask_reaches_subset_path_after_warmup() {
    const DIM: u32 = 16;
    const ROWS: usize = 1_200;

    struct TestRng {
        state: u64,
    }
    impl TestRng {
        fn new(seed: u64) -> Self {
            Self {
                state: if seed == 0 {
                    0x9E37_79B9_7F4A_7C15
                } else {
                    seed
                },
            }
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn next_unit(&mut self) -> f32 {
            let bits = (self.next_u64() >> 40) as u32;
            (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
        }
    }
    fn normalize(v: &mut [f32]) {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
    }

    let dir = unique_db_path("hnsw-crossdb-selectivity-synth");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");

    let mut rng = TestRng::new(0x659_659);
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(ROWS);
    for _ in 0..ROWS {
        let mut v: Vec<f32> = (0..DIM as usize).map(|_| rng.next_unit()).collect();
        normalize(&mut v);
        vectors.push(v);
    }
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = OperationId::parse("hnsw-crossdb-selectivity-synth-seed").expect("valid op id");
    let meta_ja = encode_scalar_columns(&schema, &[Value::Null, Value::Text("ja".into())])
        .expect("encode lang=ja metadata");
    let meta_en = encode_scalar_columns(&schema, &[Value::Null, Value::Text("en".into())])
        .expect("encode lang=en metadata");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let metadata = if i % 3 == 0 {
                meta_ja.as_slice()
            } else {
                meta_en.as_slice()
            };
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata,
                },
            )
        })
        .collect();
    engine::tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).expect("seed rows");

    let ref_dir = unique_db_path("hnsw-crossdb-selectivity-synth-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx, &rows, &op_id).expect("seed ref rows");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let kind = hnsw_kind_default();
    let core = EngineCore::from_storage_with_engine(storage, kind);

    // warm-up: フィルタなしクエリで `FullVisible` 経路の索引を構築させる
    // （`Subset` 経路は base 未構築時は plain scan へ縮退する契約）。
    let warm_sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 10",
        vec_literal(&vectors[0])
    );
    core.execute_sql(&ctx, &warm_sql).expect("warm-up query");
    let baseline = core.hnsw_index_cache_stats();
    assert_eq!(baseline.entries, 1, "warm-up must build exactly one entry");

    const K: usize = 10;
    let mut hits = 0usize;
    let queries = 30usize;
    for i in 0..queries {
        let q = &vectors[i * (ROWS / queries)];
        let sql = format!(
            "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(q)
        );
        let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
        let want = ref_core
            .execute_sql(&ctx, &sql)
            .expect("filtered query (ref)")
            .rows;
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
        for row in &got {
            assert_eq!(
                (row.id - 1) % 3,
                0,
                "row {} does not satisfy lang='ja' (0-indexed multiples of 3 are lang='ja')",
                row.id
            );
        }
    }
    let recall = hits as f64 / (queries * K) as f64;
    assert!(
        recall >= 0.9,
        "filtered DISTANCE recall@{K} against the default engine must be >= 0.9 (got {recall})"
    );

    let after = core.hnsw_index_cache_stats();
    let label = arm_label_delta!(baseline, after);
    assert_ne!(
        label, "n/a (brute_force engine or vacuous)",
        "synthetic 1/3 mask must not be vacuous (some Subset-shape counter must fire)"
    );
    assert_eq!(
        after.entries, baseline.entries,
        "Subset shape must never register a cache entry"
    );
}

fn hnsw_kind_default() -> SearchEngineKind {
    search_engine::hnsw_kind(HnswParams::default()).expect("valid hnsw params")
}

// ---------- 層 B（crossdb fixture の実体を使う。`#[ignore]`・release 専用）
// ----------

#[cfg(test)]
mod layer_b {
    use super::*;
    use std::fs;

    const TABLE: &str = "docs";
    const DIM: u32 = 128;
    // tenant-b（Private）の id 範囲。`seed_docs.rs::seed`（rows=25,000）は
    // batch=1,000・`batch_no % 10 == 9` を tenant-b（Private）として投入する
    // ため、9,001〜10,000・19,001〜20,000 が tenant-b（本テストの ctx
    // `tenant-a` からは不可視）。テナント非漏えいの機械検証に使う。
    const TENANT_B_RANGES: [(u64, u64); 2] = [(9_001, 10_000), (19_001, 20_000)];

    fn schema() -> TableSchema {
        TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("topic", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    fn crossdb_dir() -> std::path::PathBuf {
        let dir = std::env::var("CROSSDB_DIR").unwrap_or_else(|_| {
            panic!(
                "CROSSDB_DIR is required for the layer B crossdb selectivity harness \
                 (a directory containing docs25k.redb and queries200.jsonl; \
                 see `make hnsw-crossdb-selectivity`)"
            )
        });
        std::path::PathBuf::from(dir)
    }

    fn load_queries(dir: &std::path::Path) -> Vec<(String, Vec<f32>)> {
        let path = dir.join("queries200.jsonl");
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| (extract_lang(line), extract_embedding(line, DIM as usize)))
            .collect()
    }

    /// fixture の `docs25k.redb` をテスト専用の一時パスへコピーして開く。
    /// 元ファイルを直接開くと並走する他の crossdb 計測ジョブと competing
    /// writer になり得るため、常にコピー経由にする。
    fn open_copy(
        source: &std::path::Path,
        label: &str,
    ) -> (std::path::PathBuf, CleanupGuard, Storage) {
        let dest = unique_db_path(label);
        fs::copy(source, &dest).unwrap_or_else(|e| {
            panic!(
                "failed to copy {} to {}: {e}",
                source.display(),
                dest.display()
            )
        });
        let guard = CleanupGuard(dest.clone());
        let storage = Storage::open(&dest)
            .unwrap_or_else(|e| panic!("failed to open {}: {e}", dest.display()));
        (dest, guard, storage)
    }

    fn hnsw_kind_with(
        full_scan_ratio: Option<Ratio>,
        acorn_max_visible_ratio: Option<Ratio>,
    ) -> SearchEngineKind {
        let kind = search_engine::hnsw_kind(HnswParams::default()).expect("valid hnsw params");
        let SearchEngineKind::Hnsw(mut validated) = kind else {
            panic!("hnsw_kind must return SearchEngineKind::Hnsw");
        };
        if let Some(ratio) = full_scan_ratio {
            validated = validated
                .with_full_scan_ratio(ratio)
                .expect("valid full_scan_ratio");
        }
        if let Some(ratio) = acorn_max_visible_ratio {
            validated = validated
                .with_acorn_max_visible_ratio(ratio)
                .expect("valid acorn_max_visible_ratio");
        }
        SearchEngineKind::Hnsw(validated)
    }

    enum Arm {
        BruteForce,
        Hnsw {
            name: &'static str,
            full_scan_ratio: Option<Ratio>,
            acorn_max_visible_ratio: Option<Ratio>,
        },
    }

    fn arms() -> Vec<Arm> {
        vec![
            Arm::BruteForce,
            Arm::Hnsw {
                name: "default",
                full_scan_ratio: None,
                acorn_max_visible_ratio: None,
            },
            Arm::Hnsw {
                name: "acorn_4_10",
                full_scan_ratio: None,
                acorn_max_visible_ratio: Some(Ratio {
                    numerator: 4,
                    denominator: 10,
                }),
            },
            Arm::Hnsw {
                name: "acorn_1_1",
                full_scan_ratio: None,
                acorn_max_visible_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 1,
                }),
            },
            Arm::Hnsw {
                name: "full_scan_2_5",
                full_scan_ratio: Some(Ratio {
                    numerator: 2,
                    denominator: 5,
                }),
                acorn_max_visible_ratio: None,
            },
            Arm::Hnsw {
                name: "force_plain",
                full_scan_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 1,
                }),
                acorn_max_visible_ratio: None,
            },
        ]
    }

    fn assert_no_tenant_b_leak(rows: &[engine::sql::exec::ResultRow]) {
        for row in rows {
            for (lo, hi) in TENANT_B_RANGES {
                assert!(
                    !(row.id >= lo && row.id <= hi),
                    "row {} falls inside a tenant-b (Private) id range and must not be \
                     visible under ctx tenant-a",
                    row.id
                );
            }
        }
    }

    /// A1・A2・A4: crossdb fixture（`docs25k.redb`）・`lang='ja'` を対象に、
    /// arm ごとの到達分類・Recall（既定エンジン対照。フィルタあり／なし）を
    /// 標準出力へ表として記録する。`#[ignore]`・release 専用（`make
    /// hnsw-crossdb-selectivity`）。
    #[test]
    #[ignore]
    fn layer_b_crossdb_docs25k_lang_ja_arm_report() {
        let dir = crossdb_dir();
        let redb_src = dir.join("docs25k.redb");
        assert!(
            redb_src.exists(),
            "expected {} to exist (run seed_docs first)",
            redb_src.display()
        );
        let queries = load_queries(&dir);
        assert!(!queries.is_empty(), "queries200.jsonl must not be empty");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // brute-force 対照（ground truth）を先に構築する。
        let (_ref_path, _ref_guard, ref_storage) = open_copy(&redb_src, "hnsw-crossdb-659-ref");
        ref_storage.create_table(&schema()).ok(); // 既存テーブルなら Err を無視。
        let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

        println!(
            "arm,k,ja_visible_count,subset,acorn,plain_ratio,mask_split,masked_short,\
             fallbacks,builds,entries,label,recall_filtered,recall_unfiltered"
        );

        // WHERE lang='ja' の可視件数（実測。fixture 選択率の裏取り）。
        let ja_count_sql = "SELECT id FROM docs WHERE lang = 'ja' LIMIT 10000";
        let ja_rows = ref_core
            .execute_sql(&ctx, ja_count_sql)
            .expect("scan lang='ja' (SQL-15)")
            .rows;
        assert_no_tenant_b_leak(&ja_rows);
        let ja_visible_count = ja_rows.len();

        for arm in arms() {
            // `_guard`（`CleanupGuard`）は `core` と寿命を共にする必要がある
            // （`Storage` が開いている間にファイルを消してよい前提を作らない
            // ため。`_guard` を match アーム内で束縛すると `core` より先に
            // drop され、以降のクエリ実行中にファイルが削除されうる）。
            let (name, core, _guard) = match &arm {
                Arm::BruteForce => {
                    // brute-force 自身は arm 表に含めない（ref_core が既に対照）。
                    continue;
                }
                Arm::Hnsw {
                    name,
                    full_scan_ratio,
                    acorn_max_visible_ratio,
                } => {
                    let (_path, guard, storage) =
                        open_copy(&redb_src, &format!("hnsw-crossdb-659-{name}"));
                    storage.create_table(&schema()).ok();
                    let kind = hnsw_kind_with(*full_scan_ratio, *acorn_max_visible_ratio);
                    let core = EngineCore::from_storage_with_engine(storage, kind);
                    (*name, core, guard)
                }
            };

            for &k in &[10usize, 200usize] {
                // warm-up: フィルタなしクエリで `FullVisible` 索引を構築する。
                let (warm_lang, warm_vec) = &queries[0];
                let _ = warm_lang;
                let warm_sql = format!(
                    "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT {k}",
                    vec_literal(warm_vec)
                );
                core.execute_sql(&ctx, &warm_sql).expect("warm-up query");
                let baseline = core.hnsw_index_cache_stats();
                assert_eq!(
                    baseline.entries, 1,
                    "warm-up must build exactly one entry (arm={name}, k={k})"
                );

                // arm 分類・カウンタ差分はフィルタ付きクエリだけの窓で取る
                // （フィルタなしクエリも同じ `hnsw_index_cache_stats()` を
                // 共有するため、同一ウィンドウに混ぜるとフィルタなし側の
                // `FullVisible` 経路〔可視比率 1.0 で `traversal_regime_for`
                // が常に条件を満たし `acorn_searches` 等を独立に加算しうる〕
                // が混入し、フィルタ付き経路の到達方式判定を汚染する）。
                let mut hits_filtered = 0usize;
                let mut total = 0usize;
                for (_lang, vec) in &queries {
                    let filtered_sql = format!(
                        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{}' LIMIT {k}",
                        vec_literal(vec)
                    );
                    let got_f = core
                        .execute_sql(&ctx, &filtered_sql)
                        .expect("filtered query")
                        .rows;
                    let want_f = ref_core
                        .execute_sql(&ctx, &filtered_sql)
                        .expect("filtered query (ref)")
                        .rows;
                    assert_no_tenant_b_leak(&got_f);
                    let want_f_ids: std::collections::HashSet<u64> =
                        want_f.iter().map(|r| r.id).collect();
                    hits_filtered += got_f.iter().filter(|r| want_f_ids.contains(&r.id)).count();
                    total += k;
                }

                let after = core.hnsw_index_cache_stats();
                let label = arm_label_delta!(baseline, after);
                let recall_filtered = hits_filtered as f64 / total as f64;

                // フィルタなし Recall@k（この呼び出しの LIMIT=k。CSV の recall_*
                // 列は arm,k と併記するため k=10/200 いずれも実質 Recall@k として
                // 読める）は判定規則（filtered >= unfiltered - 0.02）の比較対象
                // としてのみ使う。カウンタ差分には数えない
                // （上記コメント参照）ため `after` 取得の後に独立して回す。
                let mut hits_unfiltered = 0usize;
                for (_lang, vec) in &queries {
                    let unfiltered_sql = format!(
                        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT {k}",
                        vec_literal(vec)
                    );
                    let got_u = core
                        .execute_sql(&ctx, &unfiltered_sql)
                        .expect("unfiltered query")
                        .rows;
                    let want_u = ref_core
                        .execute_sql(&ctx, &unfiltered_sql)
                        .expect("unfiltered query (ref)")
                        .rows;
                    let want_u_ids: std::collections::HashSet<u64> =
                        want_u.iter().map(|r| r.id).collect();
                    hits_unfiltered += got_u.iter().filter(|r| want_u_ids.contains(&r.id)).count();
                }
                let recall_unfiltered = hits_unfiltered as f64 / total as f64;

                println!(
                    "{name},{k},{ja_visible_count},{},{},{},{},{},{},{},{},{},{:.4},{:.4}",
                    after
                        .subset_searches
                        .saturating_sub(baseline.subset_searches),
                    after.acorn_searches.saturating_sub(baseline.acorn_searches),
                    after.plain_scans.saturating_sub(baseline.plain_scans),
                    after
                        .mask_splits_graph
                        .saturating_sub(baseline.mask_splits_graph),
                    after.masked_short.saturating_sub(baseline.masked_short),
                    after.fallbacks.saturating_sub(baseline.fallbacks),
                    after.builds,
                    after.entries,
                    label,
                    recall_filtered,
                    recall_unfiltered,
                );

                // 非 vacuous: いずれかの Subset 系カウンタが動いていること。
                assert_ne!(
                    label, "n/a (brute_force engine or vacuous)",
                    "arm={name} k={k} must not be vacuous"
                );
                // 事前登録した判定規則（`docs/design/hnsw-rls-cardinality-switch.md`
                // 「Issue #659」節・本ファイル冒頭コメント参照）は arm 名ではなく
                // 到達分類（`label`）に応じて適用する。`force_plain` に限定すると
                // 他の 4 arm が実行時に到達方式を変えても（例えばフィクスチャ・
                // パラメータの変更で `ann_masked` へ到達するようになっても）
                // 判定規則が働かないまま vacuous に近い pass になり得るため
                // （PR #671 codex-review 指摘）。
                match label {
                    "ann_masked" | "ann_masked_two_hop" => {
                        // フィルタ付き ANN がフィルタなし ANN より悪化しないこと。
                        assert!(
                            recall_filtered >= recall_unfiltered - 0.02,
                            "arm={name} k={k} label={label}: filtered ANN recall must not \
                             regress vs unfiltered ANN recall by more than 0.02 \
                             (filtered={recall_filtered}, unfiltered={recall_unfiltered})"
                        );
                    }
                    "plain_scan_ratio" | "plain_scan_mask_split" | "plain_scan_masked_short" => {
                        // plain scan 縮退は既定エンジン（brute-force）と厳密一致すること。
                        assert_eq!(
                            recall_filtered, 1.0,
                            "arm={name} k={k} label={label}: plain scan fallback must match \
                             the default engine exactly (recall_filtered={recall_filtered})"
                        );
                    }
                    other => panic!(
                        "arm={name} k={k}: unexpected label {other:?} \
                         (not covered by the pre-registered judgement rules)"
                    ),
                }
            }
        }
    }
}
