//! accept 直後の `TCP_NODELAY` 設定（Nagle アルゴリズム無効化）が簡易クエリ
//! 応答のラウンドトリップ遅延に効くことを回帰保護する結合テスト。
//!
//! 背景: 簡易クエリ応答は `RowDescription`／`DataRow`／`CommandComplete`／
//! `ReadyForQuery` の複数の小さな `write_all` に分かれて送出される
//! （`crate::simple_query`）。accept 直後に `TCP_NODELAY` を設定しないと、
//! Nagle アルゴリズムと受信側（クライアント）の delayed ACK が相互作用し、
//! ループバック接続でも 1 往復あたり 40ms 級の余計な遅延が生じうる
//! （`crates/wire-server/src/server.rs` の `accept_loop_inner` 内、
//! `limits::apply_read_timeout` 直後の `stream.set_nodelay(true)` 参照）。
//!
//! ループバック環境前提・CI ノイズ耐性のため、単発の絶対値ではなく
//! 20 往復の中央値を 20ms 未満というゆるい閾値で判定する
//! （`TCP_NODELAY` が効いていれば同一プロセス内ループバックの中央値は
//! 通常 1ms 未満に収まり、Nagle 由来の遅延が残っていれば 40ms 級になるため、
//! 20ms は両者を明確に弁別できる中間値）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// 空の `docs` テーブル（`embedding VECTOR(2)`）を持つ `EngineCore` を新設する。
/// 行の中身はレイテンシ計測に無関係（`SELECT COUNT(*)` は 0 行でも 1 行の
/// 集計結果を返すため RowDescription/DataRow/CommandComplete の 3 メッセージ +
/// ReadyForQuery という応答形は非空テーブルと同一）。
fn new_core_empty_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-nodelay-latency-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// 簡易クエリの応答一式（RowDescription → DataRow → CommandComplete →
/// ReadyForQuery）を読み切るヘルパー。列数・値は検証せず、応答フレーミングの
/// 消費のみを担う（本テストの主眼はラウンドトリップ時間であって応答内容の
/// 正しさは他テストが担保済み）。
fn drain_simple_query_response(stream: &mut std::net::TcpStream) {
    let _ = read_row_description(stream);
    let _ = read_data_row(stream);
    let _ = read_command_complete(stream);
    read_ready_for_query(stream);
}

/// `accept_loop_with_engine` 経由の実サーバーへ 20 回連続で
/// `SELECT COUNT(*) FROM docs` を送り、往復時間の中央値が 20ms 未満に収まる
/// ことを確認する。`TCP_NODELAY` 未設定への回帰（Nagle × delayed ACK による
/// 40ms 級の遅延復活）を検知する。
#[test]
fn nodelay_keeps_simple_query_round_trip_latency_low() {
    let users_path = write_user_store_file(&[("bench", "public", "bench")]);
    let (core, _guard) = new_core_empty_docs();
    let addr = spawn_server_with_engine(&users_path, core);

    let mut stream = authenticate_to_ready_for_query(addr, "bench", "bench");

    const ROUNDS: usize = 20;
    let mut latencies = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        send_simple_query(&mut stream, "SELECT COUNT(*) FROM docs");
        drain_simple_query_response(&mut stream);
        latencies.push(start.elapsed());
    }

    latencies.sort();
    let median = latencies[ROUNDS / 2];
    assert!(
        median < Duration::from_millis(20),
        "median round-trip latency {median:?} over {ROUNDS} rounds exceeds 20ms \
         (TCP_NODELAY regression suspected); all latencies: {latencies:?}"
    );
}
