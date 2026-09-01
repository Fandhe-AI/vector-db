//! 疎索引テーブル世代整合キャッシュ（Issue #357・`sql::sparse_cache::SparseIndexCache`）
//! 導入後の Recall 非劣化・RRF 同点順位規約の不変を SQL 表層（`EngineCore::
//! execute_sql`）経由で検証する回帰テスト（Issue #358）。
//!
//! **既存 Recall ゲートとの関係**: `tests/hybrid_recall.rs`・`tests/rerank_recall.rs`・
//! `tests/query_planning_recall.rs` は `SparseIndex::build` + `hybrid::hybrid_search`
//! を直接呼び出し、SQL 表層の `execute_statement`（`SparseIndexCache` が結線されている
//! 唯一の経路）を通らない。したがってこれら既存ゲートはキャッシュ導入に対して
//! 構造的に不変であり、それ自体はキャッシュ経路の非劣化を証明しない。本ファイルは
//! その空白を埋める SQL 表層専用の検証であり、既存ゲートを置き換えない
//! （`docs/design/sparse-index-cache-verification.md` 参照）。
//!
//! **検証の核心**: 「cold（クエリごとに `EngineCore` を再オープンしキャッシュを
//! 素通りさせる = キャッシュ導入前と同じ build-per-query 経路）」と「hot（単一
//! `EngineCore` でクエリを連続実行しキャッシュヒットさせる）」で Top-K の id 列が
//! 完全一致することを検証する。このとき `sparse_index_cache_stats()` で
//! hot 側が実際に `hits > 0` を記録していることも同時にアサートする——cold/hot の
//! 結果が一致するだけでは、キャッシュが一度も参照されず退化した比較（例えば
//! いずれも 0 件ヒット）でも通ってしまう vacuous pass になりうるため
//! （Issue #281 で回帰実測モデルの vacuous pass を問題視した教訓と同方針）。
//!
//! **フィクスチャの複製について**: `tests/hybrid_recall.rs` の決定的コーパス
//! 生成器（`Xorshift64`・トピック語彙・Zipf 分布）を共有 fixture へ切り出す案を
//! 検討したが、同ファイルの層 A 固定値アサーション（qa 60 件・hits20=182 等）が
//! 生成過程の 1 ビットの変化にも敏感な決定的乱数ストリームに依存しており、
//! 切り出しによる巻き添え変化のリスクがそれ自体の価値（コード重複の削減）を
//! 上回ると判断した。本ファイルは `Xorshift64`・トピック語彙のごく最小部分集合を
//! 独立に複製する（`tests/hybrid_recall.rs`・`query_planning_recall.rs` が
//! 既に同一実装を複製し合っている前例と同方針）。生成規則自体は簡略化しており
//! `hybrid_recall.rs` の Recall 実測値とは無関係（本ファイルの目的は Recall の
//! 絶対値ではなく cold/hot 等価性の検証）。
//!
//! 対応 spec ビヘイビア（ポインタのみ・本文非転記）: SEARCH-1, SEARCH-2, SEARCH-3。

use std::collections::BTreeSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（`tests/hybrid_recall.rs::Xorshift64` の複製。複製理由は
// 本ファイル冒頭のドキュメント参照） ----------

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_range(&mut self, n: usize) -> usize {
        assert!(n > 0, "next_range(0) は無効な呼び出し");
        (self.next_u64() % n as u64) as usize
    }
}

/// トピック `idx` のトークン（`sparse::tokenize` の ASCII 単語境界規則の下で必ず
/// 1 トークンになる形式。`hybrid_recall.rs::topic_token` と同一の命名規約だが、
/// 生成対象コーパスは互いに独立であり数値の互換性は不要）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

/// クエリ・本文文字列に連結する前に、生成器が作った識別子が想定形式
/// （`kw_` 接頭辞 + 4 桁数字）から逸脱していないことを検証する（SQL 文字列
/// 組み立てへ未検証入力を連結しない方針の防御的複製。本テストの生成源は
/// 生成器自身で untrusted ではないが、`benches/harness/hybrid_profile.rs::
/// validate_query_text` と同じ検証してから連結する作法を踏襲する）。
fn validate_identifier_token(token: &str) -> &str {
    let rest = token
        .strip_prefix("kw_")
        .unwrap_or_else(|| panic!("unexpected token format: {token}"));
    assert!(
        rest.len() == 4 && rest.chars().all(|c| c.is_ascii_digit()),
        "unexpected token format: {token}"
    );
    token
}

const FILLER_WORDS: [&str; 6] = ["the", "a", "of", "for", "and", "note"];

struct Doc {
    id: u64,
    text: String,
    vector: Vec<f32>,
    keywords: BTreeSet<usize>,
}

struct QaCase {
    query_text: String,
    query_vector: Vec<f32>,
    correct: BTreeSet<u64>,
}

fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

fn vector_literal(vec: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, x) in vec.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x:.1}"));
    }
    s.push(']');
    s
}

/// 決定的コーパス生成（`hybrid_recall.rs` の簡略版）。各文書へ 1〜2 個のトピック
/// キーワードを割り当て、疎チャネル（`text`）・密チャネル（`vector`）双方へ
/// 反映する（ドロップアウト・デコイは実装しない。本ファイルの目的は Recall の
/// 絶対値ではなく cold/hot 等価性の検証であるため、`hybrid_recall.rs` のような
/// 現実的な非対称分布は不要と判断した）。
fn generate_corpus(seed: u64, num_docs: usize, vocab_size: usize) -> Vec<Doc> {
    let mut rng = Xorshift64::new(seed);
    let mut docs = Vec::with_capacity(num_docs);
    for id in 1..=num_docs as u64 {
        let num_kw = 1 + rng.next_range(2);
        let mut keywords = BTreeSet::new();
        while keywords.len() < num_kw {
            keywords.insert(rng.next_range(vocab_size));
        }
        let mut text = String::new();
        for &kw in &keywords {
            let token = topic_token(kw);
            text.push_str(validate_identifier_token(&token));
            text.push(' ');
            text.push_str(validate_identifier_token(&token));
            text.push(' ');
        }
        for _ in 0..3 {
            text.push_str(FILLER_WORDS[rng.next_range(FILLER_WORDS.len())]);
            text.push(' ');
        }
        let vector = one_hot_sum(vocab_size, keywords.iter().copied());
        docs.push(Doc {
            id,
            text: text.trim_end().to_string(),
            vector,
            keywords,
        });
    }
    docs
}

/// AND 交差（2 語）で正解集合が空にならないクエリ対のみを採用する QA セット生成。
fn generate_qa_set(
    rng: &mut Xorshift64,
    docs: &[Doc],
    vocab_size: usize,
    num_queries: usize,
) -> Vec<QaCase> {
    let mut qa = Vec::with_capacity(num_queries);
    let mut attempts = 0usize;
    while qa.len() < num_queries && attempts < num_queries * 50 {
        attempts += 1;
        let a = rng.next_range(vocab_size);
        let b = rng.next_range(vocab_size);
        if a == b {
            continue;
        }
        let correct: BTreeSet<u64> = docs
            .iter()
            .filter(|d| d.keywords.contains(&a) && d.keywords.contains(&b))
            .map(|d| d.id)
            .collect();
        if correct.is_empty() {
            continue;
        }
        let query_text = format!(
            "{} {}",
            validate_identifier_token(&topic_token(a)),
            validate_identifier_token(&topic_token(b)),
        );
        let query_vector = one_hot_sum(vocab_size, [a, b]);
        qa.push(QaCase {
            query_text,
            query_vector,
            correct,
        });
    }
    qa
}

fn create_docs_table(storage: &Storage, vocab_size: usize) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(vocab_size as u32), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
}

fn insert_corpus(storage: &Storage, tenant: &str, docs: &[Doc]) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for doc in docs {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            doc.id,
            Visibility::Public,
            &[
                Value::Vector(doc.vector.clone()),
                Value::Text(doc.text.clone()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "sparse-cache-recall-seed-{tenant}-{}",
                doc.id
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

fn hybrid_sql(qa: &QaCase, limit: usize) -> String {
    format!(
        "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '{}', body, '{}') LIMIT {limit}",
        vector_literal(&qa.query_vector),
        qa.query_text,
    )
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::with_provider(path, Box::new(CpuScalarProvider)).expect("open core")
}

// --- 2.1: cold（クエリごと再オープン）/ hot（単一 EngineCore 連続実行）の完全一致 ---

const SMALL_NUM_DOCS: usize = 400;
const SMALL_VOCAB_SIZE: usize = 60;
const SMALL_NUM_QUERIES: usize = 40;
const SMALL_SEED: u64 = 0x5350_4152_5345_4331;

#[test]
fn cold_and_hot_hybrid_results_are_identical_and_cache_is_actually_hit() {
    let path = unique_db_path("sparse-cache-recall-small");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage, SMALL_VOCAB_SIZE);

    let docs = generate_corpus(SMALL_SEED, SMALL_NUM_DOCS, SMALL_VOCAB_SIZE);
    let mut qa_rng = Xorshift64::new(SMALL_SEED ^ 0xA5A5_A5A5_A5A5_A5A5);
    let qa = generate_qa_set(&mut qa_rng, &docs, SMALL_VOCAB_SIZE, SMALL_NUM_QUERIES);
    assert!(
        qa.len() >= SMALL_NUM_QUERIES / 2,
        "QA generation must find enough non-empty AND queries, got {}",
        qa.len()
    );
    insert_corpus(&storage, "tenant-recall", &docs);
    drop(storage);

    // --- cold: クエリごとに EngineCore を再オープンし、キャッシュを毎回素通りさせる
    // （#377 導入前と同一の build-per-query 経路の代替）。
    let mut cold_ids = Vec::with_capacity(qa.len());
    for case in &qa {
        let core = open_core(&path);
        let ctx = PolicyContext::new("tenant-recall").expect("valid tenant");
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("cold hybrid query ok");
        let stats = core.sparse_index_cache_stats();
        assert_eq!(stats.misses, 1, "cold query must be a fresh-cache miss");
        assert_eq!(stats.hits, 0, "cold query must not hit a stale cache");
        cold_ids.push(result_ids(&result));
    }

    // --- hot: 単一 EngineCore で全 QA を連続実行し、2 件目以降はキャッシュヒットする。
    let core = open_core(&path);
    let ctx = PolicyContext::new("tenant-recall").expect("valid tenant");
    let mut hot_ids = Vec::with_capacity(qa.len());
    let mut hot_hits20 = 0usize;
    for case in &qa {
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("hot hybrid query ok");
        let ids = result_ids(&result);
        hot_hits20 += ids.iter().filter(|id| case.correct.contains(id)).count();
        hot_ids.push(ids);
    }
    let final_stats = core.sparse_index_cache_stats();
    assert_eq!(
        final_stats.misses, 1,
        "hot run must build the sparse index exactly once"
    );
    assert_eq!(
        final_stats.hits,
        (qa.len() - 1) as u64,
        "hot run's queries after the first must all be cache hits \
         (a 0-hit run would make cold==hot a vacuous pass)"
    );
    assert_eq!(final_stats.stale_evictions, 0);

    // --- cold と hot の Top-20 id 列が全ケースで順序込み完全一致すること。
    for (i, (cold, hot)) in cold_ids.iter().zip(hot_ids.iter()).enumerate() {
        assert_eq!(cold, hot, "case {i} diverged between cold and hot runs");
    }

    // 非劣化の回帰トラッキング（実測して固定。生成規則を変えない限り決定的）。
    // hits20 の絶対値自体は本ファイル独自のフィクスチャに基づくものであり、
    // `hybrid_recall.rs`（TASK-104 層 A/B）の Recall 実測値とは対応しない。
    println!(
        "=== sparse_cache_recall small-scale: qa={} hot_hits20={hot_hits20} ===",
        qa.len()
    );
    // 実測して固定した回帰値（qa=40 件・SMALL_SEED 固定の下で決定的）。生成規則・
    // 検索カーネルの変更で値が動いた場合は意図した変化か確認する。
    assert_eq!(
        hot_hits20,
        46,
        "hot run hits20 regressed from the recorded baseline (qa={})",
        qa.len()
    );
}

// --- 2.2: RRF 同点順位規約（`TieRank::GroupEnd`・境界同点グループ完全化）が
// キャッシュヒット経路でも不変であること。`tests/sql_surface.rs::
// sql4_hybrid_tie_group_across_limit_boundary_is_deterministic` と同じ同点誘発
// コーパスを、cold（再オープン）/ hot（キャッシュヒット）双方で検証する。

#[test]
fn cold_and_hot_hybrid_tie_group_across_limit_boundary_match() {
    let path = unique_db_path("sparse-cache-recall-tie");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    // `sql_surface.rs::sql4_hybrid_tie_group_across_limit_boundary_is_deterministic`
    // と同一の同点誘発コーパス（密ランク・疎ランクを入れ替えた 2 行が完全同点になる
    // 構成。同ファイルのコメント参照）。
    let rows: [(u64, [f32; 2], Option<&str>); 5] = [
        (1, [1.0, 0.0], Some("anchor filler filler")),
        (2, [0.9, 0.0], Some("anchor anchor anchor")),
        (90, [0.1, 0.0], None),
        (91, [0.05, 0.0], None),
        (92, [0.01, 0.0], None),
    ];
    let ctx_seed =
        PolicyContext::with_visibilities("tenant-tie", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, emb, body) in rows {
        let value = match body {
            Some(b) => Value::Text(b.to_string()),
            None => Value::Null,
        };
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx_seed,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), value],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "sparse-cache-recall-tie-{id}"
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    drop(storage);

    let sql_for = |limit: usize, kw_syntax: bool| -> String {
        let func = if kw_syntax { "HYBRID" } else { "hybrid_rrf" };
        format!(
            "SELECT * FROM docs ORDER BY {func}(embedding, '[1.0,0.0]', body, 'anchor') LIMIT {limit}"
        )
    };

    // cold: LIMIT 1 / 2 / 3 それぞれを新規 EngineCore（キャッシュ未ヒット）で実行。
    let mut cold_by_limit = Vec::new();
    for limit in [1usize, 2, 3] {
        let core = open_core(&path);
        let ctx = PolicyContext::new("tenant-tie").expect("valid tenant");
        let result = core
            .execute_sql(&ctx, &sql_for(limit, false))
            .expect("cold hybrid_rrf tie query ok");
        let stats = core.sparse_index_cache_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);
        cold_by_limit.push(result_ids(&result));
    }

    // hot: 単一 EngineCore で LIMIT 1 → 2 → 3 → HYBRID(...) 構文形の順に連続実行。
    let core = open_core(&path);
    let ctx = PolicyContext::new("tenant-tie").expect("valid tenant");
    let mut hot_by_limit = Vec::new();
    for (i, limit) in [1usize, 2, 3].into_iter().enumerate() {
        let result = core
            .execute_sql(&ctx, &sql_for(limit, false))
            .expect("hot hybrid_rrf tie query ok");
        let stats = core.sparse_index_cache_stats();
        if i == 0 {
            assert_eq!(stats.misses, 1);
            assert_eq!(stats.hits, 0);
        } else {
            assert_eq!(stats.misses, 1);
            assert_eq!(stats.hits, i as u64, "case {i} must be a cache hit");
        }
        hot_by_limit.push(result_ids(&result));
    }
    let result_kw = core
        .execute_sql(&ctx, &sql_for(1, true))
        .expect("hot HYBRID() tie query ok");
    let final_stats = core.sparse_index_cache_stats();
    assert_eq!(final_stats.hits, 3, "HYBRID() form must also hit the cache");
    assert_eq!(final_stats.stale_evictions, 0);

    for (i, (cold, hot)) in cold_by_limit.iter().zip(hot_by_limit.iter()).enumerate() {
        assert_eq!(
            cold, hot,
            "LIMIT case {i} diverged between cold and hot runs"
        );
    }
    assert_eq!(
        cold_by_limit[0],
        vec![1u64],
        "LIMIT 1 cuts the tie group after id=1"
    );
    assert_eq!(
        cold_by_limit[1],
        vec![1u64, 2u64],
        "LIMIT 2 must include the full tie group in id order"
    );
    assert_eq!(result_ids(&result_kw), hot_by_limit[0]);
}

// --- 大規模段: 数万件規模での cold/hot 等価性（Issue #358 検証設計 2.1 の大規模段。
// `cargo test`（debug）での所要時間が長いため既定では実行しない。
// `make sparse-cache-recall-large` で実行する（`bench-hybrid-profile` と同じ
// opt-in 方針）。

const LARGE_NUM_DOCS: usize = 20_000;
const LARGE_VOCAB_SIZE: usize = 800;
const LARGE_NUM_QUERIES: usize = 30;
const LARGE_SEED: u64 = 0x4C41_5247_4531_3031;

#[test]
#[ignore = "数万件規模のためデフォルト実行対象外。make sparse-cache-recall-large で実行する"]
fn cold_and_hot_hybrid_results_are_identical_large_scale() {
    let path = unique_db_path("sparse-cache-recall-large");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage, LARGE_VOCAB_SIZE);

    let docs = generate_corpus(LARGE_SEED, LARGE_NUM_DOCS, LARGE_VOCAB_SIZE);
    let mut qa_rng = Xorshift64::new(LARGE_SEED ^ 0xA5A5_A5A5_A5A5_A5A5);
    let qa = generate_qa_set(&mut qa_rng, &docs, LARGE_VOCAB_SIZE, LARGE_NUM_QUERIES);
    assert!(
        !qa.is_empty(),
        "large-scale QA generation must find non-empty AND queries"
    );
    insert_corpus(&storage, "tenant-recall-large", &docs);
    drop(storage);

    let mut cold_ids = Vec::with_capacity(qa.len());
    for case in &qa {
        let core = open_core(&path);
        let ctx = PolicyContext::new("tenant-recall-large").expect("valid tenant");
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("cold hybrid query ok");
        cold_ids.push(result_ids(&result));
    }

    let core = open_core(&path);
    let ctx = PolicyContext::new("tenant-recall-large").expect("valid tenant");
    let mut hot_ids = Vec::with_capacity(qa.len());
    for case in &qa {
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("hot hybrid query ok");
        hot_ids.push(result_ids(&result));
    }
    let final_stats = core.sparse_index_cache_stats();
    assert_eq!(final_stats.misses, 1);
    assert_eq!(final_stats.hits, (qa.len() - 1) as u64);

    for (i, (cold, hot)) in cold_ids.iter().zip(hot_ids.iter()).enumerate() {
        assert_eq!(
            cold, hot,
            "large-scale case {i} diverged between cold and hot runs"
        );
    }
}
