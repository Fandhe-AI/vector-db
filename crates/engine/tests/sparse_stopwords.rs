//! CJK ストップワード除去（TASK-105、対象ビヘイビア: SEARCH-5。ポインタ:
//! `docs/spec/05-tasks.md` TASK-105・`docs/spec/04-behavior/search.md`）の統合テスト。
//!
//! `crates/engine/tests/sparse.rs`（TASK-102、SEARCH-1/3）の役割を引き継ぎつつ、
//! 本ファイルは TASK-105 固有の観点（助詞ユニグラムの単独トークン化抑制・CJK
//! コンテンツの保持・除去有無による Recall@20 の比較測定）に限定する。
//!
//! 本ファイル内の `bm25_score`/`recall_at_20` は `engine::sparse::tokenize_with_options`
//! （公開 API）のみを用いて Okapi BM25 を独立に再計算する測定専用のヘルパであり、
//! `engine::sparse::SparseIndex`（常にストップワード除去 ON のトークナイザを使う）の
//! 実装を置き換えるものではない。除去 ON/OFF 双方の挙動を同一の場所で比較測定する
//! ために、あえて `SparseIndex` を介さず直接トークナイザを呼び分けている。

use engine::sparse::{tokenize_with_options, SparseIndex};
use std::collections::{BTreeMap, HashSet};

/// [`SparseIndex::build`]（既定パラメータ）と同じ Okapi BM25（`k1 = 1.2`, `b = 0.75`）を、
/// 除去有無を選べる `tokenize_with_options` から独立に再計算する測定用ヘルパ。
/// スコア `> 0` の文書を降順（同点は `doc_id` 昇順）で返す。
fn bm25_rank(docs: &[(u64, &str)], query: &str, remove_stopwords: bool) -> Vec<u64> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;

    let mut term_freqs: Vec<(u64, BTreeMap<String, u32>, u32)> = Vec::new();
    let mut doc_freq: BTreeMap<String, u32> = BTreeMap::new();
    let mut total_len: u64 = 0;

    for &(doc_id, text) in docs {
        let toks = tokenize_with_options(text, remove_stopwords);
        let mut tf: BTreeMap<String, u32> = BTreeMap::new();
        for t in &toks {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        for t in tf.keys() {
            *doc_freq.entry(t.clone()).or_insert(0) += 1;
        }
        let doc_len = u32::try_from(toks.len()).unwrap_or(u32::MAX);
        total_len += u64::from(doc_len);
        term_freqs.push((doc_id, tf, doc_len));
    }

    let n = docs.len() as f64;
    let avg_doc_len = total_len as f64 / n.max(1.0);

    let query_terms: HashSet<String> = tokenize_with_options(query, remove_stopwords)
        .into_iter()
        .collect();

    let mut scored: Vec<(f64, u64)> = term_freqs
        .iter()
        .filter_map(|(doc_id, tf, doc_len)| {
            let mut score = 0.0f64;
            for term in &query_terms {
                let Some(&f) = tf.get(term) else { continue };
                let df = f64::from(*doc_freq.get(term).unwrap_or(&0));
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let numerator = f64::from(f) * (K1 + 1.0);
                let len_norm =
                    1.0 - B + B * (f64::from(*doc_len) / avg_doc_len.max(f64::MIN_POSITIVE));
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

/// direct Recall@20: `relevant` のうち Top-20 に入った件数の割合。
fn recall_at_20(
    docs: &[(u64, &str)],
    query: &str,
    relevant: &[u64],
    remove_stopwords: bool,
) -> f64 {
    let ranked = bm25_rank(docs, query, remove_stopwords);
    let top20: HashSet<u64> = ranked.into_iter().take(20).collect();
    let hit = relevant.iter().filter(|id| top20.contains(id)).count();
    hit as f64 / relevant.len() as f64
}

/// 合成日本語ミニコーパス（正解付き）。「ベクトル検索エンジン」に関する内容語を持つ
/// 文書（`RELEVANT_DOC_IDS`）と、助詞を多用するが無関係な内容の文書（ノイズ）を
/// 混在させる。ノイズ文書はクエリの助詞ユニグラム（の・を等）とは一致しうるが
/// 内容語のバイグラムとは一致しない。spec 側の PoC 実測値（非公開）は転記せず、
/// 本テストで計算した実測値のみを下記コメントに記録する。
fn mini_corpus() -> Vec<(u64, &'static str)> {
    vec![
        (1, "ベクトル検索エンジンの仕組みを解説する記事"),
        (2, "全文検索エンジンとベクトル検索エンジンの違いについて"),
        (3, "ベクトル検索エンジンを構築する手順を紹介します"),
        (4, "今日は天気が良いので散歩をしました"),
        (5, "彼はカフェでコーヒーを飲みながら本を読んだ"),
        (6, "この店のラーメンはとても美味しいと評判だ"),
        (7, "猫が窓辺で日向ぼっこをしている様子を眺める"),
        (8, "駅前の書店で新しい小説を買った帰り道"),
        (9, "友人と映画を見に行く約束をしたが延期になった"),
        (10, "料理教室で初めてパンを焼く体験をした"),
        (11, "山の頂上からの景色は言葉にできないほど美しい"),
        (12, "会議の議事録を作成してチームに共有する仕事"),
        (13, "電車が遅延したため会社に到着するのが遅れた"),
        (14, "冬になると雪が積もる地方の暮らしを紹介する"),
        (15, "祖母の家で作る味噌汁の味が忘れられない"),
        (16, "新しいスマートフォンの発表会が開催された"),
        (17, "図書館で借りた本を期限内に返却する必要がある"),
        (18, "海辺の町で育った子供時代の思い出を語る"),
        (19, "自転車で通勤する人が最近増えている理由"),
        (20, "庭に植えた花が春になって一斉に咲き始めた"),
        (21, "台所の片付けをしていたら古い写真が出てきた"),
        (22, "夜遅くまで残業をした翌日は体調を崩しやすい"),
        (23, "旅行先で食べた郷土料理がとても印象に残った"),
        (24, "子供が学校の宿題を終わらせてから遊びに行った"),
        (25, "隣町に新しくできたパン屋の評判を聞いてみた"),
        (26, "週末は家族で公園に出かけて過ごすことが多い"),
        (27, "工場の生産ラインを見学するツアーに参加した"),
        (28, "音楽フェスに行くために友人とチケットを取った"),
        (29, "健康のために毎朝ジョギングを続けている人が多い"),
        (30, "美術館で開催中の展覧会を見に行くつもりだ"),
    ]
}

const RELEVANT_DOC_IDS: [u64; 3] = [1, 2, 3];

#[test]
fn stopword_removal_does_not_increase_noise_document_matches() {
    // 助詞ユニグラムの単独一致はノイズ文書のスコアにもわずかに寄与しうる。除去 ON は
    // その寄与源の一部（助詞の単独一致）を取り除くため、スコア > 0 になるノイズ文書
    // （「ベクトル検索エンジン」に無関係な文書）の件数は除去 OFF を上回らないことを
    // 確認する（内容語のバイグラム一致による寄与は ON/OFF で変わらないため、この
    // 差分は助詞ユニグラム起因の寄与のみを捉える）。
    let docs = mini_corpus();
    let query = "ベクトル検索エンジンの使い方を教えてください";

    let noise_count = |remove_stopwords: bool| -> usize {
        bm25_rank(&docs, query, remove_stopwords)
            .into_iter()
            .filter(|id| !RELEVANT_DOC_IDS.contains(id))
            .count()
    };

    let noise_on = noise_count(true);
    let noise_off = noise_count(false);
    assert!(
        noise_on <= noise_off,
        "除去 ON のノイズ文書ヒット数（{noise_on}）が除去 OFF（{noise_off}）を上回っている"
    );
}

#[test]
fn recall_at_20_with_stopword_removal_is_not_lower_than_without() {
    // 除去 ON の direct Recall@20 が除去 OFF を下回らないことを固定する
    // （TASK-105 の受入基準: 除去有無の Recall@20 変動を測定・記録する）。
    // 実測値（本リポ内の合成コーパスのみ。spec 側 PoC の数値は転記しない）:
    //   on_recall  = 1.0（3/3 件が Top-20 に入る）
    //   off_recall = 1.0（3/3 件が Top-20 に入る。本コーパス規模ではノイズ文書の
    //     助詞一致スコアが内容語一致スコアを上回るほど蓄積しないため、除去 OFF でも
    //     正解 3 件は Top-20 内に留まる）
    // 数万チャンク規模の実測は TASK-106（並行フォローアップ）の管轄のためスコープ外。
    let docs = mini_corpus();
    let query = "ベクトル検索エンジンの使い方を教えてください";

    let on_recall = recall_at_20(&docs, query, &RELEVANT_DOC_IDS, true);
    let off_recall = recall_at_20(&docs, query, &RELEVANT_DOC_IDS, false);

    assert!(
        on_recall >= off_recall,
        "除去 ON の Recall@20（{on_recall}）が除去 OFF（{off_recall}）を下回っている"
    );
}

#[test]
fn relevant_documents_rank_above_noise_with_stopword_removal() {
    // 除去 ON で「ベクトル検索エンジン」関連の 3 文書が Top-3 を占めること
    // （ノイズ文書に押しのけられないこと）を確認する。
    let docs = mini_corpus();
    let query = "ベクトル検索エンジンの使い方を教えてください";

    let ranked = bm25_rank(&docs, query, true);
    let top3: HashSet<u64> = ranked.into_iter().take(3).collect();
    for id in RELEVANT_DOC_IDS {
        assert!(top3.contains(&id), "doc {id} が Top-3 に入っていない");
    }
}

#[test]
fn recall_at_20_computation_is_deterministic() {
    // 同一入力からは同一の Recall@20 が得られること（再現性の固定）。
    let docs = mini_corpus();
    let query = "ベクトル検索エンジンの使い方を教えてください";
    let first = recall_at_20(&docs, query, &RELEVANT_DOC_IDS, true);
    let second = recall_at_20(&docs, query, &RELEVANT_DOC_IDS, true);
    assert!((first - second).abs() < 1e-12);
}

/// 上記の `bm25_rank`/`recall_at_20` は `tokenize_with_options` を直接呼ぶ測定専用
/// ヘルパで `SparseIndex` を介さないため、本番エントリポイント（既定で除去 ON の
/// `tokenize()` を使う `SparseIndex::build`/`search`）の回帰はこの 1 件で検知する。
/// 助詞ユニグラムのみのクエリは既定設定でトークンが 0 件になり、`search()` は
/// 早期リターンで空結果を返す。`tokenize()` の既定が除去 OFF 相当に配線ミスした
/// 場合は助詞ユニグラムがトークンとして残り、助詞を含むノイズ文書がヒットして
/// 空結果でなくなるため、この配線崩れを検知できる。
#[test]
fn sparse_index_search_removes_particle_only_query_by_default() {
    let docs = mini_corpus();
    let idx = SparseIndex::build(&docs).expect("build");

    let results = idx.search("の", 20).expect("search");
    assert!(
        results.is_empty(),
        "助詞ユニグラムのみのクエリが SparseIndex::search 経由で空結果にならなかった: {results:?}"
    );
}
