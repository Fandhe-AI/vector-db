//! `POST /v1/query` の op 語彙検証・拒否契約（Issue #774。対象ビヘイビア
//! TASK-179・NOSQL-1・NOSQL-9。ポインタ: `docs/spec/05-tasks.md` TASK-179・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1・NOSQL-9）の層 A テスト
//! 群。4 op（`search`／`scan`／`aggregate`／`insert`）はいずれも実行結線済み
//! （TASK-186・NOSQL-2〜6・Issue #764・#766・#768・#772）のため、本ファイルは
//! **seed 済み 2 テナント fixture** 上で以下を固定する。
//!
//! ## 既存テストとの役割分担
//!
//! - `nosql9_op_allowlist.rs`（Issue #759）: スローアウェイ core 上で 4 op の
//!   engine 到達（`42P01`）・語彙外 16 ケースの `0A000`・op 判定がスキーマ
//!   検証より先であること・未知キー 15 ケースの `42601`・`op` 欠落／非文字列／
//!   非オブジェクトの回帰・非漏えいを固定する。本ファイルはこれらを再検証
//!   しない
//! - `nosql1_endpoint_routing.rs`: NOSQL-1 の 3 エンドポイント限定・未知パス／
//!   非 `POST` の `08P01`・ルーティングと認証／本文検証の順序
//! - `nosql2_search.rs`・`nosql3_*`・`nosql4_*`・`nosql6_*`: 各 op の写像意味論
//!   （SQL パリティ・precision・`operation_id` 契約 等）
//!
//! 本ファイルが新たに固定するのは、(a) seed 済み fixture での 4 op **成功
//! 応答**（受理の非 vacuous 証跡）、(b) 語彙外 op の `0A000` が seed 済み・
//! 実行可能な状態でも不変で副作用を持たないこと、(c) RLS 段を狙う JSON 上の
//! 各フィールドが `42601` で拒否され、同じ本文からそのキーだけを外した対照が
//! 200 かつ RLS clean であること（拒否がキー起因である非 vacuous 証跡）、
//! (d) 拒否要求がセッションを消費・失効させないこと、の 4 点。
//!
//! ## RLS-5 の観測境界
//!
//! NoSQL 表層には `HINT ORDER` を受理する経路が存在しない（宣言していない
//! フィールドは [`crate::http::query::schema::ObjectSchema`] の未知キー検証
//! により `42601` で拒否される）ため、engine 側の安全網本体
//! （`crates/engine/src/rls.rs::RlsSafetyNet`・`sql/exec.rs` の無条件適用。
//! 対象ビヘイビア RLS-5・RLS-7・RLS-8）を wire 越しに単独で発火させることは
//! できない。本ファイルは「RLS 段を狙いうる JSON 上のすべてのつまみが
//! `42601` で拒否される、または受理されても不可視行が一切混入しない（RLS
//! clean）」ことのみを固定する。安全網そのものの確定オラクルは engine 側
//! （`crates/engine/tests/sql_evaluation_order.rs`・`rls.rs` の
//! `RlsSafetyNet` テスト）と SQL 表層 wire テスト（`tests/wire_hint_order.rs`）
//! に委ねる（`nosql3_scan_wire_parity.rs` の早期終了に関する書き方と同型）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
// `temp_db` は `http_common` が `pub mod temp_db;` として再エクスポートする
// ため、ここでは独自に `mod temp_db;` を宣言しない
// （`clippy::duplicate_mod` 回避。`http_common/mod.rs` のコメント参照）。
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::gate::UNSUPPORTED_OP_MESSAGE;
use wire_server::http::session::store::SessionStore;
use wire_server::limits::SESSION_TTL;

const TABLE: &str = "docs";

/// 距離 0 の罠行（他テナント Private・自テナント Private）の id。wire
/// ログインが導出する `PolicyContext`（`auth::verify` → `PolicyContext::new`）
/// は Public のみを許可可視性とするため、いずれも受理された応答に混入して
/// はならない。
const TRAP_IDS: [u64; 2] = [99, 100];

/// 罠行を一意に識別するセンチネル（`lang` 列。短い値は他の語の部分文字列と
/// 誤一致しうるため一意な値を使う）。
const TRAP_SENTINELS: [&str; 2] = ["trap-b-99", "trap-a-100"];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// `docs`（`embedding VECTOR(2)` + `lang TEXT`）へ tenant-a の Public 行 3 件
/// と、クエリベクトル `[1.0,0.0]` に距離 0 で一致する Private 罠行 2 件
/// （他テナント tenant-b の id=99・自テナント tenant-a の id=100）を仕込む
/// （`tests/wire_hint_order.rs::new_core_two_tenant_docs` と同型の構成）。
fn new_core_two_tenant_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql1-op-vocabulary-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let public_rows: [(u64, [f32; 2], &str); 3] = [
        (1, [0.9, 0.1], "ja"),
        (2, [0.0, 1.0], "ja"),
        (3, [0.1, 0.9], "en"),
    ];
    for (id, emb, lang) in public_rows {
        let op_id = OperationId::parse(&format!("nosql1-seed-a-{id}")).expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert tenant-a public row");
    }

    // tenant-b の Private 罠行（他テナント・距離 0）。
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_b,
        99,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("trap-b-99".to_string()),
        ],
        &OperationId::parse("nosql1-seed-trap-b-99").expect("valid operation_id"),
    )
    .expect("insert tenant-b trap row");

    // tenant-a 自身の Private 罠行（wire ctx には許可可視性として付与されて
    // いないため、自テナントであっても見えてはならない。距離 0）。
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
        100,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("trap-a-100".to_string()),
        ],
        &OperationId::parse("nosql1-seed-trap-a-100").expect("valid operation_id"),
    )
    .expect("insert tenant-a private trap row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `alice`（tenant-a）のみを持つユーザーストアを添えて production ルータを
/// 起動する（1 `#[test]` = 1 個の新規 core・リスナーの流儀。`nosql2_search.rs`
/// と同じ）。
fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core)
}

/// [`spawn`] と同じだが、セッションストアの同時枠を `max_sessions` 個に
/// 制限する（T4: 拒否要求がセッションを消費しないことを固定するため、枠 1
/// でログインを使い回す必要がある）。
fn spawn_with_session_limit(core: Arc<EngineCore>, max_sessions: usize) -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::with_limits(max_sessions, SESSION_TTL),
        core,
    )
}

fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn post(addr: SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
    let auth_header = format!("Bearer {token}");
    let content_length = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_header),
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// テナントの Bearer トークンを都度発行し `/v1/query` へ本文を送る便宜 API
/// （セッション枠を使い切らないよう毎回新規ログインする。`nosql9_op_allowlist.rs::
/// query` と同型）。
fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

/// `{"columns":[{"name":...}],"rows":[[...]],"row_count":n}` 形の成功応答を
/// `(columns, rows)` へ解析する。応答が別の形（成功本文の形が違う op、
/// エラー応答等）であれば `None` を返す（`assert_rls_clean` が best-effort に
/// 使うための緩い解析。`nosql3_scan_wire_parity.rs::parse_body_str` の
/// 部分集合）。
fn parse_columns_rows(text: &str) -> Option<(Vec<String>, Vec<Vec<JsonValue>>)> {
    let JsonValue::Object(top) = parse_json(text).ok()? else {
        return None;
    };
    let columns = match top.get("columns") {
        Some(JsonValue::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                JsonValue::Object(col) => match col.get("name") {
                    Some(JsonValue::String(s)) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    let rows = match top.get("rows") {
        Some(JsonValue::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                JsonValue::Array(cells) => Some(cells.clone()),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    Some((columns, rows))
}

/// 応答が RLS clean であることを検証する: (1) 本文にいずれの罠センチネルも
/// 現れない、(2) `tenant-` マーカーが現れない、(3) 200 応答かつ `columns` に
/// `id` があれば、いずれの行の `id` も [`TRAP_IDS`] を含まない（`search`／
/// `scan` の行集合応答を解析ベースで検査する。`aggregate`／`insert` の成功
/// 応答には `id` 列が無いため (3) は無害に skip される）。
fn assert_rls_clean(resp: &HttpResponse) {
    let text = body_utf8(resp);
    for sentinel in TRAP_SENTINELS {
        assert!(
            !text.contains(sentinel),
            "response leaked trap sentinel {sentinel:?}: {text}"
        );
    }
    assert!(
        !text.contains("tenant-"),
        "response leaked tenant id marker: {text}"
    );
    if resp.status == 200 {
        if let Some((columns, rows)) = parse_columns_rows(&text) {
            if let Some(id_idx) = columns.iter().position(|c| c == "id") {
                for row in &rows {
                    if let Some(JsonValue::Number(n)) = row.get(id_idx) {
                        let id = n.as_f64() as u64;
                        assert!(
                            !TRAP_IDS.contains(&id),
                            "trap id {id} leaked in response: {text}"
                        );
                    }
                }
            }
        }
    }
}

// --- T1: 4 op が受理され成功応答を返す（受入 1） --------------------------

#[test]
fn all_four_ops_succeed_against_seeded_fixture_with_distinct_bodies() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    // `columns` に `scan_resp`（`["id"]` のみ）と異なる列集合 `["id","lang"]`
    // を指定する。scan は順序保証のない契約（Issue #831 レビュー指摘）のため、
    // 行順序だけに頼った `assert_ne!` は「scan がたまたま search と同じ順序を
    // 返す」正当な実装でも失敗しうる。列集合そのものを変えることで、行順序に
    // 依存せず本文が構造的に異なることを保証する。
    let search_resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id","lang"]}"#,
    );
    assert_eq!(search_resp.status, 200, "search resp={search_resp:?}");
    assert!(
        body_utf8(&search_resp).contains(r#""row_count":3"#),
        "{}",
        body_utf8(&search_resp)
    );
    assert_rls_clean(&search_resp);

    let scan_resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(scan_resp.status, 200, "scan resp={scan_resp:?}");
    assert!(
        body_utf8(&scan_resp).contains(r#""row_count":3"#),
        "{}",
        body_utf8(&scan_resp)
    );
    assert!(
        !body_utf8(&scan_resp).contains(r#""score""#),
        "scan projection must not include score: {}",
        body_utf8(&scan_resp)
    );
    assert_rls_clean(&scan_resp);

    let aggregate_resp = query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
    );
    assert_eq!(
        aggregate_resp.status, 200,
        "aggregate resp={aggregate_resp:?}"
    );
    // 可視行（Public のみ）3 件を数え、罠行 2 件（Private）を含まない。
    assert!(
        body_utf8(&aggregate_resp).contains("[3]"),
        "{}",
        body_utf8(&aggregate_resp)
    );
    assert_rls_clean(&aggregate_resp);

    let insert_resp = query_as_alice(
        addr,
        br#"{"op":"insert","table":"docs","rows":[{"id":500,"embedding":[0.2,0.3]}],"operation_id":"nosql1-t1-insert"}"#,
    );
    assert_eq!(insert_resp.status, 200, "insert resp={insert_resp:?}");
    assert_eq!(
        body_utf8(&insert_resp),
        r#"{"inserted":1,"operation_id":"nosql1-t1-insert"}"#
    );
    assert_rls_clean(&insert_resp);

    // 4 応答本文が互いに異なることで固定応答への縮退を排除する。
    let bodies = [
        body_utf8(&search_resp),
        body_utf8(&scan_resp),
        body_utf8(&aggregate_resp),
        body_utf8(&insert_resp),
    ];
    for i in 0..bodies.len() {
        for j in (i + 1)..bodies.len() {
            assert_ne!(
                bodies[i], bodies[j],
                "response bodies must differ: {bodies:?}"
            );
        }
    }

    // insert の永続化を確認する。NoSQL 表層の `insert` は SQL-10 `execute_insert`
    // と同じく常に `Visibility::Private` 固定で書き込み、wire 認証が導出する
    // `PolicyContext`（Public のみ許可）はその行を読み戻せない（既知の非対称。
    // `docs/design/three-client-e2e-harness.md`「非対称」節・
    // `nosql6_tenant_row_id_scope.rs::insert_success_body_has_exact_shape_and_is_persisted`
    // と同じ検証方法）。同一 wire scan（Public のみ）で `row_count` が不変の
    // ままであることをまず確認したうえで、保持している `Arc<EngineCore>` から
    // `Private` を含む ctx で直接 `SELECT` して非 vacuous な永続化証跡を得る。
    let scan_after_insert = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(scan_after_insert.status, 200, "resp={scan_after_insert:?}");
    assert!(
        body_utf8(&scan_after_insert).contains(r#""row_count":3"#),
        "wire scan (Public のみ) は Private 行 id=500 を読み戻さない: {}",
        body_utf8(&scan_after_insert)
    );
    assert_rls_clean(&scan_after_insert);

    let ctx_a_with_private =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-a ctx");
    let mut session = engine::sql::mode::SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx_a_with_private,
            &mut session,
            "SELECT id FROM docs LIMIT 10",
        )
        .expect("select ok");
    let engine::sql::SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    assert!(
        result.rows.iter().any(|row| row.id == 500),
        "inserted row id=500 must be persisted (Private) and visible with Private ctx: {:?}",
        result.rows
    );
}

// --- T2: 語彙外 op は seed 済み・実行可能状態でも 0A000 で副作用なし（受入 2） ---

#[test]
fn vocabulary_outside_four_ops_rejects_with_0a000_and_has_no_side_effect() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    // `nosql9_op_allowlist.rs` と同じ語彙外集合。受理形（`vector`／`limit`
    // 等）の残りフィールドを備えた本文で送り、「op 判定が実行可能な状態でも
    // 手前で止まる」ことを固定する。
    let unsupported_ops = [
        "create_table",
        "alter_table",
        "drop_table",
        "call",
        "udf",
        "begin",
        "commit",
        "rollback",
        "update",
        "delete",
        "select",
        "explain",
        "set",
        "SEARCH",
        " search",
        "",
    ];
    for op in unsupported_ops {
        let body = format!(r#"{{"op":"{op}","table":"docs","vector":[1.0,0.0],"limit":10}}"#);
        let resp = query_as_alice(addr, body.as_bytes());
        assert_eq!(resp.status, 501, "op={op:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "0A000", "op={op:?}");
        assert_eq!(http_common::error_code_of(&resp), "FEATURE_NOT_SUPPORTED");
        assert_eq!(
            http_common::error_message_of(&resp),
            UNSUPPORTED_OP_MESSAGE,
            "op={op:?}"
        );
        if !op.is_empty() {
            http_common::assert_message_does_not_echo(&resp, op);
        }
    }

    // 全拒否後も行数は不変（`delete`／`update`／`drop_table` 相当のいずれも
    // 何も変更していないことを非 vacuous に確認する）。
    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""row_count":3"#),
        "{}",
        body_utf8(&resp)
    );
}

// --- T3: RLS 段を狙うフィールドは 42601、最小差分対照は 200 かつ RLS clean（受入 3・4） ---

/// `(label, 拒否本文, 対照本文)`。対照本文は拒否本文から当該キーだけを外した
/// もの（拒否が「そのキーの存在」に起因することを非 vacuous に固定する）。
fn rls_targeting_cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // --- トップレベル hint_order（4 op） ---
        (
            "search/hint_order",
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],"hint_order":["path"]}"#,
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/hint_order",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"hint_order":["path"]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/hint_order",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"hint_order":["path"]}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- search_mode（4 op） ---
        (
            "search/search_mode",
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],"search_mode":"precision"}"#,
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/search_mode",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"search_mode":"precision"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/search_mode",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"search_mode":"precision"}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- mode（scan／aggregate。search は既知キーのため search_mode で代表） ---
        (
            "scan/mode",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"mode":"precision"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/mode",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"mode":"precision"}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- set/session/transaction（scan で代表） ---
        (
            "scan/set",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"set":"x"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/session",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"session":"x"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/transaction",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"transaction":"begin"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        // --- filter 要素の column が RLS 述語名（search/scan/aggregate） ---
        (
            "search/filter-visible",
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],"filter":[{"column":"visible","op":"eq","value":"x"}]}"#,
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/filter-visible-call",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"visible()","op":"eq","value":"x"}]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/filter-VISIBLE",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"filter":[{"column":"VISIBLE","op":"eq","value":"x"}]}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- filter 要素内の未知キー ---
        (
            "scan/filter-unknown-key",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"lang","op":"eq","value":"ja","hint_order":["path"]}]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        ),
    ]
}

#[test]
fn rls_targeting_fields_reject_with_42601_and_minimal_diff_contrast_succeeds() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    for (label, rejected_body, contrast_body) in rls_targeting_cases() {
        let rejected = query_as_alice(addr, rejected_body.as_bytes());
        assert_eq!(
            rejected.status, 400,
            "label={label} body={rejected_body} resp={rejected:?}"
        );
        assert_eq!(
            http_common::wire_code_of(&rejected),
            "42601",
            "label={label}"
        );
        let message = http_common::error_message_of(&rejected);
        assert!(
            !message.contains("tenant-a") && !message.contains("tenant-b"),
            "label={label} message must not leak tenant id: {message:?}"
        );

        let contrast = query_as_alice(addr, contrast_body.as_bytes());
        assert_eq!(
            contrast.status, 200,
            "label={label} contrast body={contrast_body} resp={contrast:?} (拒否が \
             キー以外の要因に起因している可能性がある)"
        );
        assert_rls_clean(&contrast);
    }
}

/// `insert` は行を実際に書くため専用ケースとして扱う。拒否本文と対照本文で
/// **同じ `operation_id`** を使うことで、「スキーマ拒否された要求は
/// `operation_id` 台帳に触れていない」ことを非 vacuous に固定する
/// （対照が `23505`／`22023` にならず素直に成功することで判定する）。
#[test]
fn insert_rls_targeting_fields_reject_and_shared_operation_id_contrast_succeeds() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let cases: [(&str, u64, &str); 4] = [
        ("insert/hint_order", 501, "nosql1-insert-hint-order"),
        ("insert/search_mode", 502, "nosql1-insert-search-mode"),
        ("insert/mode", 503, "nosql1-insert-mode"),
        (
            "insert/filter-not-applicable",
            504,
            "nosql1-insert-transaction",
        ),
    ];

    for (label, id, op_id) in cases {
        let rejected_body = match label {
            "insert/hint_order" => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","hint_order":["path"]}}"#
            ),
            "insert/search_mode" => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","search_mode":"precision"}}"#
            ),
            "insert/mode" => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","mode":"precision"}}"#
            ),
            _ => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","transaction":"begin"}}"#
            ),
        };
        let rejected = query_as_alice(addr, rejected_body.as_bytes());
        assert_eq!(
            rejected.status, 400,
            "label={label} body={rejected_body} resp={rejected:?}"
        );
        assert_eq!(
            http_common::wire_code_of(&rejected),
            "42601",
            "label={label}"
        );

        // 対照: 同じ operation_id・同じ id でキーだけ外す。拒否がスキーマ
        // 検証段で止まり台帳に触れていなければ 200（`23505`／`22023` になら
        // ない）。
        let contrast_body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}"}}"#
        );
        let contrast = query_as_alice(addr, contrast_body.as_bytes());
        assert_eq!(
            contrast.status, 200,
            "label={label} contrast body={contrast_body} resp={contrast:?} (拒否された \
             要求が operation_id 台帳へ書き込んでいた可能性がある)"
        );
        assert_eq!(
            body_utf8(&contrast),
            format!(r#"{{"inserted":1,"operation_id":"{op_id}"}}"#)
        );
    }
}

/// RLS clean の強化（`scan` は順序保証を持たず・`aggregate` は件数のみの
/// ため）: `filter` を罠行だけに一致する条件にすると `search`／`scan` は
/// `row_count: 0`、`aggregate` の count は 0 になる（順序に依存せず RLS を
/// 単独で切り分ける）。
#[test]
fn filter_matching_only_trap_rows_returns_empty_visible_set() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let search_resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],
            "filter":[{"column":"lang","op":"eq","value":"trap-b-99"}]}"#,
    );
    assert_eq!(search_resp.status, 200, "resp={search_resp:?}");
    assert!(
        body_utf8(&search_resp).contains(r#""row_count":0"#),
        "{}",
        body_utf8(&search_resp)
    );
    assert_rls_clean(&search_resp);

    let scan_resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"],
            "filter":[{"column":"lang","op":"eq","value":"trap-b-99"}]}"#,
    );
    assert_eq!(scan_resp.status, 200, "resp={scan_resp:?}");
    assert!(
        body_utf8(&scan_resp).contains(r#""row_count":0"#),
        "{}",
        body_utf8(&scan_resp)
    );
    assert_rls_clean(&scan_resp);

    let aggregate_resp = query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],
            "filter":[{"column":"lang","op":"eq","value":"trap-b-99"}]}"#,
    );
    assert_eq!(aggregate_resp.status, 200, "resp={aggregate_resp:?}");
    assert!(
        body_utf8(&aggregate_resp).contains("[0]"),
        "{}",
        body_utf8(&aggregate_resp)
    );
    assert_rls_clean(&aggregate_resp);
}

// --- T4: 拒否要求はセッションを消費・失効させない ---------------------------

#[test]
fn rejected_requests_do_not_consume_or_expire_the_session() {
    let (core, _guard) = new_core_two_tenant_docs();
    // 枠 1 でログインを 1 回だけ行い、以降はトークンを使い回す（拒否要求が
    // 新たな枠を確保しないこと・既存トークンを失効させないことを固定する）。
    let addr = spawn_with_session_limit(Arc::clone(&core), 1);
    let token = login(addr, "alice", "pw-alice");

    let hint_order_rejected = post(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"hint_order":["path"]}"#,
    );
    assert_eq!(
        hint_order_rejected.status, 400,
        "resp={hint_order_rejected:?}"
    );
    assert_eq!(http_common::wire_code_of(&hint_order_rejected), "42601");

    let unsupported_op_rejected = post(
        addr,
        &token,
        br#"{"op":"drop_table","table":"docs","vector":[1.0,0.0],"limit":10}"#,
    );
    assert_eq!(
        unsupported_op_rejected.status, 501,
        "resp={unsupported_op_rejected:?}"
    );
    assert_eq!(http_common::wire_code_of(&unsupported_op_rejected), "0A000");

    // 同一トークンで最後に送った対照が 200・行数不変・RLS clean であれば、
    // 上記 2 件の拒否がトークンを失効させず、枠を新たに確保もしなかった
    // ことの非 vacuous な証跡になる（枠 1 のまま再ログインせずに成功する）。
    let contrast = post(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(contrast.status, 200, "resp={contrast:?}");
    assert!(
        body_utf8(&contrast).contains(r#""row_count":3"#),
        "{}",
        body_utf8(&contrast)
    );
    assert_rls_clean(&contrast);
}

// --- T5: 非漏えい ------------------------------------------------------------

#[test]
fn success_and_rejection_bodies_never_leak_tenant_credentials_or_trap_sentinels() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let mut bodies: Vec<String> = Vec::new();

    // T1 相当の成功応答。
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"insert","table":"docs","rows":[{"id":600,"embedding":[0.4,0.5]}],"operation_id":"nosql1-t5-insert"}"#,
    )));

    // T2 相当の語彙外拒否・T3 相当の RLS 段拒否。
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"begin","table":"docs","vector":[1.0,0.0],"limit":10}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"hint_order":["path"]}"#,
    )));

    for body in bodies {
        assert!(!body.contains("tenant-a"), "leaked tenant-a: {body}");
        assert!(!body.contains("tenant-b"), "leaked tenant-b: {body}");
        assert!(!body.contains("pw-alice"), "leaked password: {body}");
        for sentinel in TRAP_SENTINELS {
            assert!(
                !body.contains(sentinel),
                "leaked sentinel {sentinel}: {body}"
            );
        }
    }
}
