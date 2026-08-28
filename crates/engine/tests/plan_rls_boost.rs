//! LLM クエリプランニングのソフトブーストヒント（`hybrid.rs` の `BoostRule`／
//! `hybrid_search_boosted`。TASK-111・PLAN-1）と RLS 事前フィルタ（`tenant.rs`・
//! `rls.rs`。TASK-133）の統合検証（TASK-139。対象ビヘイビア: なし〔基盤〕。ポインタ:
//! `docs/spec/04-behavior/rls.md`・`core-engine.md`「制約・宿題」節・
//! `query-planning.md`）。
//!
//! 両機構が実際に合流するのは engine クレート内の「RLS 事前フィルタ済み候補集合
//! （`tenant::visible_rows`）→ ヒント一致判定（`hybrid::path_hint_matches`/
//! `kind_hint_matches`）→ `BoostRule` 構築 → `hybrid::hybrid_search_boosted`」という
//! 呼び出し列であり、本ファイルはこの列を end-to-end で検証する
//! （`tests/tenant_isolation.rs` の複数テナント `Storage` 構築流儀と
//! `tests/soft_boost.rs` の合成コーパス流儀を組み合わせる）。`sql/exec.rs` 経由の
//! `USING PLAN` 実行器（TASK-77）は未実装のため、SQL 表層・wire 経由の検証は対象外
//! （engine API 層での検証に限定する。`docs/design/rls-generalized-read-paths.md` と
//! 同じスコープ境界の整理）。
//!
//! 検証観点の設計は本来 spec 上「人間」担当（TASK-139）のため、本ファイルが固定する
//! V1〜V5 の各観点は `docs/design/plan-rls-boost-interaction.md`（Proposed）でオーナーの
//! 追認を待つ。各観点のテスト関数は同レポートの節と対応する。

use std::collections::BTreeSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::hybrid::{
    hybrid_search, hybrid_search_boosted, kind_hint_matches, path_hint_matches, BoostRule,
    HybridError, RrfConfig, MAX_BOOST_IDS,
};
use engine::kernel::{CpuScalarProvider, SearchInput};
use engine::policy::PolicyContext;
use engine::sparse::SparseIndex;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

#[path = "../src/test_util/seed_rows.rs"]
mod seed_rows;
use seed_rows::seed_rows_grouped_by_tenant;

const DIM: u32 = 2;
const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

/// `tests/soft_boost.rs` の `RANK3_TOP_K_ENTRY_BOOST` と同じ根拠（密ランク 1〜5・
/// `k_const=60.0` の合成コーパスで、密ランク 2 とのスコア差は上回るが密ランク 1 との
/// スコア差は下回る値）で、真の 1 位を追い越さないまま Top-k 入りだけを起こす。
const RANK3_TOP_K_ENTRY_BOOST: f64 = 0.0004;

/// 合成コーパス 1 行分（テナント境界列＋密ベクトル＋ヒント一致判定用メタデータ）。
///
/// `path`/`kind` は `sql/exec.rs` が可視行から構築する想定のメタデータであり、行ストア
/// （`Storage`）側の列としては持たせず、`tests/soft_boost.rs::Doc` と同じくテスト側の
/// グラウンドトゥルースとしてのみ保持する。
struct DocMeta {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    vector: [f32; 2],
    text: &'static str,
    path: &'static str,
    kind: &'static str,
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

fn ctx_a() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// path ヒント（`"src/hybrid"`）に一致する tenant-a 自身の可視行（id=3。密ランク 3 位、
/// `k=2` の枠外）と、**同一の数値 id（=5）** を持つ tenant-a の不可視ではない通常行、
/// および tenant-b の不可視行を含む合成コーパスを構築する。行 id の一意性スコープは
/// テナント内に閉じている（TABLE-12）ため、tenant-a の id=5 と tenant-b の id=5 は同一
/// テーブル内で正当に共存できる——この共存が V2 で検証する漏えい経路（テナントを
/// またいだ生の id だけでのブースト一致判定）の土台になる。
///
/// `collision_matches_hint` が真の場合のみ、tenant-b の id=5 行の path がヒントに一致
/// する（不可視な「たまたま一致する」データの有無を切り替えるためのフラグ。V2/V4 が
/// 使う）。tenant-b の id=10 行は ctx_a から可視（`Public`・他テナント）だが別 id・
/// ヒント非一致で、衝突源にはならない「正当な他テナント公開行混入」の対照に使う。
fn build_docs(collision_matches_hint: bool) -> Vec<DocMeta> {
    vec![
        DocMeta {
            id: 1,
            tenant: TENANT_A,
            visibility: Visibility::Public,
            vector: [1.00, 0.0],
            text: "alpha",
            path: "src/other/alpha.rs",
            kind: "code",
        },
        DocMeta {
            id: 2,
            tenant: TENANT_A,
            visibility: Visibility::Public,
            vector: [0.90, 0.0],
            text: "beta",
            path: "src/other/beta.rs",
            kind: "code",
        },
        // target: 正当な可視ヒント一致文書（`tests/soft_boost.rs` と同一のコーパス
        // 形状）。ブーストなしでは密ランク 3 位で `k=2` の枠外。
        DocMeta {
            id: 3,
            tenant: TENANT_A,
            visibility: Visibility::Public,
            vector: [0.80, 0.0],
            text: "gamma",
            path: "src/hybrid/target.rs",
            kind: "doc",
        },
        DocMeta {
            id: 4,
            tenant: TENANT_A,
            visibility: Visibility::Public,
            vector: [0.70, 0.0],
            text: "delta",
            path: "src/other/delta.rs",
            kind: "code",
        },
        // collision: tenant-a 自身のヒント非一致行。数値 id は tenant-b の id=5 行と
        // 一致する。
        DocMeta {
            id: 5,
            tenant: TENANT_A,
            visibility: Visibility::Public,
            vector: [0.60, 0.0],
            text: "epsilon",
            path: "src/other/five-a.rs",
            kind: "code",
        },
        // tenant-b（ctx_a から不可視: Private・他テナント）。密ランクは最上位相当だが
        // 不可視のため候補集合（`SearchInput`）には一切含まれない。
        DocMeta {
            id: 5,
            tenant: TENANT_B,
            visibility: Visibility::Private,
            vector: [0.99, 0.0],
            text: "zeta",
            path: if collision_matches_hint {
                "src/hybrid/five-invisible.rs"
            } else {
                "src/other/five-b.rs"
            },
            kind: "doc",
        },
        // tenant-b（ctx_a から可視: Public・他テナント）。別 id・ヒント非一致。
        DocMeta {
            id: 10,
            tenant: TENANT_B,
            visibility: Visibility::Public,
            vector: [0.01, 0.0],
            text: "eta",
            path: "src/other/ten.rs",
            kind: "code",
        },
    ]
}

/// `docs` を新規 `Storage` へ投入して返す（`CleanupGuard` は呼び出し元が `storage` より
/// 先に宣言し、drop 順で後始末する。`temp_db.rs` の Windows 向け注意事項参照）。
fn open_corpus(label: &str, docs: &[DocMeta]) -> (CleanupGuard, Storage) {
    let path = unique_db_path(label);
    let cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let rows: Vec<(u64, RowInput<'_>)> = docs
        .iter()
        .map(|d| {
            (
                d.id,
                RowInput {
                    tenant_id: d.tenant,
                    visibility: d.visibility,
                    embedding: &d.vector,
                    metadata: &[],
                },
            )
        })
        .collect();
    seed_rows_grouped_by_tenant(&storage, TABLE, &rows);
    (cleanup, storage)
}

/// RLS 事前フィルタ済み候補集合（`tenant::visible_rows` が返す `ctx` の可視行）から、
/// `hybrid_search_boosted` へそのまま渡せる `SearchInput` の材料（id 昇順に整列済み）を
/// 組み立てる。この関数の呼び出しそのものが「RLS 通過済み候補集合の構築」の実演で
/// あり、両機構の合流点を本ファイルの各テストが共有する唯一の経路にする。
fn rls_filtered_candidates(storage: &Storage, ctx: &PolicyContext) -> (Vec<u64>, Vec<f32>) {
    let mut rows = tenant::visible_rows(storage, TABLE, ctx).expect("visible_rows ok");
    rows.sort_by_key(|r| r.id);
    let ids: Vec<u64> = rows.iter().map(|r| r.id).collect();
    let vectors: Vec<f32> = rows.iter().flat_map(|r| r.embedding.clone()).collect();
    (ids, vectors)
}

/// `ids` に対応するテキストだけを含む疎インデックスを構築する。密側の `SearchInput`
/// と同じ RLS 済み候補集合からのみ構築することで、`hybrid.rs` モジュールドキュメント
/// が要求する「密・疎とも可視集合のみを母数にする」契約をテスト側でも踏襲する
/// （テナントをまたいで数値 id が衝突しうる本コーパスでは、可視集合の外側の文書を
/// 単一の疎インデックスへ混在させると `DocId` の衝突そのものが未定義になるため、
/// これは正確さのためにも必要）。クエリは常に `"zzz-no-match"` を使い（疎側を空にして
/// 密ランクだけで融合スコアが決まる構成にする。`tests/soft_boost.rs` と同じ設計）、
/// 疎スコアの寄与は本ファイルの検証対象外にする。
fn sparse_index_for(docs: &[DocMeta], ids: &[u64]) -> SparseIndex {
    let visible: BTreeSet<u64> = ids.iter().copied().collect();
    let refs: Vec<(u64, &str)> = docs
        .iter()
        .filter(|d| visible.contains(&d.id) && d.tenant == TENANT_A)
        .map(|d| (d.id, d.text))
        .collect();
    // tenant-a 以外の可視行（tenant-b の id=10）はテキストが疎インデックス側の候補には
    // 不要（クエリが一致しない前提のため、疎融合スコアは常に 0 のまま）。すべての
    // `ids` を疎インデックスへ含める必要はなく、`SparseIndex::build` は密側 `ids` の
    // 部分集合に対してのみ構築すれば `search_within` の可視集合縮約契約を満たす。
    SparseIndex::build(&refs).expect("sparse index build ok")
}

const QUERY_VECTOR: [f32; 2] = [1.0, 0.0];
const QUERY_TEXT: &str = "zzz-no-match";

/// `docs`（可視・不可視を問わずテスト側が保持する全メタデータ）のうち `ctx` から
/// 可視な行だけを対象に、`hint` に一致する id を集める——「規約」どおりの正しい
/// `BoostRule` 構築手順（`docs/design/plan-rls-boost-interaction.md` が固定する
/// 「`BoostRule` は RLS 通過済み可視行のメタデータのみから構築する」）。
fn correct_path_hint_ids(
    storage: &Storage,
    ctx: &PolicyContext,
    docs: &[DocMeta],
    hint: &str,
) -> BTreeSet<u64> {
    let visible = tenant::visible_rows(storage, TABLE, ctx).expect("visible_rows ok");
    let visible_keys: BTreeSet<(&str, u64)> = visible
        .iter()
        .map(|r| (r.tenant_id.as_str(), r.id))
        .collect();
    docs.iter()
        .filter(|d| visible_keys.contains(&(d.tenant, d.id)) && path_hint_matches(hint, d.path))
        .map(|d| d.id)
        .collect()
}

/// `correct_path_hint_ids` の**規約違反版**: `ctx` の可視性を一切考慮せず、`docs` に
/// 保持された全テナントのメタデータへヒントを適用してから id だけを集める（実装上の
/// バグ、または「グローバルなメタデータ索引からヒント一致 id を引いてしまう」実装
/// ミスを模する）。`BoostRule`／`apply_soft_boost` はテナント修飾のない生の `u64` id
/// でしか一致判定できないため、本関数が返す集合には他テナントの不可視行由来の id が
/// 紛れ込みうる——数値 id がテナントをまたいで衝突すると（TABLE-12）、可視な別行が
/// 意図せずブーストされる（V2 が検出する漏えい経路）。
fn naive_all_tenant_path_hint_ids(docs: &[DocMeta], hint: &str) -> BTreeSet<u64> {
    docs.iter()
        .filter(|d| path_hint_matches(hint, d.path))
        .map(|d| d.id)
        .collect()
}

fn boosted_hit_score(hits: &[engine::hybrid::HybridHit], id: u64) -> f64 {
    hits.iter()
        .find(|h| h.id == id)
        .unwrap_or_else(|| {
            panic!("id={id} must be present in hits (k must cover the full visible set)")
        })
        .score
}

// ---------------------------------------------------------------------------
// V1: 非バイパス（混入 0 件）。RLS 事前フィルタ済み候補集合にヒント由来ブーストを
// 適用しても、不可視行が Top-k に混入しないこと。ブーストルールの `ids` に不可視 id
// （テナントを越えて数値衝突する id を含む）が混入していても、可視集合外の行が
// 復活しないこと（プール外 id への加点は no-op）を検証する。検査器
// （`tenant::verify_hits`）はブースト・マスク実装と独立に行 id からテナント・可視性を
// 再計算するため、独立オラクルとして使う。検査器自体の実効性は、意図的に不可視 id を
// 混ぜた `hits` を渡す negative test で確かめる。
// ---------------------------------------------------------------------------
#[test]
fn v1_boosted_rls_results_never_include_invisible_rows() {
    let docs = build_docs(true); // 最悪ケース: tenant-b の衝突行がヒントにも一致する。
    let (_cleanup, storage) = open_corpus("plan-rls-boost-v1", &docs);
    let ctx = ctx_a();

    let (ids, vectors) = rls_filtered_candidates(&storage, &ctx);
    // 可視集合は tenant-a の id=1..5（自テナント）と tenant-b の id=10（他テナント
    // `Public`）の 6 件で、tenant-b の不可視行（id=5・`Private`）は含まれない。
    assert_eq!(ids, vec![1, 2, 3, 4, 5, 10]);

    let index = sparse_index_for(&docs, &ids);
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");

    // わざと「規約違反」のルール（tenant-b の不可視行由来の id=5 も含む）を使う。
    let naive_ids = naive_all_tenant_path_hint_ids(&docs, "src/hybrid");
    assert_eq!(naive_ids, [3, 5].into_iter().collect::<BTreeSet<u64>>());
    let rule = BoostRule::new(&naive_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");

    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: DIM,
        query: &QUERY_VECTOR,
        k: 6,
    };
    let hits = hybrid_search_boosted(
        &CpuScalarProvider,
        input,
        &index,
        QUERY_TEXT,
        6,
        &cfg,
        &[rule],
    )
    .expect("hybrid search boosted ok");

    // 独立検証: 返る id はすべて `ids`（RLS 済み候補集合）に含まれる——ブーストが
    // 候補集合の外側から行を復活させることは構造的にできない（`apply_soft_boost` は
    // 既存 `Vec` の `score` 更新のみで要素の追加・削除を行わない）。
    for hit in &hits {
        assert!(
            ids.contains(&hit.id),
            "boosted hit id={} must stay within the RLS-filtered candidate pool",
            hit.id
        );
    }

    // 独立検証: `tenant::verify_hits` で `(tenant_id, id)` 完全一致による再照合を行う。
    // `hybrid::HybridHit` はテナント修飾を持たないため、`ids`/`vectors` の構築時に
    // 使った `tenant::visible_rows` の結果からテナントを引き直して `SearchHit` へ
    // 変換する（本コーパスでは ctx_a の可視集合内で id が重複しないため一意に引ける）。
    let visible = tenant::visible_rows(&storage, TABLE, &ctx).expect("visible_rows ok");
    let search_hits: Vec<engine::kernel::SearchHit> = hits
        .iter()
        .map(|h| {
            let row = visible
                .iter()
                .find(|r| r.id == h.id)
                .expect("hit id must resolve to a visible row");
            engine::kernel::SearchHit::new(row.tenant_id.clone(), h.id, h.score as f32)
        })
        .collect();
    tenant::verify_hits(&storage, TABLE, &ctx, &search_hits)
        .expect("verify_hits must accept boosted RLS-filtered hits");

    // negative test: 検査器自体が実際に fail しうることを確認する（「実装と検査器の
    // 経路分離＋fail しうることの確認」原則。security.md）。tenant-b の不可視行
    // （id=5・Private）を可視集合内であるかのように偽装した `SearchHit` を混ぜると、
    // `verify_hits` は `(tenant_id, id)` の完全一致で確実に拒否する。
    let mut tampered = search_hits.clone();
    tampered.push(engine::kernel::SearchHit::new(TENANT_B, 5, 0.0));
    assert!(matches!(
        tenant::verify_hits(&storage, TABLE, &ctx, &tampered),
        Err(tenant::TenantError::HitOutsideVisibleSet)
    ));
}

// ---------------------------------------------------------------------------
// V2: 存在情報の非漏えい（応答同一性）。`BoostRule` を可視行のメタデータのみから
// 構築する規約の下では、「不可視行が存在するコーパス」と「可視行のみの同一
// コーパス」とで結果（順位・スコア）が完全一致する。逆に、規約に反して不可視行の
// メタデータからルールを構築すると差が生じることを negative test で示し、
// 「`BoostRule` は RLS 通過済み可視行のメタデータのみから構築する」を統合時の
// 実装規約として固定する。
// ---------------------------------------------------------------------------
#[test]
fn v2_rule_from_visible_metadata_only_is_invariant_to_invisible_matches() {
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");
    let ctx = ctx_a();
    let k = 6;

    // corpus A: tenant-b の衝突行（id=5）がヒントに一致する（不可視の「たまたま一致
    // する」データが存在するケース）。
    let docs_with_invisible_match = build_docs(true);
    let (_cleanup_a, storage_a) = open_corpus("plan-rls-boost-v2-a", &docs_with_invisible_match);
    // corpus B: 同じ tenant-b 衝突行が存在するが、ヒントには一致しない（不可視データ
    // 自体は存在するが「たまたま一致」しないケース）。
    let docs_without_invisible_match = build_docs(false);
    let (_cleanup_b, storage_b) = open_corpus("plan-rls-boost-v2-b", &docs_without_invisible_match);

    let run = |storage: &Storage, docs: &[DocMeta], rule_ids: &BTreeSet<u64>| {
        let (ids, vectors) = rls_filtered_candidates(storage, &ctx);
        let index = sparse_index_for(docs, &ids);
        let rule = BoostRule::new(rule_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM,
            query: &QUERY_VECTOR,
            k,
        };
        hybrid_search_boosted(
            &CpuScalarProvider,
            input,
            &index,
            QUERY_TEXT,
            k,
            &cfg,
            &[rule],
        )
        .expect("hybrid search boosted ok")
    };

    // 正しい構築（可視メタデータのみ）: 両コーパスとも tenant-a 自身の id=3 だけが
    // 一致し、ルール自体が不可視データの有無に依存しない。
    let correct_ids_a =
        correct_path_hint_ids(&storage_a, &ctx, &docs_with_invisible_match, "src/hybrid");
    let correct_ids_b = correct_path_hint_ids(
        &storage_b,
        &ctx,
        &docs_without_invisible_match,
        "src/hybrid",
    );
    assert_eq!(correct_ids_a, [3].into_iter().collect::<BTreeSet<u64>>());
    assert_eq!(correct_ids_a, correct_ids_b);

    let hits_a_correct = run(&storage_a, &docs_with_invisible_match, &correct_ids_a);
    let hits_b_correct = run(&storage_b, &docs_without_invisible_match, &correct_ids_b);
    // 応答同一性: 不可視行のヒント一致有無（`collision_matches_hint`）にかかわらず、
    // 可視行の順位・スコアは完全一致する。
    assert_eq!(
        hits_a_correct, hits_b_correct,
        "correct (visible-only) rule construction must be invariant to invisible matches"
    );

    // negative test: 規約違反（全テナントのメタデータから構築）だと、不可視行の
    // ヒント一致有無で tenant-a 自身の id=5 のスコアが変わってしまう——存在情報の
    // 漏えい経路そのものを再現する。
    let naive_ids_a = naive_all_tenant_path_hint_ids(&docs_with_invisible_match, "src/hybrid");
    let naive_ids_b = naive_all_tenant_path_hint_ids(&docs_without_invisible_match, "src/hybrid");
    assert_eq!(naive_ids_a, [3, 5].into_iter().collect::<BTreeSet<u64>>());
    assert_eq!(naive_ids_b, [3].into_iter().collect::<BTreeSet<u64>>());

    let hits_a_naive = run(&storage_a, &docs_with_invisible_match, &naive_ids_a);
    let hits_b_naive = run(&storage_b, &docs_without_invisible_match, &naive_ids_b);
    let score_a = boosted_hit_score(&hits_a_naive, 5);
    let score_b = boosted_hit_score(&hits_b_naive, 5);
    assert!(
        score_a > score_b,
        "regression: naive (tenant-blind) rule construction must leak the presence of an \
         invisible matching row via tenant-a's own id=5 score (score_a={score_a} score_b={score_b})"
    );
    let diff = score_a - score_b;
    assert!(
        (diff - RANK3_TOP_K_ENTRY_BOOST).abs() < 1e-9,
        "the leaked score delta must equal exactly one rule's boost amount: diff={diff}"
    );
}

// ---------------------------------------------------------------------------
// V3: ブースト効果の保存。RLS 事前フィルタ下でも、可視行に対するブースト本来の
// 効果（圏外候補の Top-k 浮上・ハードフィルタ化しないこと・空ルール時の
// `hybrid_search` との完全一致）が RLS なし構成（`tests/soft_boost.rs`）と定性一致
// すること。`path_hint`/`kind_hint` の両方を経由させる。
// ---------------------------------------------------------------------------
#[test]
fn v3_boost_effect_is_preserved_under_rls_prefilter() {
    let docs = build_docs(true);
    let (_cleanup, storage) = open_corpus("plan-rls-boost-v3", &docs);
    let ctx = ctx_a();
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");

    let (ids, vectors) = rls_filtered_candidates(&storage, &ctx);
    let index = sparse_index_for(&docs, &ids);

    // ベースライン（ブーストなし）: 密ランク上位 2 件（id=1, id=2）が Top-k で、
    // id=3 は枠外。
    let plain_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: DIM,
        query: &QUERY_VECTOR,
        k: 2,
    };
    let plain = hybrid_search(&CpuScalarProvider, plain_input, &index, QUERY_TEXT, 2, &cfg)
        .expect("hybrid search ok");
    let plain_ids: Vec<u64> = plain.iter().map(|h| h.id).collect();
    assert_eq!(plain_ids, vec![1, 2]);

    // path_hint 経由: 正しく構築したルール（id=3 のみ）で Top-k 浮上（ハードフィルタ
    // 化しない: 候補集合の要素数はブースト前後で不変、k=2 のまま）。
    let path_ids = correct_path_hint_ids(&storage, &ctx, &docs, "src/hybrid");
    let path_rule = BoostRule::new(&path_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");
    let boosted_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: DIM,
        query: &QUERY_VECTOR,
        k: 2,
    };
    let boosted = hybrid_search_boosted(
        &CpuScalarProvider,
        boosted_input,
        &index,
        QUERY_TEXT,
        2,
        &cfg,
        &[path_rule],
    )
    .expect("hybrid search boosted ok");
    let boosted_ids: Vec<u64> = boosted.iter().map(|h| h.id).collect();
    assert!(
        boosted_ids.contains(&3),
        "path_hint boosted doc must enter top-k under RLS prefilter: {boosted_ids:?}"
    );

    // kind_hint 経由（同一の可視ターゲット id=3・`kind="doc"`。他の可視行はすべて
    // `kind="code"`）: 同じ浮上効果を再現できること。
    let visible = tenant::visible_rows(&storage, TABLE, &ctx).expect("visible_rows ok");
    let visible_keys: BTreeSet<(&str, u64)> = visible
        .iter()
        .map(|r| (r.tenant_id.as_str(), r.id))
        .collect();
    let kind_ids: BTreeSet<u64> = docs
        .iter()
        .filter(|d| visible_keys.contains(&(d.tenant, d.id)) && kind_hint_matches("doc", d.kind))
        .map(|d| d.id)
        .collect();
    assert_eq!(kind_ids, [3].into_iter().collect::<BTreeSet<u64>>());
    let kind_rule = BoostRule::new(&kind_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");
    let kind_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: DIM,
        query: &QUERY_VECTOR,
        k: 2,
    };
    let kind_boosted = hybrid_search_boosted(
        &CpuScalarProvider,
        kind_input,
        &index,
        QUERY_TEXT,
        2,
        &cfg,
        &[kind_rule],
    )
    .expect("hybrid search boosted ok");
    assert!(kind_boosted.iter().any(|h| h.id == 3));

    // 空ルール時は `hybrid_search`（無ブースト）と完全一致すること。
    let empty_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: DIM,
        query: &QUERY_VECTOR,
        k: 6,
    };
    let empty_boosted = hybrid_search_boosted(
        &CpuScalarProvider,
        empty_input,
        &index,
        QUERY_TEXT,
        6,
        &cfg,
        &[],
    )
    .expect("hybrid search boosted ok");
    let full_plain_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: DIM,
        query: &QUERY_VECTOR,
        k: 6,
    };
    let full_plain = hybrid_search(
        &CpuScalarProvider,
        full_plain_input,
        &index,
        QUERY_TEXT,
        6,
        &cfg,
    )
    .expect("hybrid search ok");
    assert_eq!(empty_boosted, full_plain);
}

// ---------------------------------------------------------------------------
// V4: 追加損失ゼロ（RLS-4 類推）。RLS 事前フィルタ下での可視行に対するブースト結果が、
// 「可視行のみコーパスに同一ブーストを適用した理論値」と完全一致し、両機構の併用に
// よる追加の Recall 損失がないこと。「可視行のみコーパス」は `ctx_a` から可視な行
// （tenant-a 自身の全行 ＋ tenant-b の `Public` 行 id=10）だけを含む縮小 `Storage` として
// 構築し（tenant-b の不可視行 id=5 のみを除いた、真に「ちょうど可視集合と同型」の
// コーパス）、tenant-b の不可視行を含む完全なコーパスに対して RLS フィルタを通した
// 経路と比較する。
// ---------------------------------------------------------------------------
#[test]
fn v4_no_additional_loss_versus_visible_only_corpus() {
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");
    let ctx = ctx_a();
    let k = 6;

    // full: tenant-a・tenant-b 混在（tenant-b の不可視行がヒントにも一致する
    // 最悪ケース）。
    let full_docs = build_docs(true);
    let (_cleanup_full, storage_full) = open_corpus("plan-rls-boost-v4-full", &full_docs);

    // reduced: `ctx_a` から可視な行だけ（tenant-b の不可視行 id=5 のみを除いた、
    // ちょうど可視集合と同型の「理論上の可視行のみコーパス」）。
    let reduced_docs: Vec<DocMeta> = build_docs(true)
        .into_iter()
        .filter(|d| ctx.is_visible(d.tenant, d.visibility))
        .collect();
    let (_cleanup_reduced, storage_reduced) =
        open_corpus("plan-rls-boost-v4-reduced", &reduced_docs);

    let run = |storage: &Storage, docs: &[DocMeta]| {
        let (ids, vectors) = rls_filtered_candidates(storage, &ctx);
        let index = sparse_index_for(docs, &ids);
        let rule_ids = correct_path_hint_ids(storage, &ctx, docs, "src/hybrid");
        let rule = BoostRule::new(&rule_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM,
            query: &QUERY_VECTOR,
            k,
        };
        hybrid_search_boosted(
            &CpuScalarProvider,
            input,
            &index,
            QUERY_TEXT,
            k,
            &cfg,
            &[rule],
        )
        .expect("hybrid search boosted ok")
    };

    // reduced は構造的に tenant-b の不可視行を持たないため、「不可視行が候補集合の
    // 縮約より前に候補選出・スコア計算へ一切関与しない」という契約が成り立つ限り、
    // full（RLS フィルタで縮約）と reduced（構造的に可視行のみ）の結果は完全一致する
    // はずである。id 単位の部分集合比較ではなく、返る `Vec<HybridHit>` 全体
    // （順序・件数・スコアすべて）の一致で検証する。
    let hits_full = run(&storage_full, &full_docs);
    let hits_reduced = run(&storage_reduced, &reduced_docs);
    assert_eq!(
        hits_full, hits_reduced,
        "RLS-filtered results over the full corpus must exactly match results computed \
         directly over a corpus containing only ctx_a-visible rows (no additional loss or \
         perturbation from the presence of tenant-b's invisible row)"
    );
}

// ---------------------------------------------------------------------------
// V5: エラー契約の非依存性。`BoostRule::new`/`apply_soft_boost` の拒否エラー
// （`TooManyBoostIds`・`BoostSoftBoundExceeded` 等）の発生有無が不可視行の存在に
// 依存しないこと（エラー差分による存在情報漏えいの排除）。`MAX_BOOST_IDS`
// （10,000 件）境界の実データ再現には大量の行投入が要るため、`BoostRule::new` は
// 実データではなく id 集合のサイズのみを検証する契約（`hybrid.rs` ドキュメント参照）
// であることを踏まえ、id 集合そのものを合成して境界を再現する（実データを介さず
// 契約の性質を検証する点は他の V1〜V4 と異なるが、対象は同じ `BoostRule::new`
// 契約であり本ファイルの検証範囲に含める）。
// ---------------------------------------------------------------------------
#[test]
fn v5_boost_id_limit_error_must_not_depend_on_invisible_row_count() {
    // 正しい構築（可視メタデータのみ）: 不可視行のヒント一致有無（`collision_matches_hint`
    // で切り替わる、実際に存在・不在する不可視データ）を変えても、可視メタデータのみ
    // から求めた一致 id 集合そのものが不変であることをまず固定する（V2 の
    // `correct_path_hint_ids` と同じ規約）。`BoostRule::new` の成否はこの集合だけで
    // 決まるため、集合が不変なら成否も不可視データの有無・件数に依存しない。
    let ctx = ctx_a();
    let docs_with_invisible_match = build_docs(true);
    let (_cleanup_with, storage_with) =
        open_corpus("plan-rls-boost-v5-with-match", &docs_with_invisible_match);
    let docs_without_invisible_match = build_docs(false);
    let (_cleanup_without, storage_without) = open_corpus(
        "plan-rls-boost-v5-without-match",
        &docs_without_invisible_match,
    );

    let correct_ids_with = correct_path_hint_ids(
        &storage_with,
        &ctx,
        &docs_with_invisible_match,
        "src/hybrid",
    );
    let correct_ids_without = correct_path_hint_ids(
        &storage_without,
        &ctx,
        &docs_without_invisible_match,
        "src/hybrid",
    );
    assert_eq!(
        correct_ids_with, correct_ids_without,
        "correct (visible-only) rule ids must be invariant to whether an invisible row \
         happens to match the hint"
    );
    assert!(
        BoostRule::new(&correct_ids_with, RANK3_TOP_K_ENTRY_BOOST).is_ok(),
        "correct (visible-only) rule construction must succeed regardless of invisible matches"
    );

    // 規約違反の構築（全テナント走査）を模する: 可視一致 id（1 件）に、不可視行から
    // 「たまたま一致した」と仮定する id を追加していく。追加件数が
    // `MAX_BOOST_IDS` を超えた時点で初めて `TooManyBoostIds` が発生する——つまり
    // エラーの発生有無が不可視データの件数（存在情報）に依存してしまう、という
    // 規約違反時の実際の挙動を固定する。
    let mut naive_ids = correct_ids_with.clone();
    for extra in 0..MAX_BOOST_IDS {
        // 可視 id=3 と衝突しない合成 id（十分大きな値から採番）を「不可視行由来の
        // 一致 id」として追加する。
        naive_ids.insert(1_000_000 + extra as u64);
    }
    assert_eq!(naive_ids.len(), MAX_BOOST_IDS + 1);
    assert_eq!(
        BoostRule::new(&naive_ids, RANK3_TOP_K_ENTRY_BOOST).unwrap_err(),
        HybridError::TooManyBoostIds {
            len: MAX_BOOST_IDS + 1,
            max: MAX_BOOST_IDS,
        },
        "naive (tenant-blind) rule construction's error contract leaks the count of invisible \
         matching rows via whether TooManyBoostIds fires"
    );
    // 不可視行由来の「たまたま一致」の件数がちょうど境界未満なら、規約違反の構築でも
    // 拒否は発生しない——エラー発生有無そのものが不可視データの正確な件数に左右される
    // ことの再確認（1 件でも境界を割ればエラーが消える）。
    naive_ids.remove(&(1_000_000 + (MAX_BOOST_IDS as u64 - 1)));
    assert_eq!(naive_ids.len(), MAX_BOOST_IDS);
    assert!(BoostRule::new(&naive_ids, RANK3_TOP_K_ENTRY_BOOST).is_ok());
}
