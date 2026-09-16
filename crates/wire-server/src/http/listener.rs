//! NoSQL 表層（`--surface nosql`）の accept ループ **stub**（Issue #735・
//! TASK-171／HTTP-1・HTTP-9）。
//!
//! `main.rs::run_server` が `--surface nosql` を選んだ場合に、SQL wire の
//! [`crate::server::accept_loop_with_engine`] の代わりに呼ばれる。bind 自体は
//! 両表層で [`crate::bind_guard::GuardedBindAddrs`] を共有するため（HTTP-9
//! の bind ガード適用は `main.rs` 側のディスパッチ構造で担保する）、本モジュール
//! の責務は「1 本だけ listen される」ことの受け皿にとどめる。
//!
//! 本 Issue 時点では要求を一切読まず、接続を受理した直後に閉じるだけの stub
//! である（意図的な最小実装。以下はいずれも後続 Issue の担当で、本関数の
//! 設計そのものと誤解しないこと）:
//! - 読み取りタイムアウト・同時接続数リミッター（[`crate::limits`] 相当。Issue #743）
//! - 要求の読み取り・[`crate::http::request`] によるパース・応答生成、
//!   panic の非伝播（Issue #747）
//! - 応答エンコーダ（[`crate::http::error_body`]／[`crate::http::status`] を
//!   使った実応答。Issue #746）
//!
//! untrusted なバイト列を一切読み書きしないため、受信データ経路の
//! `unwrap`／`expect`／添字アクセス禁止（`.claude/rules/coding-rust.md`）は
//! 本モジュールでは該当しない。

use std::net::{Shutdown, TcpListener};

/// 接続を受理した直後に閉じるだけの accept ループ（stub）。
///
/// `listener.incoming()` は `Err` を返しても走査を止めないため、accept
/// エラーは 1 行ログに残して次の接続へ進む（`crate::server` の accept ループ
/// と同じ「1 接続の失敗でプロセス全体を落とさない」方針）。ログにはピア
/// アドレス等の識別情報を含めない（本関数が受理する接続はまだ認証されて
/// おらず、untrusted な入力を伴うログはテナント情報漏えいの経路になり
/// うるため）。
///
/// 戻り値なし（`crate::server::accept_loop_with_engine` と同様、呼び出し元は
/// プロセス終了までこの関数から戻らない前提で呼ぶ）。
pub fn accept_loop_stub(listener: TcpListener) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                // 要求を読まず・応答も書かない（stub の契約）。`shutdown` の
                // 結果は無視してよい（すでに切断済み等のエラーは無害）。
                let _ = stream.shutdown(Shutdown::Both);
            }
            Err(e) => {
                eprintln!("wire-server: accept error: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    /// stub リスナーが接続を受理した直後に閉じ、要求を読まないこと・
    /// 応答を書かないこと・ループが次の接続を受理し続けること（1 回の
    /// 接続で終了しない）を確認する。
    #[test]
    fn accept_loop_stub_closes_connections_without_reading_or_writing() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback");
        let addr = listener.local_addr().expect("local addr");

        std::thread::spawn(move || {
            accept_loop_stub(listener);
        });

        for _ in 0..2 {
            let mut stream =
                TcpStream::connect(addr).expect("connect to stub listener should succeed");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");

            let mut buf = [0u8; 8];
            let read_result = stream.read(&mut buf);
            match read_result {
                // stub は accept 直後にクローズするため EOF（0 バイト）を
                // 期待するのが基本形だが、シャットダウンの伝播タイミングに
                // よっては OS が RST を返すこともある（いずれも「応答を
                // 待ち受けている」状態ではないことの証跡として許容する）。
                Ok(n) => assert_eq!(n, 0, "expected EOF (0 bytes), got {n} bytes"),
                Err(e) => {
                    let kind = e.kind();
                    assert!(
                        kind == std::io::ErrorKind::ConnectionReset
                            || kind == std::io::ErrorKind::BrokenPipe,
                        "unexpected read error: {e:?}"
                    );
                }
            }
        }
    }
}
