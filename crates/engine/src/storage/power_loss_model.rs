//! 電源断シミュレーション用 `redb::StorageBackend` の共通モデル（TASK-145、ポインタ:
//! `docs/spec/05-tasks.md`。関連ビヘイビア: PERSIST-1・PERSIST-3、ポインタ:
//! `docs/spec/04-behavior/persistence.md`）。
//!
//! `crates/engine/src/storage.rs` の `#[cfg(test)]` ユニットテスト（本番の `Storage::put`/
//! `Storage::get` 経由でシナリオ 4 を検証する）と、`crates/engine/tests/power_loss.rs`
//! 統合テスト（raw `redb::Database` を直接操作しシナリオ 1〜3 を検証する）の双方が同じ
//! commit/sync 契約モデルを検証するため、基本実装を本ファイルへ分離した。
//!
//! 統合テスト側は `#[path = "../src/storage/power_loss_model.rs"]` で本ファイルをそのまま
//! 別クレート（テストバイナリ）としてコンパイルする（`pub(crate)` はコンパイル先クレートに
//! 閉じるため、公開 API へは一切影響しない。`Storage` の private フィールドへ crate 内から
//! 直接アクセスする必要があるユニットテスト側の制約上、これを public モジュールとして
//! 切り出すことはしない）。部分 write-back の探索（`apply_subset` 相当）はシナリオ 2・3
//! 固有のため、統合テスト側で本モジュールの型に対する拡張 `impl` として追加する。

use std::io;
use std::sync::{Arc, Mutex};

use redb::StorageBackend;

/// [`PowerLossBackend`] が保持する内部状態。
///
/// - `durable`: 直近の `sync_data()` 完了時点のバイト列（電源断後も必ず残る像）。
/// - `len`: 現在の見かけ上のファイル長。`set_len` で即時更新するほか、`write()` が
///   現在の `len` を越える offset へ書き込んだ場合もその場で伸長する（POSIX の write
///   が EOF を越えて暗黙にファイルを伸長するのと同じ振る舞い）。`durable`（実データ）
///   の反映は `sync_data()` まで遅延する一方、`len` はメタデータとして即時更新する
///   単純化を置いている。この即時性により `len()` と `sync_data()` 後の
///   `durable_snapshot().len()` が常に一致する。
/// - `log`: 直近の `sync_data()` 以降に発行された `write()` の記録（発行順）。
///   `write()` はこの `log` に積むのみで `durable` を更新しない。`sync_data()` が
///   呼ばれて初めて `log` を `durable` へ反映する。sync を経ない書き込みが durable 像へ
///   紛れ込むと、シナリオ 4（`crate::storage::tests::power_loss`）の検証意図が成立しなく
///   なるため、この分離が本質。
#[derive(Debug)]
pub(crate) struct BackendState {
    pub(crate) durable: Vec<u8>,
    pub(crate) len: usize,
    pub(crate) log: Vec<(u64, Vec<u8>)>,
}

impl BackendState {
    pub(crate) fn new() -> Self {
        Self {
            durable: Vec::new(),
            len: 0,
            log: Vec::new(),
        }
    }

    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            durable: bytes,
            len,
            log: Vec::new(),
        }
    }

    /// `durable` を `len` にリサイズ（不足分はゼロ埋め）した基底バッファに、
    /// 未 sync の `log` をすべて適用した「見かけ上の現在の状態」を返す
    /// （sync 前の自分の書き込みは redb 自身には見える必要がある。read-your-writes）。
    ///
    /// `write()` は現在の `len` を越える offset へも書けて構わない（POSIX の write が
    /// EOF を越えて暗黙にファイルを伸長するのと同じ振る舞い）。この伸長自体は
    /// `write()` 側で `len` を更新する時点で確定しているため、`buf.resize(self.len, 0)`
    /// が既に `end` を包含している。以下の `if end > buf.len()` 分岐は、その不変条件が
    /// 崩れていないことに対する安全網（到達しないはずのフォールバック）として残す。
    /// 一方 `set_len` による明示的な縮小後は、`log` 自体が新しい長さへ切り詰め済み
    /// （`set_len` 参照）のため、縮小後 EOF より後ろの pending write がここで
    /// 幽霊のように復活することはない。
    pub(crate) fn current(&self) -> Vec<u8> {
        let mut buf = self.durable.clone();
        buf.resize(self.len, 0);
        for (offset, data) in &self.log {
            // `log` に積まれる offset/end は `write()` が `usize::try_from`・
            // `checked_add` で既に検証済み（同メソッド参照）なので、ここでの
            // truncating cast・非 checked 加算は安全。
            let start = *offset as usize;
            let end = start + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[start..end].copy_from_slice(data);
        }
        buf
    }
}

/// OS の page cache を模した `redb::StorageBackend` 実装。実ファイルを一切使わず、
/// メモリ上の [`BackendState`] だけで完結する（テストの決定論性・実行速度のため）。
/// `write()` は `log` に積むのみで `durable`（電源断後も必ず残る像）を更新せず、
/// `sync_data()` の時点で初めて `log` を `durable` へ反映する。`len` は `set_len` と
/// 同じ扱いのメタデータ操作として `write()` が即時更新する（[`BackendState`] の
/// フィールド doc 参照）。
#[derive(Debug, Clone)]
pub(crate) struct PowerLossBackend {
    pub(crate) state: Arc<Mutex<BackendState>>,
}

impl PowerLossBackend {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(BackendState::new())),
        }
    }

    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            state: Arc::new(Mutex::new(BackendState::from_bytes(bytes))),
        }
    }

    /// 直近 `sync_data()` 時点のバイト列（電源断で必ず残る像）を取得する。
    pub(crate) fn durable_snapshot(&self) -> Vec<u8> {
        self.state.lock().expect("lock poisoned").durable.clone()
    }

    /// 直近 `sync_data()` 以降に記録された未 sync の `write()` 件数。
    /// 「commit が実際に sync まで完了しているか」を検証するために使う。
    pub(crate) fn pending_write_count(&self) -> usize {
        self.state.lock().expect("lock poisoned").log.len()
    }
}

impl StorageBackend for PowerLossBackend {
    fn len(&self) -> io::Result<u64> {
        Ok(self.state.lock().expect("lock poisoned").len as u64)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let state = self.state.lock().expect("lock poisoned");
        let current = state.current();
        let start = offset as usize;
        let end = start
            .checked_add(out.len())
            .ok_or_else(|| io::Error::other("read range overflow"))?;
        let slice = current
            .get(start..end)
            .ok_or_else(|| io::Error::other("read out of bounds"))?;
        out.copy_from_slice(slice);
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut state = self.state.lock().expect("lock poisoned");
        let len = usize::try_from(len).map_err(|_| io::Error::other("len does not fit usize"))?;
        let old_len = state.len;
        state.len = len;
        // メタデータ操作は即時 durable 化する単純化（構造体 doc 参照）。
        state.durable.resize(len, 0);
        if len < old_len {
            // 縮小時、新しい EOF 以降の pending write は破棄し、EOF をまたぐ
            // write は新しい長さに合わせて切り詰める。切り詰めないと、`current`
            // が後で len を越えて再拡張してしまい、`len()` と実際のスナップショット
            // が矛盾する（電源断像として不正確になる）。
            state.log.retain_mut(|(offset, data)| {
                let start = *offset as usize;
                if start >= len {
                    return false;
                }
                let end = start.saturating_add(data.len());
                if end > len {
                    data.truncate(len - start);
                }
                true
            });
        }
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        let mut state = self.state.lock().expect("lock poisoned");
        let synced = state.current();
        state.durable = synced;
        state.log.clear();
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().expect("lock poisoned");
        // POSIX の write が EOF を越えて暗黙にファイルを伸長するのと同じ振る舞いを
        // モデル化するため、`offset + data.len()` が現在の `len` を越える場合は
        // `len` 自身もここで伸長する。log に積むだけで `len` を更新しないと、
        // sync_data() 後に `durable`（= current() のスナップショット）の実長が
        // `len` を上回り、`len()` と durable 像の長さが不整合になる。
        let offset_usize = usize::try_from(offset)
            .map_err(|_| io::Error::other("write offset does not fit usize"))?;
        let end = offset_usize
            .checked_add(data.len())
            .ok_or_else(|| io::Error::other("write range overflow"))?;
        state.len = state.len.max(end);
        state.log.push((offset, data.to_vec()));
        Ok(())
    }
}
