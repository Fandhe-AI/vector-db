//! `VectorCore` / `PolicyContext` / provider 注入の結合テスト（TASK-124）。
//! 対象ビヘイビア: CORE-1（プロトコル非依存のコア API）・CORE-2（テナント境界の
//! 単一照合パス）・CORE-13（実行バックエンド provider 注入）。境界系（次元・k 検証）も含む。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use engine::core::{CoreError, EngineCore, VectorCore};
use engine::kernel::{CpuScalarProvider, KernelError, SearchHit, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::storage::{RowInput, Visibility};

/// 自動削除される一時ディレクトリ（`redb` ファイルの置き場）。外部クレートに
/// 依存しない最小実装（dependency-policy 準拠）。
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "engine-vector-core-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn db_path(&self) -> PathBuf {
        self.0.join("db.redb")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn schema_for(table_name: &str, dim: u32) -> engine::catalog::TableSchema {
    engine::catalog::TableSchema::new(
        table_name,
        vec![engine::catalog::ColumnDef::new(
            "embedding",
            engine::catalog::ColumnType::Vector(dim),
            false,
        )],
    )
}

fn seed_row(
    core: &EngineCore,
    table: &str,
    id: u64,
    tenant: &str,
    visibility: Visibility,
    embedding: &[f32],
) {
    core.storage()
        .insert_row_into_table(
            table,
            id,
            &RowInput {
                tenant_id: tenant,
                visibility,
                embedding,
                metadata: &[],
            },
        )
        .expect("seed row");
}

// 対象ビヘイビア: CORE-1。`&dyn VectorCore` のみを介する 2 種類の「模擬プロトコル
// アダプタ」が同一結果を得ることを検証する（コア API がプロトコル非依存であること）。
fn _assert_object_safe(_: &dyn VectorCore) {}

/// 組み込み呼び出し風アダプタ（プロトコルフレーミングなしで直接 trait を呼ぶ想定）。
fn embedded_adapter_search(
    core: &dyn VectorCore,
    ctx: &PolicyContext,
    table: &str,
    query: &[f32],
    k: usize,
) -> Vec<SearchHit> {
    core.search(ctx, table, query, k)
        .expect("embedded adapter search")
}

/// リクエスト/レスポンス変換風アダプタ（wire プロトコル層を模し、ID のみのバイト列を
/// 経由させてから復元する体で `VectorCore` を呼ぶ）。
fn wire_like_adapter_search(
    core: &dyn VectorCore,
    ctx: &PolicyContext,
    table: &str,
    query: &[f32],
    k: usize,
) -> Vec<u64> {
    let hits = core
        .search(ctx, table, query, k)
        .expect("wire-like adapter search");
    hits.into_iter().map(|h| h.id).collect()
}

#[test]
fn core1_two_protocol_adapters_agree_via_object_safe_trait() {
    let dir = TempDir::new("core1");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    seed_row(
        &core,
        "docs",
        2,
        "tenant-a",
        Visibility::Public,
        &[0.0, 1.0],
    );
    seed_row(
        &core,
        "docs",
        3,
        "tenant-a",
        Visibility::Public,
        &[2.0, 0.0],
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let core_ref: &dyn VectorCore = &core;

    let embedded = embedded_adapter_search(core_ref, &ctx, "docs", &[1.0, 0.0], 2);
    let wire_ids = wire_like_adapter_search(core_ref, &ctx, "docs", &[1.0, 0.0], 2);

    assert_eq!(embedded.iter().map(|h| h.id).collect::<Vec<_>>(), wire_ids);
    assert_eq!(wire_ids, vec![3, 1]);
}

/// 実装から独立した検査器: 検索結果の行 ID から `Storage` を直接再読取して
/// テナントを再計算する（`core.rs` の判定ロジックとは別経路で検証する。CORE-2）。
fn assert_no_cross_tenant_leak(
    core: &EngineCore,
    table: &str,
    expected_tenant: &str,
    hits: &[SearchHit],
) {
    for hit in hits {
        let row = core
            .storage()
            .get_row_from_table(table, hit.id)
            .expect("checker: row must exist for a returned hit");
        assert_eq!(
            row.tenant_id, expected_tenant,
            "cross-tenant leak detected: hit id={} belongs to tenant={}",
            hit.id, row.tenant_id
        );
    }
}

#[test]
fn core2_multi_tenant_search_has_zero_cross_tenant_leakage() {
    let dir = TempDir::new("core2-leak");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    // tenant-a と tenant-b を混在させ、tenant-b 側に最高スコアの行を置く
    // （マスクが効いていなければ tenant-a の検索結果に混入する）。
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    seed_row(
        &core,
        "docs",
        2,
        "tenant-a",
        Visibility::Public,
        &[0.5, 0.0],
    );
    seed_row(
        &core,
        "docs",
        3,
        "tenant-b",
        Visibility::Public,
        &[100.0, 0.0],
    );
    seed_row(
        &core,
        "docs",
        4,
        "tenant-b",
        Visibility::Public,
        &[50.0, 0.0],
    );

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let hits = core
        .search(&ctx_a, "docs", &[1.0, 0.0], 10)
        .expect("search ok");

    assert!(!hits.is_empty());
    assert_no_cross_tenant_leak(&core, "docs", "tenant-a", &hits);
    assert!(hits.iter().all(|h| h.id == 1 || h.id == 2));
}

#[test]
fn core2_get_row_does_not_distinguish_invisible_from_missing() {
    let dir = TempDir::new("core2-getrow");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-b",
        Visibility::Public,
        &[1.0, 0.0],
    );

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let other_tenant_row = core.get_row(&ctx_a, "docs", 1);
    let missing_row = core.get_row(&ctx_a, "docs", 999);

    assert!(matches!(other_tenant_row, Err(CoreError::NotFound)));
    assert!(matches!(missing_row, Err(CoreError::NotFound)));
}

// レビュー指摘対応（Medium・Issue #32）: `get_row` はテーブル不存在・行不存在のみを
// `CoreError::NotFound` に丸め込み、それ以外（デコード不正等のデータ破損）は
// `CoreError::Catalog` としてそのまま伝播しなければならない。`Storage`/`EngineCore` の
// 公開 API では意図的な破損データを作れないため、`tests/persistence.rs` と同じ手法で
// テーブル固有の行テーブルへ `redb` を直接操作して不正なバイト列を書き込む。
#[test]
fn get_row_surfaces_data_corruption_distinctly_from_not_found() {
    let dir = TempDir::new("core-corrupt");
    let path = dir.db_path();
    {
        let core = EngineCore::open(&path).expect("open engine core");
        core.storage()
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // `Storage`/`EngineCore` を閉じてから raw `redb::Database` で同一ファイルを
        // 再度開く（redb はプロセス内で同一ファイルへの多重 `Database` を許容しない）。
    }
    {
        let db = redb::Database::create(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            // `catalog.rs::user_rows_table_name` と同一の命名規則（`user_rows/<table>`）。
            let row_table_def: redb::TableDefinition<u64, &[u8]> =
                redb::TableDefinition::new("user_rows/docs");
            let mut table = write_txn.open_table(row_table_def).expect("open row table");
            // version バイトのみで後続フィールドが一切ない、意図的な破損バイト列。
            table
                .insert(1u64, &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let core = EngineCore::open(&path).expect("reopen engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .get_row(&ctx, "docs", 1)
        .expect_err("corrupted row must not be silently reported as NotFound");
    assert!(
        matches!(err, CoreError::Catalog(_)),
        "expected CoreError::Catalog (data corruption surfaced distinctly), got: {err}"
    );
}

/// P0-1 検証用の計装 provider（Issue #137）: `SearchInput` に渡された `ids`・
/// `vectors` の長さをそのまま記録してから `CpuScalarProvider` へ委譲する。かつて
/// `MaskIgnoringProvider`（`is_visible` クロージャを無視して全行を候補にする provider）
/// が実証していた「provider がマスクを無視すれば他テナント行を読める」という経路は、
/// `SearchInput` 自体に不可視行のデータを含めない設計（`kernel.rs` のドキュメント参照）
/// へ変更したことで構造的に塞がれた。本 provider は「コアが実際に可視行だけへ絞り込んだ
/// 縮約ビューを渡していること」を provider 側の観測で裏付ける。
struct CapturingProvider {
    captured_ids: Arc<Mutex<Vec<u64>>>,
    captured_vector_len: Arc<Mutex<usize>>,
}

impl SearchProvider for CapturingProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
        *self.captured_ids.lock().expect("lock captured_ids") = input.ids.to_vec();
        *self
            .captured_vector_len
            .lock()
            .expect("lock captured_vector_len") = input.vectors.len();
        CpuScalarProvider.search(input)
    }
}

// レビュー指摘対応（codex P0-1・Issue #137）: `EngineCore::search` が provider へ
// 渡す `SearchInput` に、他テナントの不可視行の id・ベクトルが一切含まれないこと
// （＝可視行だけへ絞り込んだ縮約ビューであること）を、provider 側の観測で検証する。
// dim=2・tenant-a の可視行が 1 件のみのデータセットに対し、`ids` が他テナント
// （id=2, tenant-b）を含まず、`vectors` の長さが可視行数 × dim（1 * 2 = 2）に
// 一致することを確認する。
#[test]
fn search_projects_input_to_visible_rows_only_before_calling_provider() {
    let dir = TempDir::new("p0-1-projection");
    let captured_ids = Arc::new(Mutex::new(Vec::new()));
    let captured_vector_len = Arc::new(Mutex::new(0usize));
    let provider = CapturingProvider {
        captured_ids: Arc::clone(&captured_ids),
        captured_vector_len: Arc::clone(&captured_vector_len),
    };
    let core =
        EngineCore::with_provider(dir.db_path(), Box::new(provider)).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    seed_row(
        &core,
        "docs",
        2,
        "tenant-b",
        Visibility::Public,
        &[100.0, 0.0],
    );

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let _ = core
        .search(&ctx_a, "docs", &[1.0, 0.0], 10)
        .expect("search ok");

    let ids = captured_ids.lock().expect("lock captured_ids");
    assert_eq!(
        ids.as_slice(),
        &[1u64],
        "provider must only observe the caller's own visible row ids"
    );
    let vector_len = *captured_vector_len
        .lock()
        .expect("lock captured_vector_len");
    assert_eq!(
        vector_len, 2,
        "provider must only observe vectors for the visible rows (1 row * dim 2)"
    );
}

/// negative test 用のダミー provider: `SearchInput` の内容を一切参照せず、対象
/// データセットに存在しない id を捏造した `SearchHit` を返す。provider が
/// テナント境界を無視するだけでなく、データセットに実在しない id をでっち上げる
/// ケースも `EngineCore::search` のコア側再検証が拒否できることを確認する
/// （codex P0 対応・Issue #137）。
struct FabricatingHitProvider;

impl SearchProvider for FabricatingHitProvider {
    fn search(&self, _input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
        Ok(vec![SearchHit {
            id: 9_999,
            score: 1.0,
        }])
    }
}

#[test]
fn provider_returning_a_hit_absent_from_the_dataset_is_rejected() {
    let dir = TempDir::new("provider-fabricated-hit");
    let core = EngineCore::with_provider(dir.db_path(), Box::new(FabricatingHitProvider))
        .expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 10)
        .expect_err("provider returning an id absent from the dataset must be rejected");

    assert!(
        matches!(err, CoreError::ProviderResultRejected),
        "expected ProviderResultRejected, got: {err}"
    );
}

/// CORE-13: 呼び出しがカスタム provider を通ることを記録する計装 provider。
struct RecordingProvider {
    calls: Arc<AtomicUsize>,
}

impl SearchProvider for RecordingProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        CpuScalarProvider.search(input)
    }
}

#[test]
fn core13_custom_provider_injection_is_actually_called() {
    let dir = TempDir::new("core13-inject");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = RecordingProvider {
        calls: Arc::clone(&calls),
    };
    let core =
        EngineCore::with_provider(dir.db_path(), Box::new(provider)).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let _ = core
        .search(&ctx, "docs", &[1.0, 0.0], 1)
        .expect("search ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let _ = core
        .search(&ctx, "docs", &[1.0, 0.0], 1)
        .expect("search ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn core13_default_cpu_only_constructor_supports_full_functionality() {
    let dir = TempDir::new("core13-default");
    let core = EngineCore::open(dir.db_path()).expect("open engine core (default CPU provider)");
    core.storage()
        .create_table(&schema_for("docs", 3))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0, 0.0],
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let hits = core
        .search(&ctx, "docs", &[1.0, 0.0, 0.0], 1)
        .expect("search ok");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 1);

    let row = core.get_row(&ctx, "docs", 1).expect("get_row ok");
    assert_eq!(row.tenant_id, "tenant-a");
}

#[test]
fn boundary_query_dimension_mismatch_is_rejected() {
    let dir = TempDir::new("boundary-dim");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 3))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0, 0.0],
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core.search(&ctx, "docs", &[1.0, 0.0], 1).unwrap_err();
    assert!(matches!(
        err,
        CoreError::Kernel(KernelError::DimMismatch { .. })
    ));
}

#[test]
fn boundary_k_zero_and_k_over_limit_are_rejected() {
    let dir = TempDir::new("boundary-k");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    assert!(matches!(
        core.search(&ctx, "docs", &[1.0, 0.0], 0),
        Err(CoreError::InvalidK { k: 0 })
    ));
    assert!(matches!(
        core.search(&ctx, "docs", &[1.0, 0.0], usize::MAX),
        Err(CoreError::InvalidK { .. })
    ));
}

// レビュー指摘対応（Medium 1・Issue #32）: query の次元不一致は
// `VectorArena::build`（対象テーブル全行のデコード・確保）へ進む前に、カタログ照会
// だけで早期拒否されなければならない。これを直接計測せずに検証するため、対象テーブルへ
// `redb` を直接操作してデコード不能な破損行を仕込んでおく。もし早期拒否が働かず
// `VectorArena::build` が実際に全行を走査してしまえば、この破損行のデコードで
// 別種のエラー（`CoreError::Arena`）になるはずで、期待どおり
// `CoreError::Kernel(KernelError::DimMismatch)` が返ることは走査が発生しなかった証拠になる。
#[test]
fn dim_mismatch_is_rejected_before_scanning_table_rows() {
    let dir = TempDir::new("dim-mismatch-early-reject");
    let path = dir.db_path();
    {
        let core = EngineCore::open(&path).expect("open engine core");
        core.storage()
            .create_table(&schema_for("docs", 3))
            .expect("create table");
    }
    {
        let db = redb::Database::create(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let row_table_def: redb::TableDefinition<u64, &[u8]> =
                redb::TableDefinition::new("user_rows/docs");
            let mut table = write_txn.open_table(row_table_def).expect("open row table");
            // version バイトのみで後続フィールドが一切ない、意図的な破損バイト列
            // （`get_row_surfaces_data_corruption_distinctly_from_not_found` と同手法）。
            table
                .insert(1u64, &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let core = EngineCore::open(&path).expect("reopen engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // schema の次元（3）と異なる次元（2）のクエリを渡す。
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 1)
        .expect_err("dim mismatch must be rejected");
    assert!(
        matches!(
            err,
            CoreError::Kernel(KernelError::DimMismatch {
                expected: 3,
                found: 2
            })
        ),
        "expected early DimMismatch rejection (proving the malformed row was never scanned), got: {err}"
    );
}

// レビュー指摘対応（Cursor Bugbot Medium・Issue #32 #137）: 正しい次元だが非有限
// （NaN・Inf）の要素を含む query も、`dim_mismatch_is_rejected_before_scanning_table_rows`
// と同じ手法（破損行を仕込んだテーブル）で `VectorArena::build` へ進む前に早期拒否
// されなければならない。早期拒否が働かず全行走査してしまえば、この破損行のデコードで
// 別種のエラー（`CoreError::Arena`）になるはずで、期待どおり
// `CoreError::Kernel(KernelError::NonFiniteQuery)` が返ることは走査が発生しなかった証拠になる。
#[test]
fn non_finite_query_is_rejected_before_scanning_table_rows() {
    let dir = TempDir::new("non-finite-early-reject");
    let path = dir.db_path();
    {
        let core = EngineCore::open(&path).expect("open engine core");
        core.storage()
            .create_table(&schema_for("docs", 3))
            .expect("create table");
    }
    {
        let db = redb::Database::create(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let row_table_def: redb::TableDefinition<u64, &[u8]> =
                redb::TableDefinition::new("user_rows/docs");
            let mut table = write_txn.open_table(row_table_def).expect("open row table");
            // version バイトのみで後続フィールドが一切ない、意図的な破損バイト列
            // （`get_row_surfaces_data_corruption_distinctly_from_not_found` と同手法）。
            table
                .insert(1u64, &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let core = EngineCore::open(&path).expect("reopen engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // schema の次元（3）とは一致するが NaN を含むクエリを渡す。
    let err = core
        .search(&ctx, "docs", &[1.0, f32::NAN, 0.0], 1)
        .expect_err("non-finite query must be rejected");
    assert!(
        matches!(err, CoreError::Kernel(KernelError::NonFiniteQuery)),
        "expected early NonFiniteQuery rejection (proving the malformed row was never scanned), got: {err}"
    );
}

// レビュー指摘対応（Medium 2・Issue #32）: `search`・`get_row` はいずれもテーブル不存在を
// `CoreError::NotFound` へ丸め込み、他テナントの存在情報を漏らさない契約で統一する
// （security.md「アクセス制御の不備」）。
#[test]
fn search_and_get_row_agree_on_not_found_for_missing_table() {
    let dir = TempDir::new("missing-table-symmetry");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let search_err = core
        .search(&ctx, "not_registered", &[1.0, 0.0], 1)
        .expect_err("search on missing table must fail");
    let get_row_err = core
        .get_row(&ctx, "not_registered", 1)
        .expect_err("get_row on missing table must fail");

    assert!(matches!(search_err, CoreError::NotFound));
    assert!(matches!(get_row_err, CoreError::NotFound));
}

// レビュー指摘対応（Low・Issue #32）。`ctx` が `Public` のみ許可（既定コンストラクタ）の
// 場合、同一テナントの `Visibility::Private` 行は `search` 結果から除外されること
// （CORE-2: 可視性ラベル評価が正しく `search` 経路を通っていることの統合テスト）。
#[test]
fn search_excludes_private_rows_of_the_same_tenant_when_ctx_disallows_private() {
    let dir = TempDir::new("private-visibility-excluded");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    // Private 行がクエリに最も近い（マスクが効いていなければ検索結果の先頭に来る）。
    seed_row(
        &core,
        "docs",
        1,
        "tenant-a",
        Visibility::Private,
        &[1.0, 0.0],
    );
    seed_row(
        &core,
        "docs",
        2,
        "tenant-a",
        Visibility::Public,
        &[0.5, 0.0],
    );

    // 既定コンストラクタは Public のみ許可（`PolicyContext::new` のドキュメント参照）。
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let hits = core
        .search(&ctx, "docs", &[1.0, 0.0], 10)
        .expect("search ok");

    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);

    // `Visibility::Private` を明示許可すれば結果に含まれるようになることも併せて確認する
    // （マスク自体が機能していることの対照実験）。
    let ctx_with_private =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let hits_with_private = core
        .search(&ctx_with_private, "docs", &[1.0, 0.0], 10)
        .expect("search ok");
    assert_eq!(
        hits_with_private.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn boundary_empty_table_search_returns_empty_results() {
    let dir = TempDir::new("boundary-empty");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    core.storage()
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let hits = core
        .search(&ctx, "docs", &[1.0, 0.0], 5)
        .expect("search ok");
    assert!(hits.is_empty());
}
