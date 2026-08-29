//! TASK-106（対象ビヘイビア: SEARCH-2, SEARCH-5。ポインタ: `docs/spec/05-tasks.md`
//! TASK-106・`docs/spec/04-behavior/search.md`）。日本語主体コーパスでの CJK
//! トークナイザ構成差（ストップワード除去 ON/OFF・ASCII のみ）の影響度を、決定的に
//! 生成した合成コーパスで実測する回帰テスト。
//!
//! `crates/engine/tests/sparse_stopwords.rs`（TASK-105、ミニコーパスでの相対比較）の
//! 後続フォローアップとして、Markdown 見出し・箇条書き・雑多メモ風の文体に少量の
//! ASCII 識別子を混在させた日本語主体コーパスで、より規模のある実測を行う。
//! `engine::sparse::tokenize_with_options`（公開 API）のみを用いた測定専用ヘルパで
//! BM25 を独立に再計算し、`engine::sparse::SparseIndex`（常にストップワード除去 ON）の
//! 実装は変更しない（production コード `crates/engine/src/` は不変）。
//!
//! 実コーパスでの評価は未了（TASK-106 参照）。本テストは決定的シード付き擬似乱数
//! （外部クレート不使用の xorshift64*）で生成する合成コーパスによる暫定測定であり、
//! 既知の制約として評価レポート（`docs/design/cjk-tokenizer-impact-ja-corpus.md`）に
//! 明記する。
//!
//! 測定値は決定的コーパスのため再現可能であり、下部の関係アサーション（数値
//! リテラルを含まない。Issue #312）で回帰トラッキングする。トークナイザ実装が
//! 変わって関係が崩れた場合はこのテストが失敗し、レポートの再測定を促す。
//! 実測値（hit 数・到達率）は既定では標準出力へも出さず、`RECALL_VERBOSE=1`
//! （`hybrid_recall.rs::resolve_verbose` と同一契約。`GITHUB_ACTIONS` 下は
//! fail-closed で拒否）の opt-in 時のみ出力する。

use engine::sparse::tokenize_with_options;
use std::collections::{BTreeMap, BTreeSet};

// ---------- 決定的擬似乱数（xorshift64*。外部クレート不使用） ----------

/// コーパス生成専用の決定的擬似乱数生成器。テスト再現性のため外部の乱数クレートは
/// 使わず、xorshift64* をこのファイル内に自前実装する（依存最小方針）。
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
        // n == 0 は呼び出し側の配列長・範囲指定ミスであり、コーパス生成の不変条件を
        // 壊すため早期に panic させる（テストコード自身の契約であり untrusted 入力
        // 経路ではないため coding-rust.md の unwrap 禁止は適用されない）。
        assert!(n > 0, "next_range(0) は無効な呼び出し");
        (self.next_u64() % n as u64) as usize
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------- 合成コーパスの語彙 ----------

/// 内容語プール（日本語主体コーパスの「正解キーワード」source of truth）。技術・天気・
/// 料理・旅行・仕事・自然・生活・趣味・健康・教育の 10 ドメイン ×8 語で構成する。
const CONTENT_VOCAB: [&str; 80] = [
    "検索エンジン",
    "機械学習",
    "人工知能",
    "深層学習",
    "自然言語",
    "画像認識",
    "音声認識",
    "量子計算",
    "天気予報",
    "台風情報",
    "気温上昇",
    "積雪注意",
    "梅雨入り",
    "紅葉狩り",
    "花粉症",
    "熱中症",
    "和食料理",
    "中華料理",
    "洋食メニュー",
    "郷土料理",
    "手作りパン",
    "味噌汁",
    "天ぷら",
    "寿司職人",
    "海外旅行",
    "国内旅行",
    "温泉旅館",
    "観光名所",
    "世界遺産",
    "登山道",
    "島巡り",
    "夜行バス",
    "会議資料",
    "議事録作成",
    "在宅勤務",
    "出張手続き",
    "研修制度",
    "転職活動",
    "昇進試験",
    "有給休暇",
    "森林保護",
    "野生動物",
    "河川生態",
    "星空観測",
    "桜並木",
    "紅葉並木",
    "農作物",
    "家庭菜園",
    "掃除道具",
    "洗濯物",
    "収納家具",
    "引っ越し作業",
    "賃貸契約",
    "住宅ローン",
    "庭園設計",
    "電気工事",
    "読書感想",
    "絵画鑑賞",
    "陶芸教室",
    "手芸材料",
    "映画鑑賞",
    "演劇公演",
    "写真撮影",
    "楽器演奏",
    "定期健診",
    "栄養管理",
    "睡眠改善",
    "運動不足",
    "生活習慣",
    "予防接種",
    "歯科治療",
    "心理相談",
    "小学校教育",
    "大学受験",
    "語学学習",
    "資格試験",
    "通信教育",
    "図書館利用",
    "奨学金制度",
    "卒業論文",
];

/// 助詞・機能語・接続表現のプール（ストップワード除去 ON/OFF の差を生む文脈語）。
const FILLER_WORDS: [&str; 30] = [
    "の",
    "を",
    "が",
    "は",
    "に",
    "で",
    "と",
    "から",
    "まで",
    "より",
    "へ",
    "も",
    "な",
    "だ",
    "です",
    "ます",
    "した",
    "して",
    "いる",
    "ある",
    "という",
    "ため",
    "こと",
    "もの",
    "これ",
    "それ",
    "今日",
    "昨日",
    "また",
    "しかし",
];

/// 少量混在させる ASCII 識別子（雑多メモ風コーパスに現れる id・タグの模擬）。
const ASCII_IDS: [&str; 8] = ["API", "TODO", "v2", "memo", "draft", "id01", "ver3", "tmp"];

const NUM_DOCS: usize = 5000;
const NUM_QUERIES: usize = 100;

/// ミスマッチ制御（chance level）比較の許容倍率。実際の hit 数が「無関係な
/// 別クエリの正解集合」に対する偶然一致数の何倍を上回れば良しとするかを決める
/// テストハーネス自身の設計値であり、spec 由来の非公開数値ではない
/// （Issue #312 codex-review P1 対応。`docs/design/cjk-tokenizer-impact-ja-corpus.md`
/// 参照）。
const CONTROL_FACTOR: usize = 3;

/// [`zipf_index`] が使う累積重み（重み `1/(i+1)` の累積和）を 1 度だけ構築する。
/// コーパス生成中（拒否ループ含む）で数万回呼ばれるため、呼び出しごとの
/// 重みベクトル再構築を避ける。
fn build_zipf_cumulative_weights(n: usize) -> Vec<f64> {
    let mut acc = 0.0;
    let mut cumulative = Vec::with_capacity(n);
    for i in 0..n {
        acc += 1.0 / (i as f64 + 1.0);
        cumulative.push(acc);
    }
    cumulative
}

/// Zipf 近似分布（重み `1/(i+1)`）で `CONTENT_VOCAB` の添字を選ぶ。先頭語ほど高頻度、
/// 末尾語ほど低頻度になり、QA セットの正解集合を小さく絞り込める語彙分布を作る。
/// `cumulative_weights` は [`build_zipf_cumulative_weights`] の事前計算結果。
fn zipf_index(rng: &mut Xorshift64, cumulative_weights: &[f64]) -> usize {
    let total = *cumulative_weights.last().unwrap_or(&0.0);
    let r = rng.next_f64() * total;
    match cumulative_weights.iter().position(|&acc| r <= acc) {
        Some(i) => i,
        None => cumulative_weights.len().saturating_sub(1),
    }
}

/// 合成コーパスの 1 文書。`keywords` は生成時に既知の「正解キーワード集合」
/// （`CONTENT_VOCAB` への添字）で、QA セットの正解文書判定に使う。
struct Doc {
    id: u64,
    text: String,
    keywords: BTreeSet<usize>,
}

/// QA セット 1 件（`direct` カテゴリ相当: 内容語をほぼそのまま問う短文クエリ）。
/// `correct` は生成時に既知の正解文書 ID 集合。
struct QaCase {
    query: String,
    correct: BTreeSet<u64>,
}

/// 決定的シード付き擬似乱数で日本語主体の合成コーパスと QA セットを生成する。
///
/// 将来、実コーパスへ差し替える際はこの関数を置き換えるだけで済むよう、
/// 生成ロジックをこの 1 関数に閉じ込めている。`sparse.rs` の各上限
/// （`MAX_CORPUS_DOCS`・`MAX_DOC_BYTES`・`MAX_CORPUS_BYTES`）に収まる規模で設計する。
fn generate_corpus(seed: u64) -> (Vec<Doc>, Vec<QaCase>) {
    let mut rng = Xorshift64::new(seed);
    let mut docs = Vec::with_capacity(NUM_DOCS);
    let mut inverted: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
    let zipf_weights = build_zipf_cumulative_weights(CONTENT_VOCAB.len());

    for doc_id in 0..NUM_DOCS as u64 {
        let num_keywords = 3 + rng.next_range(4); // 3..=6
        let mut kw_set: BTreeSet<usize> = BTreeSet::new();
        while kw_set.len() < num_keywords {
            kw_set.insert(zipf_index(&mut rng, &zipf_weights));
        }
        let kw_vec: Vec<usize> = kw_set.iter().copied().collect();

        let mut text = String::new();
        // Markdown 見出し・箇条書き・地の文を混在させる（対象ドメインの詳細は
        // TASK-106 参照）。
        match rng.next_range(3) {
            0 => text.push_str("## "),
            1 => text.push_str("- "),
            _ => {}
        }
        for (i, &kw_idx) in kw_vec.iter().enumerate() {
            if i > 0 {
                text.push_str(FILLER_WORDS[rng.next_range(FILLER_WORDS.len())]);
            }
            text.push_str(CONTENT_VOCAB[kw_idx]);
        }
        // 少量の ASCII 識別子を混在させる（高頻度には出さず「日本語主体」を保つ）。
        if rng.next_range(5) == 0 {
            text.push(' ');
            text.push_str(ASCII_IDS[rng.next_range(ASCII_IDS.len())]);
        }
        text.push_str(FILLER_WORDS[rng.next_range(FILLER_WORDS.len())]);
        text.push_str("こと。");

        for &kw_idx in &kw_vec {
            inverted.entry(kw_idx).or_default().push(doc_id);
        }
        docs.push(Doc {
            id: doc_id,
            text,
            keywords: kw_set,
        });
    }

    let qa = generate_qa_set(&mut rng, &docs, &inverted);
    (docs, qa)
}

/// 各文書から最も出現頻度の低いキーワード 2 語（AND 組み合わせ）を選び、正解集合が
/// コーパス全体に対して十分に絞り込まれた `direct` クエリを構成する。
fn generate_qa_set(
    rng: &mut Xorshift64,
    docs: &[Doc],
    inverted: &BTreeMap<usize, Vec<u64>>,
) -> Vec<QaCase> {
    let mut order: Vec<usize> = (0..docs.len()).collect();
    // Fisher-Yates シャッフル（シード由来のため決定的）。
    for i in (1..order.len()).rev() {
        let j = rng.next_range(i + 1);
        order.swap(i, j);
    }

    let mut qa = Vec::with_capacity(NUM_QUERIES);
    for &doc_idx in &order {
        if qa.len() >= NUM_QUERIES {
            break;
        }
        let doc = &docs[doc_idx];
        if doc.keywords.len() < 2 {
            continue;
        }
        let mut kws: Vec<usize> = doc.keywords.iter().copied().collect();
        kws.sort_by_key(|k| inverted.get(k).map_or(0, Vec::len));
        let (a, b) = (kws[0], kws[1]);

        let set_a: BTreeSet<u64> = inverted.get(&a).into_iter().flatten().copied().collect();
        let set_b: BTreeSet<u64> = inverted.get(&b).into_iter().flatten().copied().collect();
        let correct: BTreeSet<u64> = set_a.intersection(&set_b).copied().collect();
        if correct.is_empty() {
            continue;
        }

        qa.push(QaCase {
            query: format!("{}{}", CONTENT_VOCAB[a], CONTENT_VOCAB[b]),
            correct,
        });
    }

    qa
}

// ---------- BM25 測定ヘルパ（`sparse_stopwords.rs` の `bm25_rank` を踏襲） ----------

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// [`SparseIndex::build`]（既定パラメータ）と同じ Okapi BM25 のコーパス統計。クエリ
/// ごとに全文書を再トークン化しないよう、変種（除去 ON/OFF・ASCII のみ）ごとに 1 度
/// だけ構築して使い回す（`sparse_stopwords.rs` のミニコーパス版はクエリ単位で構築
/// するが、本ファイルは 5,000 件 ×100 クエリ規模のため分離が必須）。
struct Bm25Stats {
    term_freqs: Vec<(u64, BTreeMap<String, u32>, u32)>,
    doc_freq: BTreeMap<String, u32>,
    avg_doc_len: f64,
    n: f64,
}

fn build_stats<F>(docs: &[Doc], tokenize_fn: F) -> Bm25Stats
where
    F: Fn(&str) -> Vec<String>,
{
    let mut term_freqs = Vec::with_capacity(docs.len());
    let mut doc_freq: BTreeMap<String, u32> = BTreeMap::new();
    let mut total_len: u64 = 0;

    for doc in docs {
        let toks = tokenize_fn(&doc.text);
        let mut tf: BTreeMap<String, u32> = BTreeMap::new();
        for t in &toks {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        for t in tf.keys() {
            *doc_freq.entry(t.clone()).or_insert(0) += 1;
        }
        let doc_len = u32::try_from(toks.len()).unwrap_or(u32::MAX);
        total_len += u64::from(doc_len);
        term_freqs.push((doc.id, tf, doc_len));
    }

    let n = docs.len() as f64;
    Bm25Stats {
        term_freqs,
        doc_freq,
        avg_doc_len: total_len as f64 / n.max(1.0),
        n,
    }
}

/// 事前構築済みの [`Bm25Stats`] からクエリ 1 件のランキングを計算する。スコア降順
/// （同点は `doc_id` 昇順）で返す。`query_terms` は `BTreeSet` を要求し、走査順を
/// 決定的にする（`HashSet` はプロセスごとに反復順が変わり、浮動小数の加算順序に
/// 依存するスコアの再現性を壊すため使わない）。
fn rank(stats: &Bm25Stats, query_terms: &BTreeSet<String>) -> Vec<u64> {
    let mut scored: Vec<(f64, u64)> = stats
        .term_freqs
        .iter()
        .filter_map(|(doc_id, tf, doc_len)| {
            let mut score = 0.0f64;
            for term in query_terms {
                let Some(&f) = tf.get(term) else { continue };
                let df = f64::from(*stats.doc_freq.get(term).unwrap_or(&0));
                let idf = ((stats.n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let numerator = f64::from(f) * (K1 + 1.0);
                let len_norm =
                    1.0 - B + B * (f64::from(*doc_len) / stats.avg_doc_len.max(f64::MIN_POSITIVE));
                let denominator = f64::from(f) + K1 * len_norm;
                if denominator > 0.0 {
                    score += idf * (numerator / denominator);
                }
            }
            (score > 0.0).then_some((score, *doc_id))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, id)| id).collect()
}

/// 変種 C（ASCII のみ）: `tokenize_with_options(text, false)`（除去 OFF の全トークン）
/// から CJK 由来トークン（ユニグラム・バイグラム）を後段フィルタで除去し、CJK
/// トークンを一切使わない構成を再現する。日本語主体コーパスでの劣化度を測る参照点。
fn tokenize_ascii_only(text: &str) -> Vec<String> {
    tokenize_with_options(text, false)
        .into_iter()
        .filter(|t| t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .collect()
}

/// Issue #31 / TASK-106（SEARCH-2, SEARCH-5）: 日本語主体コーパスでの CJK トークナイザ
/// 構成差（除去 ON / 除去 OFF / ASCII のみ）の Recall@20・Recall@100 を実測する。
///
/// 合成コーパス・QA セットは決定的に生成されるため実測値は再現可能であり、下部の
/// 関係アサーション（数値リテラルを含まない。Issue #312）で回帰トラッキングする。
/// 測定方法・関係の詳細は `docs/design/cjk-tokenizer-impact-ja-corpus.md` を参照。
#[test]
fn cjk_tokenizer_impact_on_ja_corpus() {
    let verbose = verbose_requested_from_env();
    let (docs, qa) = generate_corpus(0x5EED_C0FF_EE42_1337);

    // 健全性の検証: コーパスが sparse.rs の各上限（MAX_CORPUS_DOCS・MAX_DOC_BYTES・
    // MAX_CORPUS_BYTES）に収まること。無制限なコーパス生成を許さない設計指針を
    // 測定ハーネス自身にも適用する。
    assert!(!docs.is_empty());
    assert!(
        docs.len() <= 100_000,
        "MAX_CORPUS_DOCS を超過してはならない"
    );
    let mut total_bytes: usize = 0;
    for doc in &docs {
        assert!(
            doc.text.len() <= 1024 * 1024,
            "MAX_DOC_BYTES を超過してはならない"
        );
        total_bytes += doc.text.len();
    }
    assert!(
        total_bytes <= 64 * 1024 * 1024,
        "MAX_CORPUS_BYTES を超過してはならない"
    );

    // QA セットの非空性・正解集合の非空性を検証する。
    assert!(!qa.is_empty());
    for case in &qa {
        assert!(!case.correct.is_empty());
    }
    // フィクスチャ規模そのものの縮退検知（Cursor Bugbot 指摘対応）:
    // `total_correct`/`ceil20`/`ceil100` は qa 自身から会計整合を再計算するため、
    // qa（延いてはコーパス）が生成ロジックの変化で縮小しても検知できない。
    // 生成に使う定数（NUM_DOCS・NUM_QUERIES）に対する固定値検査で fixture
    // サイズそのものを回帰トラッキングする（hybrid_recall.rs・rerank_recall.rs の
    // 既存 `qa.len()` 固定値検査と同型）。
    assert_eq!(
        docs.len(),
        NUM_DOCS,
        "コーパス件数が生成定数から変化した（コーパス生成ロジックの回帰）"
    );
    assert_eq!(
        qa.len(),
        NUM_QUERIES,
        "QA 件数が生成定数から変化した（generate_qa_set の重複除外・空集合除外パスでの縮退懸念）"
    );

    let stats_on = build_stats(&docs, |t| tokenize_with_options(t, true));
    let stats_off = build_stats(&docs, |t| tokenize_with_options(t, false));
    let stats_ascii = build_stats(&docs, tokenize_ascii_only);

    let mut total_correct = 0usize;
    let mut hits20 = [0usize; 3];
    let mut hits100 = [0usize; 3];
    // Recall@k の理論上限（ADR「考察」の天井効果の根拠）: 1 クエリの正解数が k を
    // 超える場合、そのクエリからは高々 k 件しか回収できない。Σmin(k, |correct_q|)
    // が達成可能な hit 数の上限となる。
    let mut ceil20 = 0usize;
    let mut ceil100 = 0usize;
    // クエリ単位の到達カバレッジ（変種 A/B のみ。正解を 1 件も拾えなかった
    // クエリの件数を検知する。Issue #312 codex-review P1 対応: 会計整合・
    // 上限以下・単調性・非空だけでは「1 hit だけ拾えれば全体が通過」してしまい
    // Recall がほぼゼロまで崩壊しても検知できないため追加する）。
    let mut queries_hit20 = [0usize; 3];
    // ミスマッチ制御（chance level）: 各クエリの実際の上位 20 件を「1 つずらした
    // 別クエリの正解集合」に対しても採点し、偶然一致の水準を同一ランで実測する。
    // 実際の hit 数がこの chance level を大きく上回ることを下部で確認することで、
    // Recall が偶然一致の水準まで崩壊していないことを、非公開の絶対値を使わずに
    // 検知する（Issue #312 codex-review P1 対応）。
    let mut control_hits20 = [0usize; 3];

    for (idx, case) in qa.iter().enumerate() {
        total_correct += case.correct.len();
        ceil20 += case.correct.len().min(20);
        ceil100 += case.correct.len().min(100);
        let control_correct = &qa[(idx + 1) % qa.len()].correct;

        let query_on: BTreeSet<String> = tokenize_with_options(&case.query, true)
            .into_iter()
            .collect();
        let query_off: BTreeSet<String> = tokenize_with_options(&case.query, false)
            .into_iter()
            .collect();
        let query_ascii: BTreeSet<String> = tokenize_ascii_only(&case.query).into_iter().collect();

        let ranked_on = rank(&stats_on, &query_on);
        let ranked_off = rank(&stats_off, &query_off);
        let ranked_ascii = rank(&stats_ascii, &query_ascii);

        for (i, ranked) in [&ranked_on, &ranked_off, &ranked_ascii]
            .into_iter()
            .enumerate()
        {
            let top20: Vec<u64> = ranked.iter().take(20).copied().collect();
            let h20 = top20.iter().filter(|id| case.correct.contains(id)).count();
            hits20[i] += h20;
            if h20 > 0 {
                queries_hit20[i] += 1;
            }
            hits100[i] += ranked
                .iter()
                .take(100)
                .filter(|id| case.correct.contains(id))
                .count();
            control_hits20[i] += top20
                .iter()
                .filter(|id| control_correct.contains(id))
                .count();
        }
    }

    if verbose {
        println!("=== TASK-106 CJK トークナイザ影響度（日本語主体合成コーパス） ===");
        println!(
            "docs={} queries={} total_correct={}",
            docs.len(),
            qa.len(),
            total_correct
        );
        let labels = ["A:除去ON", "B:除去OFF", "C:ASCIIのみ"];
        for i in 0..3 {
            println!(
                "{}: Recall@20={:.4} ({}/{})  Recall@100={:.4} ({}/{})",
                labels[i],
                hits20[i] as f64 / total_correct as f64,
                hits20[i],
                total_correct,
                hits100[i] as f64 / total_correct as f64,
                hits100[i],
                total_correct,
            );
        }
    }

    // 数値リテラルを含まない関係アサーション（会計整合・上限以下・単調性・非空）で
    // 回帰トラッキングする（トークナイザ実装が変わって関係が崩れた場合はこのテストが
    // 失敗し、レポート（docs/design/cjk-tokenizer-impact-ja-corpus.md）の再測定を
    // 促す。Issue #312。それ以前は hit 数・理論上限の固定値アサーションだったが、
    // 層 B の非公開閾値の逆算材料になりうるため除去した）。
    let exp_total_correct: usize = qa.iter().map(|c| c.correct.len()).sum();
    let exp_ceil20: usize = qa.iter().map(|c| c.correct.len().min(20)).sum();
    let exp_ceil100: usize = qa.iter().map(|c| c.correct.len().min(100)).sum();
    assert_eq!(
        total_correct, exp_total_correct,
        "total_correct が QA セットからの会計整合と一致しなかった"
    );
    assert_eq!(
        ceil20, exp_ceil20,
        "ceil20 が QA セットからの会計整合と一致しなかった"
    );
    assert_eq!(
        ceil100, exp_ceil100,
        "ceil100 が QA セットからの会計整合と一致しなかった"
    );

    // 変種 A（除去 ON）・B（除去 OFF）: 上限以下・単調性・非空（vacuous pass 防止）。
    for i in [0usize, 1usize] {
        assert!(
            hits20[i] <= ceil20,
            "変種 {i} の Recall@20 hit 数が理論上限 ceil20 を超えた"
        );
        assert!(
            hits100[i] <= ceil100,
            "変種 {i} の Recall@100 hit 数が理論上限 ceil100 を超えた"
        );
        assert!(
            hits100[i] <= total_correct,
            "変種 {i} の Recall@100 hit 数が正解集合の総数を超えた"
        );
        assert!(
            hits20[i] <= hits100[i],
            "変種 {i} の Recall@20 hit 数が Recall@100 hit 数を上回った"
        );
        assert!(
            hits20[i] > 0,
            "変種 {i} の Recall@20 hit 数が 0 件（vacuous pass の懸念）"
        );
        // クエリ単位カバレッジ: 本 QA セットは各クエリの正解を「2 語 AND 一致」で
        // 厳密に構成しており（`generate_qa_set`）、トークナイザが正しく機能する
        // 限りどのクエリも上位 20 件から少なくとも 1 件は正解を拾えるはずである。
        // 合計 hit 数だけでは「特定の 1 クエリが hit を稼ぎ、残り全クエリが 0 件」
        // でも通過してしまう（Issue #312 codex-review P1 対応）。
        assert_eq!(
            queries_hit20[i],
            qa.len(),
            "変種 {i} で Recall@20 の hit が 0 件のクエリが存在した（一部クエリで正解を 1 件も拾えていない）"
        );
        // ミスマッチ制御（chance level）比較: 実際の hit 数が、無関係な別クエリの
        // 正解集合に対する偶然一致水準の CONTROL_FACTOR 倍を上回ることを確認する。
        // Recall が偶然一致の水準まで崩壊すると、この関係が破れて検知できる。
        assert!(
            hits20[i] > control_hits20[i] * CONTROL_FACTOR,
            "変種 {i} の Recall@20 hit 数が、無関係クエリに対する偶然一致水準（chance level）を \
             十分に上回らなかった（Recall が chance level 近くまで崩壊した懸念）"
        );
    }

    // 変種 C（ASCII のみ）: 本 QA セットのクエリは CONTENT_VOCAB（純 CJK 語彙）のみ
    // から構成されるため、ASCII のみ構成では query_ascii が構造的に空集合となり、
    // rank() は恒等的に空を返す。これは劣化度の実測値ではなく構造的に自明な値
    // （詳細は docs/design/cjk-tokenizer-impact-ja-corpus.md「考察」参照）であり、
    // 実測値ではなく構造不変条件として維持する。
    assert_eq!(
        hits20[2], 0,
        "ASCII のみ構成の Recall@20 hit 数が構造的な自明値 0 から変化した"
    );
    assert_eq!(
        hits100[2], 0,
        "ASCII のみ構成の Recall@100 hit 数が構造的な自明値 0 から変化した"
    );

    // 除去 ON が空クエリ構成（変種 C）を上回ることを確認する（構造的に真な
    // 回帰チェックであり、劣化度の実測検証ではない）。
    assert!(
        hits100[0] > hits100[2],
        "除去 ON が ASCII のみ構成を上回ることを期待したが逆転した"
    );
}

/// PR #319 codex-review P1 対応: `cjk_tokenizer_impact_on_ja_corpus`
/// の関係アサーション（chance level 比較・クエリ単位カバレッジ）が、公開資産に
/// 実測値を持ち込まずに実際の劣化を検知できることを証明するミューテーション
/// テスト（`hybrid_recall.rs` の同名ミューテーションテストと同型）。除去 ON
/// 変種について、各クエリを 1 つずらして正解集合（[`QaCase::correct`]）との
/// 対応を崩し、この状態で本体テストと同じ chance level 比較が実際に失敗する
/// ことを確認する。
#[test]
fn cjk_tokenizer_impact_on_ja_corpus_detects_query_answer_mismatch() {
    let (docs, qa) = generate_corpus(0x5EED_C0FF_EE42_1337);
    assert!(!qa.is_empty());

    let stats_on = build_stats(&docs, |text| tokenize_with_options(text, true));

    let mut hits20 = 0usize;
    let mut control_hits20 = 0usize;
    for (idx, case) in qa.iter().enumerate() {
        // 正解集合はケース idx のまま、クエリだけをケース idx+1 のものへ
        // 差し替える（`hybrid_recall.rs` のミューテーションテストと同じ構成）。
        let wrong_query = &qa[(idx + 1) % qa.len()].query;
        let control_correct = &qa[(idx + 2) % qa.len()].correct;

        let query_terms: BTreeSet<String> = tokenize_with_options(wrong_query, true)
            .into_iter()
            .collect();
        let ranked = rank(&stats_on, &query_terms);
        let top20: Vec<u64> = ranked.iter().take(20).copied().collect();
        hits20 += top20.iter().filter(|id| case.correct.contains(id)).count();
        control_hits20 += top20
            .iter()
            .filter(|id| control_correct.contains(id))
            .count();
    }

    assert!(
        hits20 <= control_hits20 * CONTROL_FACTOR,
        "クエリ・正解の対応を崩したミューテーションが層 A の chance level 比較を \
         すり抜けた（層 A のゲートが vacuous pass になっている懸念）"
    );
}

// ---------- 実測値の既定非出力（Issue #312）。`RECALL_VERBOSE` opt-in ゲート ----------
// `hybrid_recall.rs::resolve_verbose`/`verbose_requested_from_env` と同一契約
// （`tests/` 直下は独立 test crate・共有モジュール無しの既存慣行のため複製する）。

/// `RECALL_VERBOSE` の生値と `GITHUB_ACTIONS` 判定を引数化した純関数（単体テスト可能・
/// 環境変数を直接読まない）。`raw == Some("1")` の厳密一致のみ verbose 要求とみなす。
/// `under_github_actions` が真の場合、verbose 要求は defense-in-depth として `Err` で
/// fail-closed に拒否する。
fn resolve_verbose(raw: Option<&str>, under_github_actions: bool) -> Result<bool, &'static str> {
    let requested = raw == Some("1");
    if requested && under_github_actions {
        return Err(
            "RECALL_VERBOSE=1 is refused while running under GitHub Actions (GITHUB_ACTIONS is set); rerun outside GitHub Actions to print measured values",
        );
    }
    Ok(requested)
}

/// 環境変数を読み取って [`resolve_verbose`] へ渡し、`Err` は `panic!` で fail-closed に
/// する（テスト冒頭、コーパス生成前に呼ぶ）。
fn verbose_requested_from_env() -> bool {
    let raw = std::env::var("RECALL_VERBOSE").ok();
    match resolve_verbose(raw.as_deref(), std::env::var_os("GITHUB_ACTIONS").is_some()) {
        Ok(v) => v,
        Err(msg) => panic!("{msg}"),
    }
}

#[cfg(test)]
mod verbose_gate_tests {
    use super::resolve_verbose;

    #[test]
    fn resolve_verbose_defaults_to_false_when_unset() {
        assert_eq!(resolve_verbose(None, false), Ok(false));
    }

    #[test]
    fn resolve_verbose_true_on_exact_match_outside_github_actions() {
        assert_eq!(resolve_verbose(Some("1"), false), Ok(true));
    }

    #[test]
    fn resolve_verbose_rejects_non_exact_values() {
        for raw in [" 1", "true", "0", ""] {
            assert_eq!(resolve_verbose(Some(raw), false), Ok(false));
        }
    }

    #[test]
    fn resolve_verbose_fails_closed_under_github_actions_when_requested() {
        assert!(resolve_verbose(Some("1"), true).is_err());
    }

    #[test]
    fn resolve_verbose_unset_under_github_actions_does_not_block_normal_gate_runs() {
        assert_eq!(resolve_verbose(None, true), Ok(false));
    }
}
