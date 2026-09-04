//! 動作検証用シード投入ツール（手動検証専用・CI 非配線）。
//!
//! wire-server を起動して psql 等の実クライアントから `USING PLAN` や hybrid 検索を
//! 手で叩く際に、`docs` テーブルへ合成コーパスを用意するために使う。
//! `seed <db> <rows> <dim>` で 2 テナント（`tenant-a`／`tenant-b`）分の合成文書を
//! Public 可視で投入し、`embed <dim> <text>` で同じ `HashingEmbedder` による
//! クエリベクトルのリテラル（SQL へ貼り付ける形式）を出力する。
//! 引数はローカル運用者の入力であり wire 経由の untrusted 入力ではないため、
//! 解析失敗は `expect` で即終了させる。
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::embedding::{Embedder, HashingEmbedder};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::storage::{RowInput, Storage, Visibility};
use std::time::Instant;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() as usize) % xs.len()]
    }
}

const TOPICS: &[(&str, &[&str], &[&str])] = &[
    (
        "vector-search",
        &[
            "cosine similarity",
            "HNSW index",
            "approximate nearest neighbor",
            "embedding dimension",
            "recall at k",
        ],
        &["コサイン類似度", "近似最近傍探索", "埋め込み次元", "再現率"],
    ),
    (
        "postgres-wire",
        &[
            "startup message",
            "cleartext password",
            "simple query protocol",
            "extended query bind",
            "ErrorResponse SQLSTATE",
        ],
        &[
            "ワイヤプロトコル",
            "認証ハンドシェイク",
            "簡易クエリ",
            "エラー応答",
        ],
    ),
    (
        "storage",
        &[
            "redb write transaction",
            "commit boundary",
            "crash recovery",
            "operation id ledger",
            "durable fsync",
        ],
        &[
            "書き込みトランザクション",
            "クラッシュ回復",
            "台帳",
            "永続化",
        ],
    ),
    (
        "tenant-security",
        &[
            "row level security",
            "tenant boundary",
            "fail closed policy",
            "visibility label",
            "privilege escalation",
        ],
        &[
            "テナント境界",
            "行レベルセキュリティ",
            "可視性ラベル",
            "権限昇格",
        ],
    ),
    (
        "gpu",
        &[
            "wgpu compute shader",
            "f16 resident batch",
            "Vulkan backend",
            "Metal unified memory",
            "kernel dispatch",
        ],
        &[
            "コンピュートシェーダ",
            "半精度常駐",
            "バッチ距離計算",
            "GPU ディスパッチ",
        ],
    ),
    (
        "llm-planning",
        &[
            "query expansion",
            "Ollama endpoint",
            "intent classification",
            "soft boost hint",
            "re-embedding rule",
        ],
        &["クエリ展開", "意図分類", "ソフトブースト", "再埋め込み"],
    ),
    (
        "rerank",
        &[
            "cross encoder",
            "lexical overlap",
            "reciprocal rank fusion",
            "tie break",
            "candidate pool",
        ],
        &["クロスエンコーダ", "字句一致", "順位融合", "同点規約"],
    ),
    (
        "sql-surface",
        &[
            "allowlist parser",
            "GROUP BY HAVING",
            "aggregate COUNT SUM AVG",
            "EXPLAIN plan",
            "USING PLAN syntax",
        ],
        &["許可リスト", "集計関数", "実行計画", "構文検証"],
    ),
    (
        "cooking",
        &[
            "sourdough starter",
            "miso soup",
            "slow braised pork",
            "knife sharpening",
            "fermentation",
        ],
        &["味噌汁の出汁", "発酵", "包丁研ぎ", "煮込み料理"],
    ),
    (
        "astronomy",
        &[
            "exoplanet transit",
            "red giant",
            "radio telescope",
            "dark matter halo",
            "gravitational lensing",
        ],
        &["系外惑星", "赤色巨星", "電波望遠鏡", "重力レンズ"],
    ),
    (
        "gardening",
        &[
            "tomato seedlings",
            "compost pile",
            "drip irrigation",
            "pruning shears",
            "soil pH",
        ],
        &["トマトの苗", "堆肥", "点滴灌漑", "土壌の酸度"],
    ),
    (
        "finance",
        &[
            "index fund",
            "compound interest",
            "bond yield curve",
            "portfolio rebalance",
            "tax loss harvesting",
        ],
        &["インデックス投資", "複利", "利回り曲線", "リバランス"],
    ),
];

const EN_TPL: &[&str] = &[
    "This note explains {a} and how it relates to {b} in the {t} module.",
    "Troubleshooting guide: when {a} fails, first inspect {b} before touching {t}.",
    "Design decision for {t}: we chose {a} over {b} because of latency.",
    "FAQ about {t}. Q: what is {a}? A: it is the mechanism behind {b}.",
    "Meeting summary ({t}): agreed to benchmark {a} against {b} next week.",
];
const JA_TPL: &[&str] = &[
    "{t} に関するメモ。{a}と{b}の関係を整理する。",
    "{t} の障害対応手順。{a}が失敗した場合はまず{b}を確認する。",
    "{t} の設計判断。レイテンシの観点から{b}ではなく{a}を採用した。",
    "{t} についての FAQ。{a}とは{b}を支える仕組みである。",
];

fn render(tpl: &str, t: &str, a: &str, b: &str) -> String {
    tpl.replace("{t}", t).replace("{a}", a).replace("{b}", b)
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:.6}")).collect();
    format!("[{}]", parts.join(","))
}

/// 使い方を表示して終了する（引数不足・不明なサブコマンド）。
fn usage() -> ! {
    eprintln!("usage: seed_docs seed <db> <rows> <dim> | embed <dim> <text...>");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("embed") => {
            let Some(dim) = args.get(2) else { usage() };
            let dim: u32 = dim.parse().expect("dim");
            let text = args.get(3..).unwrap_or(&[]).join(" ");
            if text.is_empty() {
                usage();
            }
            let e = HashingEmbedder::new(dim).expect("embedder");
            let v = e.embed_batch(&[&text]).expect("embed");
            println!("{}", vec_literal(&v[0]));
        }
        Some("seed") => {
            let (Some(db), Some(rows), Some(dim)) = (args.get(2), args.get(3), args.get(4)) else {
                usage()
            };
            let rows: u64 = rows.parse().expect("rows");
            let dim: u32 = dim.parse().expect("dim");
            let embedder = HashingEmbedder::new(dim).expect("embedder");
            let storage = Storage::open(db).expect("open");
            let schema = TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(dim), false),
                    ColumnDef::new("lang", ColumnType::Text, false),
                    ColumnDef::new("topic", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            );
            storage.create_table(&schema).expect("create table");
            let ctx_a = PolicyContext::new("tenant-a").expect("ctx");
            let ctx_b = PolicyContext::new("tenant-b").expect("ctx");
            let mut rng = Lcg(20260831);
            let batch = 1000u64;
            let start = Instant::now();
            let mut id = 1u64;
            let mut batch_no = 0u64;
            while id <= rows {
                let end = (id + batch - 1).min(rows);
                // 1 バッチ内はテナントを固定（末尾 1 桁バッチを tenant-b へ）
                let (ctx, tenant_tag) = if batch_no % 10 == 9 {
                    (&ctx_b, "b")
                } else {
                    (&ctx_a, "a")
                };
                let mut bodies = Vec::new();
                let mut metas: Vec<(u64, String, String)> = Vec::new();
                for i in id..=end {
                    let (topic, en, ja) = rng.pick(TOPICS);
                    let is_ja = rng.next().is_multiple_of(3);
                    let body = if is_ja {
                        let a = rng.pick(ja);
                        let b = rng.pick(ja);
                        render(rng.pick(JA_TPL), topic, a, b)
                    } else {
                        let a = rng.pick(en);
                        let b = rng.pick(en);
                        render(rng.pick(EN_TPL), topic, a, b)
                    };
                    let body = format!("{body} (doc #{i}, tenant {tenant_tag})");
                    metas.push((
                        i,
                        if is_ja { "ja".into() } else { "en".into() },
                        topic.to_string(),
                    ));
                    bodies.push(body);
                }
                let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
                let embs = embedder.embed_batch(&refs).expect("embed");
                let encoded: Vec<Vec<u8>> = metas
                    .iter()
                    .zip(bodies.iter())
                    .zip(embs.iter())
                    .map(|(((_, lang, topic), body), emb)| {
                        encode_scalar_columns(
                            &schema,
                            &[
                                Value::Vector(emb.clone()),
                                Value::Text(lang.clone()),
                                Value::Text(topic.clone()),
                                Value::Text(body.clone()),
                            ],
                        )
                        .expect("encode")
                    })
                    .collect();
                let inputs: Vec<(u64, RowInput<'_>)> = metas
                    .iter()
                    .zip(embs.iter())
                    .zip(encoded.iter())
                    .map(|(((i, _, _), emb), meta)| {
                        (
                            *i,
                            RowInput {
                                tenant_id: ctx.tenant_id(),
                                visibility: Visibility::Public,
                                embedding: emb,
                                metadata: meta,
                            },
                        )
                    })
                    .collect();
                let op = OperationId::parse(&format!("seed-batch-{batch_no}")).expect("op");
                engine::tenant::insert_rows(&storage, "docs", ctx, &inputs, &op)
                    .expect("insert_rows");
                batch_no += 1;
                id = end + 1;
                if batch_no.is_multiple_of(10) {
                    eprintln!(
                        "inserted {} rows ({:.1}s)",
                        end,
                        start.elapsed().as_secs_f64()
                    );
                }
            }
            eprintln!(
                "done: {rows} rows, dim {dim}, {:.1}s",
                start.elapsed().as_secs_f64()
            );
        }
        _ => usage(),
    }
}
