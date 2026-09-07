//! 簡易クエリ応答（`RowDescription`/`DataRow`×N/`CommandComplete`/
//! `ReadyForQuery`）を上限付きバッファへ束ね、行数に比例しない回数の
//! `write_all` で送出するための組み立て器（Issue #481）。
//!
//! 呼び出し文脈: [`crate::simple_query::respond_query_result`] が
//! `RowDescription` を皮切りに各行を [`ResponseBuffer::push_frame`] へ渡し、
//! `CommandComplete`/`ReadyForQuery` まで積んでから最後に一度
//! [`ResponseBuffer::flush`] する。以前は各フレームをそれぞれ個別の
//! `write_all` で送出しており（`docs/design/knn-wire-stage-profile.md`
//! 「スコープ外・申し送り」で先送りされていた点）、行数分のシステムコールが
//! wire 応答レイテンシに比例して乗っていた。
//!
//! 上限（[`crate::limits::MAX_RESPONSE_BUFFER_BYTES`]）は「拒否」ではなく
//! 「フラッシュ閾値」である ―― バッファがこの値を超えそうになったら、まず
//! 溜まっている分を送出してから続ける（分割送出）。これにより 1 接続あたりの
//! 追加常駐メモリを上限値 + フレーム 1 個ぶんに有界化しつつ、応答全体が
//! 上限以下に収まる（大半のケース）では真に 1 回の `write_all` になる。
//!
//! フレームは常に境界単位で扱う（分割してバッファへまたがせない）。1
//! フレーム自体が上限を超える場合はバッファを経由せず直接 `write_all` する
//! （コピーを増やさない）。
//!
//! untrusted 入力の扱い（`.claude/rules/coding-rust.md`）: 添字アクセス
//! （`[]`）・`unwrap`/`expect` は使わない。長さ演算はすべて `checked_*`。

use std::io::{self, Write};

/// 簡易クエリ応答を組み立てる際の未送出バッファ。`W: Write` をジェネリックに
/// 取ることで、単体テストは実ソケットを介さず `Vec<u8>` シンク（＋書き込み
/// 回数を数えるラッパ）で検証できる。
pub(crate) struct ResponseBuffer {
    buf: Vec<u8>,
    cap: usize,
}

impl ResponseBuffer {
    /// 上限 `cap` バイトのバッファを作る。初期確保は `hint` を `cap` で
    /// 頭打ちにした値のみを使う（untrusted 入力に基づく無制限
    /// `Vec::with_capacity` を避ける規約に従う。`hint` は応答内容から
    /// 呼び出し元が見積もった概算で構わない）。
    pub(crate) fn with_capacity_hint(cap: usize, hint: usize) -> Self {
        Self {
            buf: Vec::with_capacity(hint.min(cap)),
            cap,
        }
    }

    /// 現在の未送出バイト数。
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// 未送出バイトが無いか。
    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// `frame` を書き込むための開始位置を返す。呼び出し元は
    /// `buf.frame_start()` の直後にフレームを直接エンコードし
    /// （[`ResponseBuffer::as_mut_vec`]）、失敗した場合は
    /// [`ResponseBuffer::truncate_to`] で巻き戻す。
    pub(crate) fn frame_start(&self) -> usize {
        self.buf.len()
    }

    /// in-place エンコード用の可変参照。エンコードが `frame_start()` から
    /// 追記する契約を守るのは呼び出し元の責務（本型はその契約を強制しない）。
    pub(crate) fn as_mut_vec(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// `start`（[`frame_start`] が返した値）まで巻き戻す。書きかけの
    /// フレームを完全に取り除き、部分フレームを絶対に送出しないための
    /// 巻き戻し操作（`Vec::truncate` は `start > len` でも panic しないが、
    /// 契約上 `start` は必ず過去の `frame_start()` の戻り値を使うこと）。
    pub(crate) fn truncate_to(&mut self, start: usize) {
        self.buf.truncate(start);
    }

    /// 完成済みフレーム `frame` を積む。積んだ結果が上限を超え、かつ
    /// バッファが非空なら先に [`flush`] する。それでも `frame` 単体が上限を
    /// 超える場合はバッファへコピーせず `w` へ直接書く（フレームを跨いで
    /// 分割しない契約を保つ）。
    pub(crate) fn push_frame(&mut self, w: &mut impl Write, frame: &[u8]) -> io::Result<()> {
        let projected = self.buf.len().checked_add(frame.len());
        let exceeds_cap = match projected {
            Some(total) => total > self.cap,
            None => true,
        };
        if exceeds_cap && !self.is_empty() {
            self.flush(w)?;
        }
        if frame.len() > self.cap {
            // 単体で上限を超える 1 フレームはバッファへ経由させずそのまま送出する
            // （コピーを増やさない。バッファは既に flush 済みで空である）。
            return w.write_all(frame);
        }
        self.buf.extend_from_slice(frame);
        Ok(())
    }

    /// 未送出バイトが有れば `write_all` で送出してクリアする（容量は保持）。
    /// 空なら何もしない（無駄な `write_all(&[])` を発行しない）。
    pub(crate) fn flush(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        w.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 書き込み回数を数える `Vec<u8>` シンク（実ソケットを介さず `write_all`
    /// 呼び出し回数を検証するためのテスト専用ラッパ）。
    struct CountingSink {
        data: Vec<u8>,
        writes: usize,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                data: Vec::new(),
                writes: 0,
            }
        }
    }

    impl Write for CountingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.data.extend_from_slice(buf);
            self.writes += 1;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frames_under_cap_are_sent_in_a_single_write() {
        let mut sink = CountingSink::new();
        let mut buf = ResponseBuffer::with_capacity_hint(1024, 64);
        buf.push_frame(&mut sink, b"AAAA").expect("push");
        buf.push_frame(&mut sink, b"BBBB").expect("push");
        buf.flush(&mut sink).expect("flush");

        assert_eq!(sink.writes, 1, "must coalesce into a single write_all");
        assert_eq!(sink.data, b"AAAABBBB");
    }

    #[test]
    fn exceeding_cap_flushes_at_frame_boundary_without_splitting_a_frame() {
        let mut sink = CountingSink::new();
        // cap=8: 最初の 2 フレーム(4+4=8)はちょうど収まり、3 フレーム目で
        // 超過するため境界で flush される。
        let mut buf = ResponseBuffer::with_capacity_hint(8, 8);
        buf.push_frame(&mut sink, b"AAAA").expect("push 1");
        buf.push_frame(&mut sink, b"BBBB").expect("push 2");
        assert_eq!(sink.writes, 0, "no flush yet: exactly at cap");
        buf.push_frame(&mut sink, b"CCCC")
            .expect("push 3 forces flush");
        assert_eq!(
            sink.writes, 1,
            "boundary flush before pushing the 3rd frame"
        );
        assert_eq!(sink.data, b"AAAABBBB");
        buf.flush(&mut sink).expect("final flush");
        assert_eq!(sink.writes, 2);
        assert_eq!(sink.data, b"AAAABBBBCCCC");
    }

    #[test]
    fn a_single_frame_larger_than_cap_is_sent_directly_without_buffering() {
        let mut sink = CountingSink::new();
        let mut buf = ResponseBuffer::with_capacity_hint(4, 4);
        let huge = vec![b'X'; 100];
        buf.push_frame(&mut sink, &huge).expect("push huge frame");
        assert_eq!(sink.writes, 1, "oversized frame goes straight to the sink");
        assert!(buf.is_empty(), "buffer stays empty for a direct write");
        assert_eq!(sink.data, huge);
    }

    #[test]
    fn truncate_to_rewinds_a_partially_written_frame() {
        let mut buf = ResponseBuffer::with_capacity_hint(1024, 64);
        buf.as_mut_vec().extend_from_slice(b"COMMITTED");
        let start = buf.frame_start();
        buf.as_mut_vec().extend_from_slice(b"PARTIAL_GARBAGE");
        buf.truncate_to(start);
        assert_eq!(buf.as_mut_vec().as_slice(), b"COMMITTED");
    }

    #[test]
    fn flush_on_empty_buffer_issues_no_write() {
        let mut sink = CountingSink::new();
        let mut buf = ResponseBuffer::with_capacity_hint(64, 64);
        buf.flush(&mut sink).expect("flush empty");
        assert_eq!(sink.writes, 0);
    }
}
