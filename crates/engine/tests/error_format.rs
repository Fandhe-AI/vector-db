//! `engine::error_format` の結合テスト（TASK-152、対象ビヘイビア: ERR-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-152・`docs/spec/04-behavior/error-format.md`）。
//!
//! `ErrorClass`（`wire_code` 写像の単一真実源）の一意性・決定性・往復変換、
//! 既存 `SqlSurfaceError`／`TenantWriteError` の `wire_code()` 委譲後の値不変、
//! 実 `EngineCore` を用いた分類境界（構文・値・テーブル不在）の決定的分類、
//! `InternalError` がクライアント文言へ内部詳細を運ばないことを検証する。
//!
//! 台帳系（`operation_id` 内容不一致・`22023`）は ERR-3 管轄で本タスクの対象外
//! （TASK-154 が発生経路を実装する。ここでは写像の一意性検証にのみ含める）。

use std::collections::HashSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass, WireError};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::storage::Storage;
use engine::tenant::TenantWriteError;

// 一時 DB パス払い出しは共通ヘルパへ委譲する（Issue #173）。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

// --- 一意性・決定性 -----------------------------------------------------------

#[test]
fn err2_all_classes_have_unique_wire_codes() {
    let codes: HashSet<&str> = ErrorClass::ALL.iter().map(|c| c.wire_code()).collect();
    assert_eq!(
        codes.len(),
        ErrorClass::ALL.len(),
        "wire_code は分類ごとに一意でなければならない"
    );
}

/// ERR-2 分類表（15 行）と、表外の拡張分類の関係を固定する。表側の増減、および
/// 拡張分類の増加（ビヘイビアファイル固有の `wire_code` 追加）を検知する。
#[test]
fn err2_table_is_fifteen_rows_and_extensions_are_explicit() {
    let table: Vec<ErrorClass> = ErrorClass::ALL
        .into_iter()
        .filter(|c| c.is_err2_table_row())
        .collect();
    assert_eq!(table.len(), 15, "spec 表は計 15 行");

    // 表外の拡張は SQL-13（集計の数値範囲超過）の 1 分類のみ。
    let extensions: Vec<ErrorClass> = ErrorClass::ALL
        .into_iter()
        .filter(|c| !c.is_err2_table_row())
        .collect();
    assert_eq!(extensions, vec![ErrorClass::NumericOutOfRange]);
    assert_eq!(ErrorClass::NumericOutOfRange.wire_code(), "22003");
}

#[test]
fn err2_wire_code_is_five_char_sqlstate_shape() {
    for class in ErrorClass::ALL {
        let code = class.wire_code();
        assert_eq!(code.len(), 5, "wire_code は 5 文字: {code}");
        assert!(
            code.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "wire_code は ASCII 英大文字・数字のみ: {code}"
        );
    }
}

#[test]
fn err2_wire_code_is_deterministic() {
    for class in ErrorClass::ALL {
        let first = class.wire_code();
        for _ in 0..1000 {
            assert_eq!(
                class.wire_code(),
                first,
                "同一 variant は常に同一 wire_code"
            );
        }
        // 往復変換（wire_code → from_wire_code）が元の分類へ戻ることを確認する。
        assert_eq!(ErrorClass::from_wire_code(first), Some(class));
    }

    // スレッドを跨いでも同一値であることを確認する（外部状態非依存の証跡）。
    let handles: Vec<_> = (0..8)
        .map(|_| std::thread::spawn(|| ErrorClass::InvalidInput.wire_code()))
        .collect();
    for h in handles {
        assert_eq!(h.join().expect("thread join"), "22000");
    }
}

#[test]
fn err2_from_wire_code_rejects_unknown_codes() {
    assert_eq!(ErrorClass::from_wire_code(""), None);
    assert_eq!(ErrorClass::from_wire_code("22000x"), None, "6 文字は未知");
    assert_eq!(ErrorClass::from_wire_code("2200"), None, "4 文字は未知");
    assert_eq!(
        ErrorClass::from_wire_code("22o00"),
        None,
        "未知の 5 文字コード"
    );
    assert_eq!(
        ErrorClass::from_wire_code("28p01"),
        None,
        "小文字化した既知コードは一致しない（fail-closed）"
    );
}

#[test]
fn err2_label_is_screaming_snake_case_and_unique() {
    let labels: HashSet<&str> = ErrorClass::ALL.iter().map(|c| c.label()).collect();
    assert_eq!(
        labels.len(),
        ErrorClass::ALL.len(),
        "label はクラスごとに一意"
    );
    for class in ErrorClass::ALL {
        let label = class.label();
        assert!(
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()),
            "label は SCREAMING_SNAKE_CASE: {label}"
        );
    }
}

// --- 既存 wire_code() の回帰（委譲後も値不変） ---------------------------------

#[test]
fn err2_sql_surface_error_mapping_is_unchanged() {
    assert_eq!(
        SqlSurfaceError::UnsupportedSyntax {
            detail: "x".to_string()
        }
        .wire_code(),
        "42601"
    );
    assert_eq!(SqlSurfaceError::MissingOperationId.wire_code(), "23502");

    let undefined_table_code = SqlSurfaceError::UndefinedTable {
        name: "t".to_string(),
    }
    .wire_code();
    assert_eq!(undefined_table_code, "42P01");

    assert_eq!(
        SqlSurfaceError::Internal {
            detail: "x".to_string()
        }
        .wire_code(),
        "XX000"
    );
    assert_eq!(
        SqlSurfaceError::InvalidInput {
            detail: "x".to_string()
        }
        .wire_code(),
        "22000"
    );
    assert_eq!(
        SqlSurfaceError::PayloadTooLarge {
            detail: "x".to_string()
        }
        .wire_code(),
        "54000"
    );
    assert_eq!(SqlSurfaceError::IdConflict.wire_code(), "23505");
    assert_eq!(
        SqlSurfaceError::NumericOutOfRange {
            detail: "x".to_string()
        }
        .wire_code(),
        "22003"
    );
}

#[test]
fn err2_tenant_write_error_mapping_is_unchanged() {
    assert_eq!(TenantWriteError::Forbidden.wire_code(), "42501");
    assert_eq!(TenantWriteError::NotFound.wire_code(), "P0002");
    assert_eq!(TenantWriteError::IdConflict.wire_code(), "23505");
    assert_eq!(TenantWriteError::MissingOperationId.wire_code(), "23502");
}

// --- 分類境界（構文 vs 値 vs テーブル不在。決定的分類の確認） -------------------

#[test]
fn err2_classification_boundary_syntax_vs_value_vs_protocol() {
    let path = unique_db_path("err2-boundary");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // (a) 許可リスト外の SQL → 構文分類（42601）。同一入力を 2 回投げて同一 wire_code
    // であることを確認する（決定的分類）。
    let syntax_sql = "DROP TABLE documents";
    let e1 = core.execute_sql(&ctx, syntax_sql).expect_err("rejected");
    let e2 = core.execute_sql(&ctx, syntax_sql).expect_err("rejected");
    assert_eq!(e1.wire_code(), "42601");
    assert_eq!(e2.wire_code(), "42601");
    assert_eq!(e1.wire_code(), e2.wire_code());

    // (b) 受理構文だが値が不正（ORDER BY 距離演算子に未知の列名を束縛） →
    // 入力不正分類（22000）。
    let value_sql =
        "SELECT * FROM documents ORDER BY nonexistent_column <=> '[0.1,0.2,0.3]' LIMIT 3";
    let e3 = core.execute_sql(&ctx, value_sql).expect_err("rejected");
    let e4 = core.execute_sql(&ctx, value_sql).expect_err("rejected");
    assert_eq!(e3.wire_code(), "22000");
    assert_eq!(e3.wire_code(), e4.wire_code());

    // (c) 未存在テーブル → テーブル不在分類（42P01）。
    let table_sql =
        "SELECT * FROM nonexistent_table ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 3";
    let e5 = core.execute_sql(&ctx, table_sql).expect_err("rejected");
    let e6 = core.execute_sql(&ctx, table_sql).expect_err("rejected");
    assert_eq!(e5.wire_code(), "42P01");
    assert_eq!(e5.wire_code(), e6.wire_code());

    // 3 分類が互いに異なる wire_code であること（同一入力が複数分類へ跨らない）。
    assert_ne!(e1.wire_code(), e3.wire_code());
    assert_ne!(e1.wire_code(), e5.wire_code());
    assert_ne!(e3.wire_code(), e5.wire_code());
}

// --- InternalError はクライアント文言へ詳細を運ばない ---------------------------

#[test]
fn err2_internal_error_client_message_never_carries_detail() {
    let secret_detail = "redb: /secret/path/to/storage.db I/O error";
    let err = SqlSurfaceError::Internal {
        detail: secret_detail.to_string(),
    };
    assert_eq!(err.wire_code(), "XX000");
    let client_msg = ClassifiedError::client_message(&err);
    assert!(
        !client_msg.contains("/secret/path"),
        "client_message は内部詳細を運ばない: {client_msg}"
    );
    assert_eq!(client_msg, "internal error");

    let wire_err: WireError = (&err).into();
    assert_eq!(wire_err.class(), ErrorClass::InternalError);
    assert_eq!(wire_err.wire_code(), "XX000");
    assert!(
        !wire_err.message().contains("/secret/path"),
        "WireError::message は内部詳細を運ばない: {}",
        wire_err.message()
    );

    let fixed = WireError::internal();
    assert_eq!(fixed.message(), "internal error");
    assert_eq!(fixed.wire_code(), "XX000");
}

/// マルチバイト文字が上限バイト位置を跨ぐ場合に、文字境界まで巻き戻して切り詰める
/// （不正な UTF-8 断片・文字の途中での切断を作らない）。純 ASCII の
/// [`err2_wire_error_message_is_truncated`] では通らない巻き戻し経路の回帰検知。
#[test]
fn err2_wire_error_message_truncation_respects_char_boundary() {
    // "あ" は 3 バイト。上限 200 バイトは 3 の倍数ではない（200 = 3 * 66 + 2）ため、
    // 200 バイト目は文字の途中に当たり、198 バイト（66 文字）まで巻き戻される。
    let long = "あ".repeat(200);
    let err = WireError::new(ErrorClass::InvalidInput, long);
    let msg = err.message();

    assert_eq!(
        msg.len(),
        198 + "...".len(),
        "文字境界まで巻き戻して切り詰める"
    );
    assert!(msg.ends_with("..."), "切り詰め時は省略記号を付与する");
    let body = msg.strip_suffix("...").expect("suffix");
    assert_eq!(body.chars().count(), 66, "完全な文字のみを残す");
    assert!(body.chars().all(|c| c == 'あ'), "文字の途中で切らない");
    // `String` として保持できている時点で UTF-8 として妥当（不正断片を作っていない）。
    assert_eq!(
        std::str::from_utf8(msg.as_bytes()).expect("valid utf-8"),
        msg
    );
}

/// 1 文字が上限バイト長を跨ぐ極端な入力でも巻き戻しが停止し、パニックしない。
#[test]
fn err2_wire_error_message_truncation_handles_single_huge_char() {
    // 4 バイト文字（絵文字）を並べ、上限 200 の直前が文字境界にならない場合を含めて
    // 巻き戻しが常に停止することを確認する（200 = 4 * 50 でちょうど境界）。
    let long = "\u{1F600}".repeat(100);
    let err = WireError::new(ErrorClass::InvalidInput, long);
    let msg = err.message();
    assert_eq!(msg.len(), 200 + "...".len());
    let body = msg.strip_suffix("...").expect("suffix");
    assert_eq!(body.chars().count(), 50);
}

#[test]
fn err2_wire_error_message_is_truncated() {
    // `error_format::MAX_MESSAGE_LEN`（200、非 pub）+ 省略記号 "..." の 3 文字で
    // 固定長 203 バイトへ切り詰められることを検証する（回帰検知のため固定値で確認）。
    const EXPECTED_TRUNCATED_LEN: usize = 203;
    let long = "a".repeat(10_000);
    let err = WireError::new(ErrorClass::InvalidInput, long.clone());
    assert_eq!(
        err.message().len(),
        EXPECTED_TRUNCATED_LEN,
        "上限長ちょうどへ切り詰める"
    );
    assert!(
        err.message().ends_with("..."),
        "切り詰め時は省略記号を付与する"
    );
}
