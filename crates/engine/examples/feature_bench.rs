//! engine の一通りの機能（SQL 表層・ベクトル検索・RLS）を通しで実行し、
//! レイテンシとリソース利用状況を計測して JSON を stdout へ出力するベンチマーク。
//!
//! `cargo run --release -p engine --example feature_bench` で実行する。依存追加は
//! せず std のみで計測する（`/proc/self/status`・`/proc/self/stat` を読む簡易実装。
//! Linux 以外では該当欄が 0 のまま出力される）。
//!
//! 計測対象は `docs/spec` の個別ビヘイビア ID には対応づけない（横断的な性能観測用の
//! 補助ツールであり、`tests/*.rs` の受け入れ基準テストとは独立）。SQL 構文・API の
//! 呼び出し方は各結合テスト（`tests/sql_aggregate.rs`・`tests/sql_group_by.rs`・
//! `tests/sql_surface.rs`・`tests/sql_search_mode.rs`・`tests/sql_udf_call.rs`）を
//! 参照して揃えてある。UDF 呼び出し（TASK-79）フェーズも含む。
//!
//! Issue #344/#355 の Recall/性能受け入れ基準の実測（`hybrid_rrf` 288.6ms 等）を
//! 出した計測 example。以前は git 追跡外だったため before/after の再現性が無く
//! （PR #374 の ADR 参照）、Issue #358 で `crates/engine/examples/` へ追跡化した。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{Embedder, HashingEmbedder};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use std::hint::black_box;
use std::time::Instant;

const DIM: u32 = 128;
const ROWS_A: u64 = 20_000;
const ROWS_B: u64 = 5_000;
/// tenant-a 投入時に Private 可視性を割り当てる周期（[`run_ingest`] の
/// `make_batch` 呼び出しに使う）。RLS 境界確認（[`run_rls_isolation_phase`]）が
/// tenant-a の期待 Public/Private 件数をこの定数から導出するため、投入側と
/// 検証側で値がずれないよう単一の定数として共有する。
const TENANT_A_PRIVATE_EVERY_N: u64 = 10;
const BATCH_SIZE: u64 = 1_000;
const WARMUP: usize = 5;
const ITERS: usize = 50;

const LANGS: &[&str] = &["ja", "en", "fr", "de", "es"];
const TOPICS: &[&str] = &[
    "topic-00", "topic-01", "topic-02", "topic-03", "topic-04", "topic-05", "topic-06", "topic-07",
    "topic-08", "topic-09", "topic-10", "topic-11", "topic-12", "topic-13", "topic-14", "topic-15",
    "topic-16", "topic-17", "topic-18", "topic-19",
];
const WORDS: &[&str] = &[
    "vector",
    "index",
    "search",
    "query",
    "engine",
    "tenant",
    "policy",
    "cache",
    "kernel",
    "embedding",
    "storage",
    "recall",
    "precision",
    "hybrid",
    "rerank",
    "fusion",
    "latency",
    "throughput",
    "cluster",
    "shard",
    "replica",
    "commit",
    "transaction",
    "ledger",
    "operation",
    "dictionary",
    "planner",
    "boost",
    "score",
    "rank",
];
const QUERY_TEXTS: &[&str] = &[
    "vector search recall precision",
    "tenant policy cache kernel",
    "hybrid rerank fusion latency",
    "storage commit transaction ledger",
    "dictionary planner boost score",
];

/// `/proc/self/status`・`/proc/self/stat` から読んだリソース使用量のスナップショット。
struct ProcStats {
    vm_rss_kb: u64,
    vm_hwm_kb: u64,
    cpu_ticks: u64,
}

/// 1 フェーズ分の計測結果（レイテンシ分布＋計測後のリソーススナップショット＋
/// フェーズ固有の付帯情報。`extra` は呼び出し元が組み立て済みの JSON オブジェクト
/// 本体の断片で、安全な既知キー・数値・固定文字列のみで構成する）。
struct PhaseStat {
    name: &'static str,
    iterations: usize,
    min_us: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    mean_us: f64,
    rss_kb_after: u64,
    cpu_tick_delta: u64,
    extra: String,
}

/// `rest`（`"VmRSS:    1234 kB"` の `"VmRSS:"` を取り除いた残り）から kB 数値を読む。
/// 値が読めない場合は untrusted 環境差として 0 を返す（本ツールは診断目的であり、
/// 計測失敗でベンチ全体を止めない）。
fn parse_kb(rest: &str) -> u64 {
    rest.split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// `/proc/self/stat` から utime（14 番目）＋stime（15 番目）フィールドの合計 tick 数を
/// 読む。`comm`（2 番目のフィールド）は括弧で囲まれ空白を含みうるため、最後の `)` の
/// 後ろから空白区切りで数える（`man proc` の慣行どおり）。
fn parse_cpu_ticks(stat: &str) -> u64 {
    let Some(paren_end) = stat.rfind(')') else {
        return 0;
    };
    let fields: Vec<&str> = stat[paren_end + 1..].split_whitespace().collect();
    // `paren_end` 直後は 3 番目のフィールド（state）から始まる。utime は 14 番目
    // （このスライスでの index 11）、stime は 15 番目（index 12）。
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    utime.saturating_add(stime)
}

/// 現プロセスのメモリ・CPU 使用量を読む（Linux 専用。読めない環境では全欄 0）。
fn read_proc_stats() -> ProcStats {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut vm_rss_kb = 0u64;
    let mut vm_hwm_kb = 0u64;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            vm_rss_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            vm_hwm_kb = parse_kb(rest);
        }
    }
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let cpu_ticks = parse_cpu_ticks(&stat);
    ProcStats {
        vm_rss_kb,
        vm_hwm_kb,
        cpu_ticks,
    }
}

/// マイクロ秒サンプル列から (min, p50, p95, p99, max, mean) を計算する。
fn percentiles(mut samples: Vec<u64>) -> (u64, u64, u64, u64, u64, f64) {
    if samples.is_empty() {
        return (0, 0, 0, 0, 0, 0.0);
    }
    samples.sort_unstable();
    let n = samples.len();
    let min = samples[0];
    let max = samples[n - 1];
    let sum: u64 = samples.iter().sum();
    let mean = sum as f64 / n as f64;
    let pick = |q: f64| -> u64 {
        let idx = ((q * (n as f64 - 1.0)).round() as usize).min(n - 1);
        samples[idx]
    };
    (min, pick(0.50), pick(0.95), pick(0.99), max, mean)
}

/// `warmup` 回のウォームアップ後、`iters` 回のレイテンシ（マイクロ秒）を計測する。
/// `f` の戻り値は `black_box` で消費し、コンパイラによる呼び出し省略を避ける。
fn measure_us<F: FnMut() -> R, R>(warmup: usize, iters: usize, mut f: F) -> Vec<u64> {
    for _ in 0..warmup {
        black_box(f());
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        let r = f();
        let elapsed = start.elapsed();
        black_box(r);
        samples.push(elapsed.as_micros() as u64);
    }
    samples
}

/// レイテンシサンプル列とリソーススナップショット差分から [`PhaseStat`] を組み立てる。
fn build_phase(
    name: &'static str,
    samples: Vec<u64>,
    rss_kb_after: u64,
    cpu_tick_delta: u64,
    extra: String,
) -> PhaseStat {
    let iterations = samples.len();
    let (min_us, p50_us, p95_us, p99_us, max_us, mean_us) = percentiles(samples);
    PhaseStat {
        name,
        iterations,
        min_us,
        p50_us,
        p95_us,
        p99_us,
        max_us,
        mean_us,
        rss_kb_after,
        cpu_tick_delta,
        extra,
    }
}

fn phase_to_json(p: &PhaseStat) -> String {
    format!(
        "{{\"name\":\"{}\",\"iterations\":{},\"min_us\":{},\"p50_us\":{},\"p95_us\":{},\
         \"p99_us\":{},\"max_us\":{},\"mean_us\":{:.1},\"rss_kb_after\":{},\
         \"cpu_tick_delta\":{},\"extra\":{{{}}}}}",
        p.name,
        p.iterations,
        p.min_us,
        p.p50_us,
        p.p95_us,
        p.p99_us,
        p.max_us,
        p.mean_us,
        p.rss_kb_after,
        p.cpu_tick_delta,
        p.extra,
    )
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:.6}")).collect();
    format!("[{}]", parts.join(","))
}

/// 決定的な合成本文を生成する（外部 RNG 依存なし。`id`・`lang_idx`・`topic_idx` から
/// `WORDS` を周回選択して数十語のダミーテキストを組み立てる）。
fn make_body(id: u64, lang_idx: usize, topic_idx: usize) -> String {
    let mut s = String::new();
    for w in 0..40usize {
        let idx = (id as usize)
            .wrapping_mul(31)
            .wrapping_add(w * 7)
            .wrapping_add(lang_idx)
            .wrapping_add(topic_idx)
            % WORDS.len();
        s.push_str(WORDS[idx]);
        s.push(' ');
    }
    s.push_str(&format!("doc-{id}"));
    s
}

/// `[start_id, end_id]` 範囲の 1 バッチ分の投入データを構築する。`private_every_n`
/// が `Some(n)` の場合、`id % n == 0` の行を `Visibility::Private` にする
/// （ロング説明の「tenant-a の 10% は Private」を `n = 10` で表現する）。
///
/// 戻り値は (ids, visibilities, embeddings, encoded scalar columns) の 4 本の
/// 並行配列。呼び出し元（[`insert_batch`]）が同じ index で zip して `RowInput` を
/// 組み立てる。
type BatchData = (Vec<u64>, Vec<Visibility>, Vec<Vec<f32>>, Vec<Vec<u8>>);

fn make_batch(
    schema: &TableSchema,
    embedder: &HashingEmbedder,
    start_id: u64,
    end_id: u64,
    private_every_n: Option<u64>,
) -> BatchData {
    let mut ids = Vec::new();
    let mut viss = Vec::new();
    let mut bodies = Vec::new();
    let mut langs = Vec::new();
    let mut topics = Vec::new();
    for id in start_id..=end_id {
        let lang_idx = (id as usize) % LANGS.len();
        let topic_idx = (id as usize) % TOPICS.len();
        let visibility = match private_every_n {
            Some(n) if n > 0 && id % n == 0 => Visibility::Private,
            _ => Visibility::Public,
        };
        ids.push(id);
        viss.push(visibility);
        langs.push(LANGS[lang_idx].to_string());
        topics.push(TOPICS[topic_idx].to_string());
        bodies.push(make_body(id, lang_idx, topic_idx));
    }
    let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let embs = embedder
        .embed_batch(&refs)
        .expect("embed_batch for ingest batch");
    let encoded: Vec<Vec<u8>> = langs
        .iter()
        .zip(topics.iter())
        .zip(bodies.iter())
        .zip(embs.iter())
        .map(|(((lang, topic), body), emb)| {
            encode_scalar_columns(
                schema,
                &[
                    Value::Vector(emb.clone()),
                    Value::Text(lang.clone()),
                    Value::Text(topic.clone()),
                    Value::Text(body.clone()),
                ],
            )
            .expect("encode_scalar_columns for ingest batch")
        })
        .collect();
    (ids, viss, embs, encoded)
}

/// 1 バッチを `insert_rows` で投入し、そのレイテンシ（マイクロ秒）を返す。
fn insert_batch(
    storage: &Storage,
    ctx: &PolicyContext,
    ids: &[u64],
    viss: &[Visibility],
    embs: &[Vec<f32>],
    encoded: &[Vec<u8>],
    op_label: &str,
) -> u64 {
    let inputs: Vec<(u64, RowInput<'_>)> = ids
        .iter()
        .zip(viss.iter())
        .zip(embs.iter())
        .zip(encoded.iter())
        .map(|(((id, vis), emb), meta)| {
            (
                *id,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: *vis,
                    embedding: emb,
                    metadata: meta,
                },
            )
        })
        .collect();
    let op = OperationId::parse(op_label).expect("valid operation id label");
    let start = Instant::now();
    engine::tenant::insert_rows(storage, "docs", ctx, &inputs, &op).expect("insert_rows");
    start.elapsed().as_micros() as u64
}

/// フェーズ 1: `insert_rows` によるバッチ投入（ROWS_A 行を tenant-a・10% Private、
/// ROWS_B 行を tenant-b・全 Public）。バッチ毎レイテンシとスループットを計測する。
fn run_ingest(
    storage: &Storage,
    schema: &TableSchema,
    embedder: &HashingEmbedder,
    ctx_a: &PolicyContext,
    ctx_b: &PolicyContext,
) -> PhaseStat {
    let cpu_before = read_proc_stats().cpu_ticks;
    let mut samples = Vec::new();
    let mut total_rows: u64 = 0;
    let mut batch_no: u64 = 0;

    let mut id = 1u64;
    while id <= ROWS_A {
        let end = (id + BATCH_SIZE - 1).min(ROWS_A);
        let (ids, viss, embs, encoded) =
            make_batch(schema, embedder, id, end, Some(TENANT_A_PRIVATE_EVERY_N));
        let us = insert_batch(
            storage,
            ctx_a,
            &ids,
            &viss,
            &embs,
            &encoded,
            &format!("feature-bench-a-{batch_no}"),
        );
        samples.push(us);
        total_rows += ids.len() as u64;
        batch_no += 1;
        id = end + 1;
    }

    let mut id = ROWS_A + 1;
    let end_b = ROWS_A + ROWS_B;
    while id <= end_b {
        let end = (id + BATCH_SIZE - 1).min(end_b);
        let (ids, viss, embs, encoded) = make_batch(schema, embedder, id, end, None);
        let us = insert_batch(
            storage,
            ctx_b,
            &ids,
            &viss,
            &embs,
            &encoded,
            &format!("feature-bench-b-{batch_no}"),
        );
        samples.push(us);
        total_rows += ids.len() as u64;
        batch_no += 1;
        id = end + 1;
    }

    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    let total_us: u64 = samples.iter().sum();
    let rows_per_sec = if total_us > 0 {
        total_rows as f64 / (total_us as f64 / 1_000_000.0)
    } else {
        0.0
    };
    let extra = format!(
        "\"rows_total\":{total_rows},\"batches\":{},\"rows_per_sec\":{rows_per_sec:.1}",
        samples.len()
    );
    build_phase("ingest", samples, after.vm_rss_kb, cpu_delta, extra)
}

/// `execute_sql`（セッション不要の SELECT/集計）を warmup+計測ループで実行する
/// 汎用フェーズヘルパー。事前の 1 回の「プローブ」呼び出しで正当性・行数を確認し、
/// `extra` へ記録する。
fn run_select_phase(
    name: &'static str,
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> PhaseStat {
    let probe = core.execute_sql(ctx, sql);
    let extra = match &probe {
        Ok(r) => format!("\"ok\":true,\"rows\":{}", r.rows.len()),
        Err(_) => "\"ok\":false".to_string(),
    };
    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || core.execute_sql(ctx, sql));
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase(name, samples, after.vm_rss_kb, cpu_delta, extra)
}

/// `USING MODE '<mode>'` 付きクエリのフェーズ。`precision` は確信度ゲートにより
/// 空集合を返しうる契約（エラーではない）ため、成功・失敗いずれでもベンチ全体を
/// 止めず `extra` へ結果を記録する。
fn run_mode_phase(
    name: &'static str,
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> PhaseStat {
    let mut probe_session = SessionState::default();
    let probe = core.execute_sql_in_session(ctx, &mut probe_session, sql);
    let extra = match &probe {
        Ok(SqlOutcome::Query(r)) => format!("\"ok\":true,\"rows\":{}", r.rows.len()),
        Ok(_) => "\"ok\":true,\"note\":\"unexpected non-query outcome\"".to_string(),
        Err(_) => "\"ok\":false,\"note\":\"mode query returned an error\"".to_string(),
    };
    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || {
        let mut session = SessionState::default();
        core.execute_sql_in_session(ctx, &mut session, sql)
    });
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase(name, samples, after.vm_rss_kb, cpu_delta, extra)
}

/// UDF 呼び出しフェーズ（TASK-79・SQL-9）。`CREATE FUNCTION` はセッションへの
/// 登録のため計測対象外（1 回だけセットアップし、以降はそのセッションを使い回して
/// `SELECT ... norm_scale(embedding, 2.0) ...` を計測する）。
fn run_udf_phase(core: &EngineCore, ctx: &PolicyContext, query_vec_literal: &str) -> PhaseStat {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        ctx,
        &mut session,
        "CREATE FUNCTION norm_scale(v, s) AS s * vec_sum(vec_div(v, vec_norm(v)))",
    )
    .expect("CREATE FUNCTION should succeed");
    let sql = format!(
        "SELECT id, norm_scale(embedding, 2.0) AS score FROM docs \
         ORDER BY embedding <=> '{query_vec_literal}' LIMIT 10"
    );
    let probe = core.execute_sql_in_session(ctx, &mut session, &sql);
    let extra = match &probe {
        Ok(SqlOutcome::Query(r)) => format!("\"ok\":true,\"rows\":{}", r.rows.len()),
        Ok(_) => "\"ok\":true,\"note\":\"unexpected non-query outcome\"".to_string(),
        Err(_) => "\"ok\":false".to_string(),
    };
    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || {
        core.execute_sql_in_session(ctx, &mut session, &sql)
    });
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase("udf_call", samples, after.vm_rss_kb, cpu_delta, extra)
}

fn count_of(result: &QueryResult) -> Option<u64> {
    let row = result.rows.first()?;
    match row.cells.first()? {
        engine::sql::exec::Cell::Integer(n) => Some(*n),
        _ => None,
    }
}

/// フェーズ 11: RLS テナント境界の確認。tenant-a（Public のみ／Public+Private）・
/// tenant-b（Public のみ）それぞれから見える件数を、[`run_ingest`] が投入した
/// fixture の期待値（tenant-a: Public `ROWS_A - ROWS_A / TENANT_A_PRIVATE_EVERY_N`・
/// full `ROWS_A`、tenant-b: Public `ROWS_B`）と個別に照合し、他テナントの行が
/// 混入していない（=期待値どおりの件数しか見えない）ことを確認する。
/// tenant-a・tenant-b の Public 件数は 18,000 対 5,000 と非対称であり単純な相互
/// 一致比較では正しい分離時にも false になるため使わない（Issue #358 PR #380
/// codex-review 指摘）。計測対象クエリは tenant-b 視点の `COUNT(*)`。
fn run_rls_isolation_phase(
    core: &EngineCore,
    ctx_a_public: &PolicyContext,
    ctx_a_full: &PolicyContext,
    ctx_b: &PolicyContext,
) -> PhaseStat {
    let sql = "SELECT COUNT(*) FROM docs";
    let count_a_public = core
        .execute_sql(ctx_a_public, sql)
        .ok()
        .and_then(|r| count_of(&r));
    let count_a_full = core
        .execute_sql(ctx_a_full, sql)
        .ok()
        .and_then(|r| count_of(&r));
    let count_b = core.execute_sql(ctx_b, sql).ok().and_then(|r| count_of(&r));

    // fixture（run_ingest）の期待件数。tenant-a は ROWS_A 行中
    // id % TENANT_A_PRIVATE_EVERY_N == 0 のみ Private（make_batch 参照）。
    let expected_private_a = ROWS_A / TENANT_A_PRIVATE_EVERY_N;
    let expected_public_a = ROWS_A - expected_private_a;

    // 各コンテキストの件数が fixture の期待値どおりであれば、他テナントの行の
    // 混入（漏えい）も自テナント内 Private/Public の取り違えも起きていない
    // （RLS-7/RLS-8 相当の境界確認。数値基準そのものは本ベンチの管轄外）。
    let no_leak = count_a_public == Some(expected_public_a)
        && count_a_full == Some(ROWS_A)
        && count_b == Some(ROWS_B);
    let private_rows_a = match (count_a_full, count_a_public) {
        (Some(full), Some(public)) => full.saturating_sub(public),
        _ => 0,
    };
    let extra = format!(
        "\"count_tenant_a_public\":{},\"count_tenant_a_full\":{},\"count_tenant_b_public\":{},\
         \"private_rows_tenant_a\":{private_rows_a},\"no_cross_tenant_leak\":{}",
        count_a_public.unwrap_or(0),
        count_a_full.unwrap_or(0),
        count_b.unwrap_or(0),
        no_leak,
    );

    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || core.execute_sql(ctx_b, sql));
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase("rls_isolation", samples, after.vm_rss_kb, cpu_delta, extra)
}

fn db_file_size_bytes(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn main() {
    let mut db_path = std::env::temp_dir();
    db_path.push(format!(
        "vector-db-feature-bench-{}-{:x}.redb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    ));
    // プロセス終了時に一時 DB ファイルを削除する RAII ガード。
    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = CleanupGuard(db_path.clone());

    let embedder = HashingEmbedder::new(DIM).expect("valid embedder dim");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("topic", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );

    let storage = Storage::open(&db_path).expect("open storage");
    storage.create_table(&schema).expect("create table");

    let ctx_a_public = PolicyContext::new("tenant-a").expect("valid tenant ctx");
    let ctx_a_full =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant ctx");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant ctx");

    let mut phases: Vec<PhaseStat> = vec![run_ingest(
        &storage,
        &schema,
        &embedder,
        &ctx_a_public,
        &ctx_b,
    )];
    let rows_after_ingest_bytes = db_file_size_bytes(&db_path);

    // `insert_rows` で書き込み済みの `Storage` を `EngineCore` へ引き渡す
    // （`EngineCore::open` による同一ファイルへの二重オープンを避ける。
    // `from_storage` は所有権移動のみでテナント境界の迂回経路を新設しない）。
    let core = EngineCore::from_storage(storage, engine::search_engine::default_engine());

    // クエリベクトル・クエリ文の周回プール。
    let query_vecs: Vec<Vec<f32>> = QUERY_TEXTS
        .iter()
        .map(|t| {
            embedder
                .embed_batch(&[t])
                .expect("embed query text")
                .remove(0)
        })
        .collect();
    let qv0 = vec_literal(&query_vecs[0]);
    let qv1 = vec_literal(&query_vecs[1 % query_vecs.len()]);
    let qv2 = vec_literal(&query_vecs[2 % query_vecs.len()]);

    // 許可リスト構文（`sql::allowlist`）は非集計 `SELECT` に `ORDER BY` を必須と
    // するため（`LIMIT` 単体では受理されない）、`WHERE` フィルタの効果を見るために
    // `ORDER BY embedding <=> '<vec>'` を併記する（`tests/sql_surface.rs` の
    // `WHERE lang = 'ja' ORDER BY ... LIMIT ...` と同じ形）。
    phases.push(run_select_phase(
        "point_where",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{qv0}' LIMIT 10"),
    ));
    phases.push(run_select_phase(
        "where_compound",
        &core,
        &ctx_a_full,
        "SELECT COUNT(*) FROM docs WHERE visible() AND id > 100 AND lang = 'ja'",
    ));
    phases.push(run_select_phase(
        "agg_count",
        &core,
        &ctx_a_full,
        "SELECT COUNT(*) FROM docs",
    ));
    phases.push(run_select_phase(
        "agg_multi",
        &core,
        &ctx_a_full,
        "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs",
    ));
    phases.push(run_select_phase(
        "group_by_having",
        &core,
        &ctx_a_full,
        "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n > 1 ORDER BY n DESC LIMIT 5",
    ));
    phases.push(run_select_phase(
        "vector_knn",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs ORDER BY embedding <=> '{qv0}' LIMIT 10"),
    ));
    phases.push(run_select_phase(
        "vector_knn_where",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{qv1}' LIMIT 10"),
    ));
    phases.push(run_select_phase(
        "hybrid_rrf",
        &core,
        &ctx_a_full,
        &format!(
            "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{qv2}', body, '{}') LIMIT 10",
            QUERY_TEXTS[2 % QUERY_TEXTS.len()]
        ),
    ));
    phases.push(run_mode_phase(
        "mode_recall",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs ORDER BY embedding <=> '{qv0}' LIMIT 10 USING MODE 'recall'"),
    ));
    phases.push(run_mode_phase(
        "mode_precision",
        &core,
        &ctx_a_full,
        &format!(
            "SELECT * FROM docs ORDER BY embedding <=> '{qv0}' LIMIT 10 USING MODE 'precision'"
        ),
    ));
    phases.push(run_rls_isolation_phase(
        &core,
        &ctx_a_public,
        &ctx_a_full,
        &ctx_b,
    ));
    phases.push(run_udf_phase(&core, &ctx_a_full, &qv0));

    let final_db_bytes = db_file_size_bytes(&db_path);
    let final_proc = read_proc_stats();

    let mut out = String::new();
    out.push_str("{\"meta\":{");
    out.push_str(&format!(
        "\"rows_tenant_a\":{ROWS_A},\"rows_tenant_b\":{ROWS_B},\"dim\":{DIM},\
         \"batch_size\":{BATCH_SIZE},\"warmup\":{WARMUP},\"iters\":{ITERS},\
         \"db_bytes_after_ingest\":{rows_after_ingest_bytes},\
         \"db_bytes_final\":{final_db_bytes},\
         \"vm_rss_kb_final\":{},\"vm_hwm_kb_final\":{},\
         \"label\":\"feature_bench\"",
        final_proc.vm_rss_kb, final_proc.vm_hwm_kb,
    ));
    out.push_str("},\"phases\":[");
    for (i, p) in phases.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&phase_to_json(p));
    }
    out.push_str("]}");
    println!("{out}");
}
