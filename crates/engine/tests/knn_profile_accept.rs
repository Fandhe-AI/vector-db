//! `benches/harness/knn_profile.rs`（Issue #362。KNN 経路の段別内訳プロファイル。
//! 走査・ヘッダデコード・f32 デコード・arena 構築・距離計算の切り分け）の回帰テスト。
//!
//! `knn_profile_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hybrid_latency_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。
//!
//! 本テストの中心は [`decode_header_reimpl`]・[`decode_row_reimpl`]（ベンチ内の
//! 行フォーマット v2 再実装）が `engine::storage::Storage`（pub API・正本）と
//! 一致することの検証（ドリフト検出）。`Storage::put`（`RowInput` 経由）で
//! 書き込んだ行を、生 `redb::Database` で再オープンして本モジュールの再実装で
//! デコードし、`Storage::scan()` の結果と突き合わせる。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::knn_profile::{
    assert_scan_row_counts_match, decode_header_reimpl, decode_row_reimpl, explain_resident_value,
    ns_per_row, refuse_under_github_actions, render_index_memory_line, render_kernel_isa_line,
    requires_hnsw_stats_check, resident_label_for_token, scaled_rows, stage_diff_ns_per_row,
    KnnProfileError,
};

use engine::storage::{RowInput, Storage, Visibility};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use std::time::Duration;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ROWS_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("rows");

// --- refuse_under_github_actions --------------------------------------------

#[test]
fn refuse_under_github_actions_rejects_when_set() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, KnnProfileError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_unset() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- decode_header_reimpl / decode_row_reimpl: ドリフト検出 -----------------

fn seed_storage(path: &std::path::Path) -> Vec<(String, Visibility, Vec<f32>)> {
    let storage = Storage::open(path).expect("open storage");
    let fixtures: Vec<(String, Visibility, Vec<f32>)> = vec![
        (
            "tenant-a".to_string(),
            Visibility::Public,
            vec![1.0, 2.0, 3.0, 4.0],
        ),
        (
            "tenant-a".to_string(),
            Visibility::Private,
            vec![-1.5, 0.0, 2.25],
        ),
        ("tenant-b".to_string(), Visibility::Public, vec![0.0; 8]),
    ];
    for (i, (tenant, visibility, embedding)) in fixtures.iter().enumerate() {
        storage
            .put(
                i as u64,
                &RowInput {
                    tenant_id: tenant,
                    visibility: *visibility,
                    embedding,
                    metadata: b"",
                },
            )
            .expect("seed row");
    }
    fixtures
}

#[test]
fn decode_row_reimpl_matches_storage_scan_for_every_row() {
    let path = unique_db_path("issue362-knn-profile-accept-decode");
    let _guard = CleanupGuard(path.clone());
    let fixtures = seed_storage(&path);

    // 正本: `Storage::scan()`（pub API・完全デコード）。
    let storage = Storage::open(&path).expect("reopen storage for scan");
    let expected = storage.scan().expect("scan must succeed");
    assert_eq!(expected.len(), fixtures.len());
    drop(storage);

    // 再実装: 生 `redb::Database` で再オープンし、本モジュールのデコードを通す。
    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");

    let mut scratch: Vec<f32> = Vec::new();
    let mut actual_count = 0usize;
    for entry in table.iter().expect("iter rows table") {
        let (_k, v) = entry.expect("iterate row entry");
        let decoded = decode_row_reimpl(v.value(), &mut scratch).expect("reimpl decode");
        // `decoded` は embedding を所有化しない（`scratch` へ書き込まれる。
        // `ReimplDecodedRow` のドキュメンテーションコメント参照）ため、
        // 突き合わせは `scratch` を直接使う。
        let expected_row = expected
            .iter()
            .find(|row| {
                row.tenant_id == decoded.tenant_id
                    && (row.visibility == Visibility::Public) == decoded.is_public
                    && row.embedding == scratch
            })
            .unwrap_or_else(|| {
                panic!(
                    "reimpl decoded row not found in Storage::scan() result \
                     (layout drift suspected): tenant_id={} is_public={} embedding={:?}",
                    decoded.tenant_id, decoded.is_public, scratch
                )
            });
        assert_eq!(expected_row.metadata.len(), decoded.metadata_len);
        actual_count += 1;
    }
    assert_eq!(actual_count, fixtures.len());
}

#[test]
fn decode_header_reimpl_matches_tenant_and_visibility() {
    let path = unique_db_path("issue362-knn-profile-accept-header");
    let _guard = CleanupGuard(path.clone());
    let _fixtures = seed_storage(&path);

    let storage = Storage::open(&path).expect("reopen storage for scan");
    let expected = storage.scan().expect("scan must succeed");
    drop(storage);

    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");

    let mut matched = 0usize;
    for entry in table.iter().expect("iter rows table") {
        let (_k, v) = entry.expect("iterate row entry");
        let (tenant_id, is_public, _offset) =
            decode_header_reimpl(v.value()).expect("header decode");
        assert!(expected
            .iter()
            .any(|row| row.tenant_id == tenant_id
                && (row.visibility == Visibility::Public) == is_public));
        matched += 1;
    }
    assert_eq!(matched, expected.len());
}

#[test]
fn decode_row_reimpl_rejects_truncated_buffer() {
    // ヘッダより短いバイト列は `Err` になる（`storage.rs::decode_row_header` と
    // 同じ fail-closed 契約をベンチ内再実装でも維持していることの確認）。
    let mut scratch: Vec<f32> = Vec::new();
    let truncated = [2u8, 0u8]; // version=2, tenant_len の途中で切れている
    let err = decode_row_reimpl(&truncated, &mut scratch).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn decode_row_reimpl_rejects_unsupported_version() {
    let mut scratch: Vec<f32> = Vec::new();
    let buf = [9u8, 0u8, 0u8]; // version=9 は未対応
    let err = decode_row_reimpl(&buf, &mut scratch).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

// --- 検証契約の突き合わせ（codex-review 指摘 #2・PR #378）-------------------
//
// 以下は `storage.rs::decode_row_header`／`decode_row_embedding_and_metadata_into`
// が拒否する不正入力（未知 visibility・空/過長 tenant_id・metadata の切断/末尾
// garbage）を、ベンチ内再実装（`decode_header_reimpl`/`decode_row_reimpl`）も
// 同じく拒否することを確認する。バイト列は v2 レイアウト（モジュール冒頭
// コメント参照）を手組みし、`Storage`（pub API）を経由しない——正本側は
// これらの入力を書き込み時点で `RowInput` 検証済みのため拒否できず、再現
// できない（`decode_row_reimpl_rejects_truncated_buffer` と同じ手法）。

/// v2 行フォーマットのバイト列を組み立てる（テスト専用ヘルパー）。
/// `visibility_byte` は検証を素通しするため生の値を渡せる。
fn encode_row_v2_bytes(
    tenant: &[u8],
    visibility_byte: u8,
    embedding: &[f32],
    metadata: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(2u8); // version
    buf.extend_from_slice(&(tenant.len() as u16).to_le_bytes());
    buf.extend_from_slice(tenant);
    buf.push(visibility_byte);
    buf.extend_from_slice(&(embedding.len() as u32).to_le_bytes());
    for v in embedding {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    buf.extend_from_slice(metadata);
    buf
}

#[test]
fn decode_header_reimpl_rejects_unknown_visibility_byte() {
    // `storage.rs::Visibility::from_byte` は未知バイトを `Public` へ黙殺
    // フォールバックせず拒否する。ベンチ内再実装が誤って `false`（非公開）
    // 扱いへ黙殺していないことを確認する。
    let buf = encode_row_v2_bytes(b"tenant-a", 0xFF, &[1.0], b"");
    let err = decode_header_reimpl(&buf).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn decode_row_reimpl_rejects_unknown_visibility_byte() {
    let buf = encode_row_v2_bytes(b"tenant-a", 0xFF, &[1.0], b"");
    let mut scratch: Vec<f32> = Vec::new();
    let err = decode_row_reimpl(&buf, &mut scratch).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn decode_header_reimpl_rejects_empty_tenant_id() {
    let buf = encode_row_v2_bytes(b"", 1, &[1.0], b"");
    let err = decode_header_reimpl(&buf).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn decode_header_reimpl_rejects_oversized_tenant_id() {
    // `storage.rs::MAX_TENANT_ID_LEN`（256）超過は拒否される
    // （[`harness::knn_profile::MAX_REIMPL_TENANT_ID_LEN`] が同値のコピー）。
    let oversized_tenant = vec![b't'; 257];
    let buf = encode_row_v2_bytes(&oversized_tenant, 1, &[1.0], b"");
    let err = decode_header_reimpl(&buf).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn decode_row_reimpl_rejects_metadata_trailing_garbage() {
    // 宣言された metadata 長よりバッファが長い（末尾 garbage）場合を拒否する
    // （`storage.rs::decode_row_embedding_and_metadata_into` の
    // `metadata_end != buf.len()` 検証と同じ契約）。
    let mut buf = encode_row_v2_bytes(b"tenant-a", 1, &[1.0], b"meta");
    buf.push(0); // 宣言長を超える余剰バイト
    let mut scratch: Vec<f32> = Vec::new();
    let err = decode_row_reimpl(&buf, &mut scratch).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn decode_row_reimpl_rejects_metadata_truncated_buffer() {
    // metadata_len フィールドが実バッファより長い長さを宣言している場合を
    // 拒否する（宣言された長さ分の存在確認）。
    let full = encode_row_v2_bytes(b"tenant-a", 1, &[1.0], b"meta");
    // metadata バイト列（末尾 4 バイト "meta"）を落として、metadata_len
    // フィールド（宣言値 4）だけを残す。
    let truncated = &full[..full.len() - 4];
    let mut scratch: Vec<f32> = Vec::new();
    let err = decode_row_reimpl(truncated, &mut scratch).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

// --- ns_per_row / stage_diff_ns_per_row --------------------------------------

#[test]
fn ns_per_row_computes_expected_rate() {
    let total = Duration::from_millis(10);
    let rate = ns_per_row(total, 1_000).expect("non-zero rows");
    assert!((rate - 10_000.0).abs() < 1.0, "rate={rate}");
}

#[test]
fn ns_per_row_rejects_zero_rows() {
    let err = ns_per_row(Duration::from_millis(1), 0).unwrap_err();
    assert_eq!(err, KnnProfileError::ZeroRows);
}

#[test]
fn stage_diff_ns_per_row_computes_positive_diff() {
    let earlier = Duration::from_millis(5);
    let later = Duration::from_millis(8);
    let diff = stage_diff_ns_per_row(earlier, later, 1_000, "S1", "S2").expect("later >= earlier");
    assert!((diff - 3_000.0).abs() < 1.0, "diff={diff}");
}

#[test]
fn stage_diff_ns_per_row_rejects_non_monotonic_stages() {
    let earlier = Duration::from_millis(8);
    let later = Duration::from_millis(5);
    let err = stage_diff_ns_per_row(earlier, later, 1_000, "S1", "S2").unwrap_err();
    assert_eq!(
        err,
        KnnProfileError::NonMonotonicStages {
            earlier: "S1",
            later: "S2"
        }
    );
}

// --- assert_scan_row_counts_match --------------------------------------------

#[test]
fn assert_scan_row_counts_match_accepts_equal_counts() {
    assert!(assert_scan_row_counts_match(&[("S1", 100), ("S2", 100), ("S3", 100)]).is_ok());
}

#[test]
fn assert_scan_row_counts_match_rejects_mismatched_counts() {
    let err = assert_scan_row_counts_match(&[("S1", 100), ("S2", 99)]).unwrap_err();
    assert!(matches!(err, KnnProfileError::Codec(_)));
}

#[test]
fn assert_scan_row_counts_match_accepts_empty_input() {
    assert!(assert_scan_row_counts_match(&[]).is_ok());
}

// --- scaled_rows (Issue #516) ------------------------------------------------

#[test]
fn scaled_rows_multiplies_scale_by_unit_rows() {
    assert_eq!(scaled_rows(1, 25_000).unwrap(), 25_000);
    assert_eq!(scaled_rows(4, 25_000).unwrap(), 100_000);
    assert_eq!(scaled_rows(20, 25_000).unwrap(), 500_000);
}

#[test]
fn scaled_rows_rejects_zero_unit_rows_and_overflow() {
    assert!(scaled_rows(1, 0).is_err());
    assert!(scaled_rows(u64::MAX, 2).is_err());
}

// --- explain_resident_value (Issue #516) ------------------------------------

#[test]
fn explain_resident_value_extracts_from_hnsw_params_line() {
    let lines = vec![
        "mode_source: default".to_string(),
        "engine: hnsw".to_string(),
        "hnsw_params: m=32,ef_construction=200,ef_search=128,resident=f16".to_string(),
        "ann_plan: hnsw_full_visible".to_string(),
    ];
    assert_eq!(explain_resident_value(&lines), Some("f16".to_string()));
}

#[test]
fn explain_resident_value_returns_none_when_missing_or_no_resident_field() {
    assert_eq!(explain_resident_value(&[]), None);
    let lines = vec!["engine: parallel_brute_force".to_string()];
    assert_eq!(explain_resident_value(&lines), None);
    let lines = vec!["hnsw_params: m=32,ef_construction=200,ef_search=128".to_string()];
    assert_eq!(explain_resident_value(&lines), None);
}

#[test]
fn explain_resident_value_handles_trailing_field_and_whitespace() {
    let lines = vec!["  hnsw_params: m=32, resident=f32 ".to_string()];
    assert_eq!(explain_resident_value(&lines), Some("f32".to_string()));
}

// --- resident_label_for_token (Issue #516) ----------------------------------

#[test]
fn resident_label_for_token_maps_known_tokens() {
    assert_eq!(resident_label_for_token("hnsw"), Some("f32"));
    assert_eq!(resident_label_for_token("hnsw_f16"), Some("f16"));
    assert_eq!(resident_label_for_token("hnsw_i8"), Some("i8"));
    assert_eq!(resident_label_for_token("brute_force"), None);
    assert_eq!(resident_label_for_token("bogus"), None);
}

// --- render_index_memory_line (Issue #516) ----------------------------------

#[test]
fn render_index_memory_line_formats_available_values() {
    let line = render_index_memory_line(
        500_000,
        768,
        "f16",
        "f16",
        123_456,
        Some(1_000),
        Some(2_500),
        Some(3_000),
    );
    assert_eq!(
        line,
        "knn_profile_bench: index_memory rows=500000 dim=768 requested=f16 effective=f16 \
         approx_heap_bytes=123456 vm_rss_kb_before=1000 vm_rss_kb_after=2500 \
         vm_rss_delta_kb=1500 vm_hwm_kb=3000"
    );
}

#[test]
fn render_index_memory_line_reports_unavailable_when_proc_stats_missing() {
    let line = render_index_memory_line(25_000, 128, "f32", "f32", 42, None, None, None);
    assert!(line.contains("vm_rss_kb_before=unavailable"));
    assert!(line.contains("vm_rss_kb_after=unavailable"));
    assert!(line.contains("vm_rss_delta_kb=unavailable"));
    assert!(line.contains("vm_hwm_kb=unavailable"));
}

// --- render_kernel_isa_line (Issue #526) -------------------------------------

#[test]
fn render_kernel_isa_line_formats_all_three_fields_in_order() {
    let line = render_kernel_isa_line("Avx2Fma", "F16c", "Avx2Widen");
    assert_eq!(
        line,
        "knn_profile_bench: kernel_isa dot=Avx2Fma f16=F16c i8=Avx2Widen"
    );
}

#[test]
fn render_kernel_isa_line_reflects_apple_expected_values() {
    // Apple 実機（aarch64）での期待値（`isa.rs::detect_f16`／`detect_i8` の
    // 優先順）。x86_64 環境ではこの組み合わせは実際には出力されない
    // （§7.7 の非 vacuous チェックリストが参照する期待値の固定）。
    let line = render_kernel_isa_line("Neon", "NeonFp16", "NeonDotprod");
    assert_eq!(
        line,
        "knn_profile_bench: kernel_isa dot=Neon f16=NeonFp16 i8=NeonDotprod"
    );
}

#[test]
fn render_kernel_isa_line_rejects_empty_field_by_not_collapsing_prefix() {
    // 空文字列を渡しても "kernel_isa" 接頭辞・フィールド境界（スペース）は
    // 維持され、後続フィールドと結合しない（`--summarize` の grep パターンが
    // 前方一致で誤集計しないことの回帰）。
    let line = render_kernel_isa_line("", "F16c", "Avx2Widen");
    assert_eq!(
        line,
        "knn_profile_bench: kernel_isa dot= f16=F16c i8=Avx2Widen"
    );
    assert!(line.starts_with("knn_profile_bench: kernel_isa "));
}

// --- requires_hnsw_stats_check (Issue #516・codex P1 指摘対応) --------------

#[test]
fn requires_hnsw_stats_check_covers_hnsw_and_hnsw_f16() {
    // hnsw・hnsw_f16 はいずれも `sql::hnsw_cache::HnswIndexCacheStats`
    // （精度非依存の単一型）を返す設計のため、非 vacuous 検証・Subset 系
    // カウンタ検証・builds_delta 契約検査のいずれも両エンジンで有効である
    // べき（codex P1 指摘: hnsw_f16 だけ検証が省略され observed=n/a
    // (brute_force engine) という実体と異なるラベルが出力されていた）。
    assert!(requires_hnsw_stats_check("hnsw"));
    assert!(requires_hnsw_stats_check("hnsw_f16"));
}

#[test]
fn requires_hnsw_stats_check_covers_hnsw_i8() {
    // Issue #523: I8（SQ8）常駐でも `hnsw`／`hnsw_f16` と同型に非 vacuous 検証
    // 対象であるべき（`hnsw_f16` と同じ codex P1 指摘の再発防止）。
    assert!(requires_hnsw_stats_check("hnsw_i8"));
}

#[test]
fn requires_hnsw_stats_check_excludes_brute_force_and_unknown_tokens() {
    assert!(!requires_hnsw_stats_check("brute_force"));
    assert!(!requires_hnsw_stats_check(""));
    assert!(!requires_hnsw_stats_check("HNSW"));
    assert!(!requires_hnsw_stats_check("bogus"));
}
