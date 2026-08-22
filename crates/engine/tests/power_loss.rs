//! 電源断シミュレーションによるクラッシュ耐性の再検証テスト（TASK-145、対象ビヘイビア:
//! なし（基盤タスク）。PERSIST-1・PERSIST-3（ポインタ: `docs/spec/04-behavior/persistence.md`）
//! を電源断シナリオへ拡張して再現する）。
//!
//! 実機の電源断は CI で再現できないため、`redb::StorageBackend`（`redb =4.2.0`。
//! `crates/engine/Cargo.toml` の既承認済み依存。新規依存は追加しない）をテストスコープで
//! 実装し、OS の page cache（sync 前の書き込みがメモリ上にのみ存在し、電源断で消失し得る
//! 状態）を決定論的にシミュレーションする（[`PowerLossBackend`]）。乱数は `rand` 等を
//! 追加せず、固定シードの xorshift（[`xorshift64`]）で代替する（dependency-policy.md 準拠）。
//!
//! ## モデルの限界（レポート `docs/design/crash-tolerance-reverification.md` 参照）
//!
//! 本シミュレーションはユーザー空間での近似であり、以下は対象外:
//! - 実デバイス（SSD/HDD）のファームウェアレベルの書き込みキャッシュ・並べ替え
//! - OS カーネルの page cache の実際の write-back 順序（本モデルは「発行順の任意部分集合が
//!   反映される」という単純化で近似する）
//! - `set_len`（ファイル長変更）はメタデータ操作として即時 durable 化する単純化を置く
//!   （多くのファイルシステムでメタデータジャーナリングは別経路のため、単純化として妥当と判断）
//! - シナリオ 3（[`run_partial_writeback_search`]）が探索する「部分集合」は、abort された
//!   トランザクションの `write()` ログに限られる。`redb` は copy-on-write B-tree のため、
//!   コミットに到達しなかったトランザクションの書き込みは新規割当ページ（既存のコミット済み
//!   ルートが一切参照しない領域）にしか行かれない。したがって本モデルの部分集合探索は
//!   **構造的に** コミット済みツリーへ干渉できず、`reopen_from_image` の `Err` 分岐へは
//!   到達し得ない（これは `redb` の頑健性の実証ではなく、本ハーネスの探索空間の限界である）。
//!   ハーネス自体が拒否を検出できることは、コミット済み像への直接バイト破損を用いた
//!   `power_loss_scenario3_corrupted_durable_image_is_rejected`（否定コントロール）で
//!   別途確認する。
//!
//! テストは `redb` を直接操作する（`tests/persistence.rs` と同じ前提。
//! `crates/engine/Cargo.toml` の `[dev-dependencies]` 参照）。`Storage`（`crates/engine/src/storage.rs`）
//! の公開 API 越しには `StorageBackend` を差し替えられないため、raw `redb::Database` を
//! `redb::Builder::create_with_backend` で開く。

use std::io;
use std::sync::{Arc, Mutex};

use redb::{Database, ReadableDatabase, StorageBackend, TableDefinition};

/// テスト対象のテーブル定義（`crates/engine/src/storage.rs` の `ROWS_TABLE` と同一の
/// キー・値型。本ファイルは行の中身を解釈しないため、値のエンコード詳細に依存しない）。
const ROWS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("rows");

/// [`PowerLossBackend`] が保持する内部状態。
///
/// - `durable`: 直近の `sync_data()` 完了時点のバイト列（電源断後も必ず残る像）。
/// - `len`: 現在の見かけ上のファイル長。`set_len` で即時更新する（モデルの限界を参照）。
/// - `log`: 直近の `sync_data()` 以降に発行された `write()` の記録（発行順）。
///   電源断シミュレーションでは、この一部（部分集合）だけが `durable` に反映された状態を
///   再現することで、write-back の並べ替え・部分反映を近似する。
#[derive(Debug)]
struct BackendState {
    durable: Vec<u8>,
    len: usize,
    log: Vec<(u64, Vec<u8>)>,
}

impl BackendState {
    fn new() -> Self {
        Self {
            durable: Vec::new(),
            len: 0,
            log: Vec::new(),
        }
    }

    fn from_bytes(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            durable: bytes,
            len,
            log: Vec::new(),
        }
    }

    /// `durable` を `len` にリサイズ（不足分はゼロ埋め）した基底バッファ。
    fn base(&self) -> Vec<u8> {
        let mut buf = self.durable.clone();
        buf.resize(self.len, 0);
        buf
    }

    /// `log` のうち `indices` に含まれるものだけを、記録順（`indices` は昇順を期待）で
    /// 基底バッファへ適用した結果を返す。全 `indices` を渡せば「現在の見かけ上の状態」
    /// （sync 前も含めた読み取り側の view）になり、空を渡せば「直近 sync 時点のまま」の
    /// 電源断像になる。
    fn apply_subset(&self, indices: &[usize]) -> Vec<u8> {
        let mut buf = self.base();
        for &i in indices {
            let (offset, data) = &self.log[i];
            let start = *offset as usize;
            let end = start + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[start..end].copy_from_slice(data);
        }
        buf
    }

    fn current(&self) -> Vec<u8> {
        let all: Vec<usize> = (0..self.log.len()).collect();
        self.apply_subset(&all)
    }
}

/// OS の page cache を模した `redb::StorageBackend` 実装。実ファイルを一切使わず、
/// メモリ上の [`BackendState`] だけで完結する（テストの決定論性・実行速度のため）。
///
/// `Arc<Mutex<..>>` で状態を共有し、`redb::Database` へ所有権を渡した後も、
/// テスト側は clone したハンドルから任意時点のスナップショットを取得できる
/// （「電源断」＝そのスナップショットのみを初期像として新しい `Database` を開き直すこと）。
#[derive(Debug, Clone)]
struct PowerLossBackend {
    state: Arc<Mutex<BackendState>>,
}

impl PowerLossBackend {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(BackendState::new())),
        }
    }

    fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            state: Arc::new(Mutex::new(BackendState::from_bytes(bytes))),
        }
    }

    /// 直近 `sync_data()` 時点のバイト列（電源断で必ず残る像）を取得する。
    fn durable_snapshot(&self) -> Vec<u8> {
        self.state.lock().expect("lock poisoned").durable.clone()
    }

    /// 直近 `sync_data()` 以降に記録された `write()` の件数。
    fn pending_write_count(&self) -> usize {
        self.state.lock().expect("lock poisoned").log.len()
    }

    /// 指定した部分集合（`log` へのインデックス。昇順であること）だけを直近 sync 像へ
    /// 適用した「電源断時点の像」を返す。
    fn crash_image(&self, subset: &[usize]) -> Vec<u8> {
        self.state
            .lock()
            .expect("lock poisoned")
            .apply_subset(subset)
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
        state.len = len;
        // メタデータ操作は即時 durable 化する単純化（モジュール doc の「モデルの限界」参照）。
        state.durable.resize(len, 0);
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
        state.log.push((offset, data.to_vec()));
        Ok(())
    }
}

/// テスト内蔵の固定シード xorshift64（`rand` 等の新規依存を追加しないための代替。
/// dependency-policy.md 準拠）。CI 用の小規模な部分集合探索にのみ使う。
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        // シード 0 は xorshift の不動点になるため回避する。
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// `total` 件の書き込みログのうち、確率的に選んだ部分集合（昇順インデックス列）を返す。
/// page cache の write-back が「発行順の任意部分集合だけが反映される」ことの近似
/// （モジュール doc「モデルの限界」参照）。
fn random_subset(rng: &mut Xorshift64, total: usize) -> Vec<usize> {
    (0..total)
        .filter(|_| rng.next_u64().is_multiple_of(2))
        .collect()
}

/// 新規（空）の [`PowerLossBackend`] 上に `redb::Database` を開く。
///
/// `cache_size` は既定（1GiB）のままだと、小規模なテストデータではページが
/// `redb` 内部の write buffer に留まり続け、`commit()`（＝ `sync_data()`）まで
/// `StorageBackend::write()` が一切呼ばれない（トランザクション全体がメモリ内で
/// 完結する）。これはシナリオ 2・3 が意図する「commit 前にページの一部が
/// バックエンドへ書き戻される」状況を作れないため、`set_cache_size` で
/// 小さな値に設定し、write buffer からの追い出し（eviction）を確実に起こす。
fn open_fresh() -> (PowerLossBackend, Database) {
    let backend = PowerLossBackend::new();
    let mut builder = redb::Builder::new();
    builder.set_cache_size(4096 * 4);
    let db = builder
        .create_with_backend(backend.clone())
        .expect("open fresh database on PowerLossBackend");
    (backend, db)
}

/// 指定バイト列を初期像として `redb::Database` を開き直す（「電源断後の再起動」に相当）。
/// 破損像で開けない場合は `Err` をそのまま返す（fail-closed の判定はテスト側で行う）。
fn reopen_from_image(image: Vec<u8>) -> Result<Database, redb::DatabaseError> {
    redb::Builder::new().create_with_backend(PowerLossBackend::from_bytes(image))
}

fn insert_row(db: &Database, id: u64, value: &[u8]) {
    let write_txn = db.begin_write().expect("begin write txn");
    {
        let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
        table.insert(id, value).expect("insert row");
    }
    write_txn.commit().expect("commit write txn");
}

fn get_row(db: &Database, id: u64) -> Option<Vec<u8>> {
    let read_txn = db.begin_read().expect("begin read txn");
    let table = match read_txn.open_table(ROWS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return None,
        Err(e) => panic!("unexpected open_table error: {e}"),
    };
    table
        .get(id)
        .expect("get row")
        .map(|guard| guard.value().to_vec())
}

// シナリオ 1（PERSIST-1 の電源断拡張）: commit 完了（応答済み）後に電源断しても、
// 再オープンでコミット済み行がすべて読める。
//
// `Storage::put`（≒ 本テストの `insert_row`）が返った時点で `redb` の既定 durability
// （`Durability::Immediate`）により `sync_data()` まで完了しているため、`durable_snapshot()`
// をそのまま電源断像として使えばよい（`log` は commit 完了後には空になっている）。
#[test]
fn power_loss_scenario1_committed_data_survives_crash_after_commit_response() {
    let (backend, db) = open_fresh();
    insert_row(&db, 1, b"committed-a");
    insert_row(&db, 2, b"committed-b");

    assert_eq!(
        backend.pending_write_count(),
        0,
        "Immediate durability の commit 完了後は log が sync 済みで空になっているはず\
         （観測: Storage の commit は応答前に sync している）"
    );

    let crash_image = backend.durable_snapshot();
    drop(db);

    let recovered = reopen_from_image(crash_image).expect("reopen after crash must succeed");
    assert_eq!(get_row(&recovered, 1), Some(b"committed-a".to_vec()));
    assert_eq!(get_row(&recovered, 2), Some(b"committed-b".to_vec()));
}

// シナリオ 2（PERSIST-1 の電源断拡張）: トランザクション途中（最終 sync 前）で電源断すると、
// 当該トランザクションは丸ごと消え、既存コミット済みデータは無傷で、DB は正常に開ける。
#[test]
fn power_loss_scenario2_mid_transaction_crash_discards_whole_transaction() {
    let (backend, db) = open_fresh();
    insert_row(&db, 1, b"pre-existing");

    // 直近 sync 時点（行 1 のコミット完了時点）のスナップショットをあらかじめ確保しておく。
    // 電源断像そのものではなく、後段で「部分 write-back が実際に電源断像へ混入したこと」
    // （`assert_ne!`）を確認するためのベースライン（write-back が一切起きなかった場合の像）
    // として使う。
    let pre_txn_image = backend.durable_snapshot();

    // 最終 sync（コミット）に到達しない書き込みを発生させる。`open_fresh` で cache_size を
    // 小さく設定しているため、十分な件数を書けば `redb` 内部の write buffer から
    // 追い出しが発生し、commit 前でも `StorageBackend::write()` 経由で `log` に記録される
    // （それでも `commit()` を呼ばない＝ Immediate durability の最終 sync には到達しないため、
    // 電源断時点で `durable` には反映されない）。
    {
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            for i in 0..64u64 {
                table
                    .insert(1_000 + i, &[0u8; 512][..])
                    .expect("insert uncommitted row");
            }
            table
                .insert(2u64, &b"never-committed"[..])
                .expect("insert uncommitted row");
        }
        write_txn.abort().expect("abort uncommitted txn");
    }

    let pending = backend.pending_write_count();
    assert!(
        pending > 0,
        "未コミットのページ書き込みが log に記録されているはず \
         （0 件だと本テストは何も検証していないことになる）"
    );

    // 「電源断時点の像」として、pre_txn_image（未コミット txn 開始前のスナップショット）
    // ではなく、abort された txn が実際にバックエンドへ書き戻したページをすべて反映した像
    // （crash_image(全 indices)）を使う。こうすることで、`open_fresh` の `set_cache_size`
    // 縮小によって発生させた部分 write-back が実際に電源断像へ混入していることを
    // `assert_ne!` で確認したうえで、それでもなお未コミット txn の内容は読めない
    // （commit していないためルートページが更新されておらず、不到達）ことを検証する。
    let all_indices: Vec<usize> = (0..pending).collect();
    let crash_image_with_partial_writeback = backend.crash_image(&all_indices);
    assert_ne!(
        crash_image_with_partial_writeback, pre_txn_image,
        "部分 write-back が電源断像へ実際に混入していなければ、本テストは \
         シナリオ 1 相当の検証（コミット後の電源断）と区別がつかない"
    );

    drop(db);
    let recovered = reopen_from_image(crash_image_with_partial_writeback)
        .expect("reopen after mid-transaction crash must succeed");
    assert_eq!(
        get_row(&recovered, 1),
        Some(b"pre-existing".to_vec()),
        "電源断前にコミット済みだった行は無傷でなければならない"
    );
    assert_eq!(
        get_row(&recovered, 2),
        None,
        "commit に到達しなかったトランザクションの内容は電源断後に見えてはならない \
         （部分 write-back 像を使っても、ルートページは未コミットのため不到達）"
    );
}

// シナリオ 3: 部分 write-back 像（page cache の書き戻し順序不定の近似）から再オープンした
// 場合、「正常に開けて内容は最後のコミット時点と一致」または「明示的なエラーで開けない
// （fail-closed）」のいずれかでなければならない。応答済みコミットの黙示的消失・
// 別内容へのすり替わりは 1 件でも観測されれば検証 NG として扱う（fail-closed 原則。
// security.md「不安全な設計」）。
//
// CI 実行分は固定シード・有限反復（数秒以内）に収める。より広い探索は
// `power_loss_scenario3_partial_writeback_extended_search`（`#[ignore]`）で行う。
#[test]
fn power_loss_scenario3_partial_writeback_is_either_consistent_or_rejected() {
    run_partial_writeback_search(32, 64);
}

// シナリオ 3 の拡張反復（ローカル実行用。CI では実行しない）。
// `cargo test -p engine -- --ignored` で実行し、結果は
// `docs/design/crash-tolerance-reverification.md` に記録する。
#[test]
#[ignore = "拡張反復のためローカル実行用（CI では #[ignore] を外さない）"]
fn power_loss_scenario3_partial_writeback_extended_search() {
    run_partial_writeback_search(2048, 64);
}

/// シナリオ 3 の本体。シード `1..=seed_count` それぞれについて、直近コミット完了時点の
/// バイト列と、その後の（commit まで到達しなかった）追加書き込みログから部分集合を
/// 固定シードで選び、その部分 write-back 像を電源断像として再オープンを試みる。
fn run_partial_writeback_search(seed_count: u64, extra_writes: u64) {
    let mut opened_consistent: u64 = 0;
    let mut rejected: u64 = 0;

    for seed in 1..=seed_count {
        let (backend, db) = open_fresh();
        insert_row(&db, 1, b"committed-before-crash");
        let committed_snapshot_len = backend.pending_write_count();
        assert_eq!(
            committed_snapshot_len, 0,
            "commit 直後は log が空（sync 済み）のはず"
        );

        // commit まで到達しない追加の書き込みトランザクションを 1 本開始する
        // （page cache 上には乗るが、電源断時点でどこまで反映されているかは不定）。
        {
            let write_txn = db.begin_write().expect("begin write txn");
            {
                let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
                for i in 0..extra_writes {
                    table
                        .insert(100 + i, &[0u8; 512][..])
                        .expect("insert uncommitted row");
                }
            }
            write_txn.abort().expect("abort uncommitted txn");
        }

        let total_log_len = backend.pending_write_count();
        // 部分集合の探索が意味を持つには、まず「commit に到達しなかった書き込みが
        // 実際にバックエンドへ記録されている」ことが前提になる。これが 0 件のままだと
        // `random_subset` は常に空集合しか返せず、以降の全反復が
        // `crash_image(&[])` == 直近 sync 像（＝行 1 のみのシナリオ 1 相当）を
        // 再オープンするだけの空検証になってしまう。
        assert!(
            total_log_len > 0,
            "seed={seed}: 未コミットのページ書き込みが log に記録されていない。\
             open_fresh の cache_size もしくは extra_writes（={extra_writes}）を見直すこと \
             （0 件のままだと本シナリオは部分 write-back を一切検証しない）"
        );

        let mut rng = Xorshift64::new(seed);
        let subset = random_subset(&mut rng, total_log_len);
        let durable_before = backend.durable_snapshot();
        let crash_image = backend.crash_image(&subset);
        // subset が実際にバイト列を変化させていることを確認する。`random_subset` は
        // 空集合を返しうるため、これがないと当該反復が `crash_image == durable_before`
        // （＝シナリオ 1 相当の空検証）に縮退していても気づけない。
        assert_ne!(
            crash_image, durable_before,
            "seed={seed}: 選ばれた部分集合が電源断像へ何も反映していない \
             （空検証に縮退している）"
        );
        drop(db);

        match reopen_from_image(crash_image) {
            Err(_) => {
                // fail-closed: 破損像を明示的に拒否するのは合格。
                rejected += 1;
            }
            Ok(recovered) => {
                // 開けた場合は、応答済みコミット（行 1）が消失・変質していないことを
                // 必須要件とする（黙示的なデータ欠落・すり替わりは 1 件でも検証 NG）。
                let row1 = get_row(&recovered, 1);
                assert_eq!(
                    row1,
                    Some(b"committed-before-crash".to_vec()),
                    "seed={seed}: 部分 write-back 像が開けた以上、応答済みコミット（行 1）は\
                     最後のコミット時点の内容と完全一致していなければならない \
                     （黙示的な欠落・すり替わりは fail-closed 違反として扱う）"
                );
                opened_consistent += 1;
            }
        }
    }

    // 実測した Ok/Err の内訳を記録する（`cargo test -- --nocapture` で観測し、
    // `docs/design/crash-tolerance-reverification.md` の結果表へ転記する。未計測のまま
    // 「多くは拒否される」等と根拠なく報告書に書かないための実測値）。
    //
    // 注意: `Err`（拒否）は fail-closed の観点では合格の結果であり、本テストは
    // `rejected` の値そのものをアサートしない（拒否が発生しても失敗にしてはならない）。
    // モジュール doc「モデルの限界」に記載の通り、本探索が扱う部分集合は abort された
    // トランザクションの書き込み（新規割当ページのみ）に限られ、コミット済みツリーには
    // 構造的に干渉できないため、この探索空間では実測上 `rejected == 0` が続いている
    // （`redb` の頑健性の実証ではなく、探索空間の限界に起因する）。拒否経路が実際に
    // 機能することは、コミット済み像への直接バイト破損を用いた
    // `power_loss_scenario3_corrupted_durable_image_is_rejected`（否定コントロール）で
    // 別途検証している。
    println!(
        "power_loss scenario3: seed_count={seed_count} extra_writes={extra_writes} \
         opened_and_consistent={opened_consistent} rejected_fail_closed={rejected}"
    );
}

// シナリオ 3 の否定コントロール: コミット済みの電源断像そのもの（`durable_snapshot()`）を
// 直接バイト破損させた場合、`reopen_from_image` が実際に `Err` を返すことを確認する。
// `run_partial_writeback_search` は abort された txn の部分集合適用ではコミット済みツリーへ
// 構造的に干渉できず拒否経路（`Err` 分岐）へ到達し得ない（モジュール doc「モデルの限界」
// 参照）。本テストはそれとは別に、ハーネス自体が破損像を実際に拒否できることを直接示す
// （テストの拒否分岐が到達不能なコードではないことの証明）。
#[test]
fn power_loss_scenario3_corrupted_durable_image_is_rejected() {
    let (backend, db) = open_fresh();
    insert_row(&db, 1, b"committed-before-corruption");

    let mut corrupted_image = backend.durable_snapshot();
    drop(db);

    assert!(
        corrupted_image.len() >= 64,
        "破損させる先頭 64 バイトを確保できる程度にはコミット済み像が存在するはず"
    );
    for byte in corrupted_image.iter_mut().take(64) {
        *byte = !*byte;
    }

    let result = reopen_from_image(corrupted_image);
    assert!(
        result.is_err(),
        "コミット済み像の先頭 64 バイトを反転させた場合、reopen_from_image は \
         明示的な Err を返さなければならない（fail-closed。実測でこの反転が \
         Err を引き起こすことを確認済み。内部レイアウトの主張はしない）。\
         Ok になった場合はハーネスが破損を検出できていないことを意味し、\
         run_partial_writeback_search の rejected==0 が「拒否経路が機能しない」ことに \
         起因していないかを疑う必要がある"
    );
}

/// `crates/engine/src/storage.rs` の v2 行レイアウト（RLS フィールドを行本体に同居させる形式）
/// を模した最小限のエンコード（テストローカル。engine のプライベート実装には依存しない。
/// レイアウトは `tests/persistence.rs` の
/// `persist3_rls_fields_are_colocated_in_single_row_entry_not_a_separate_table` と同一の
/// 前提を踏襲する）。dim・metadata は本シナリオの検証対象外のため 0 固定とする。
///
/// レイアウト: `[version(1)=2][tenant_len(2) le][tenant bytes][visibility(1)][dim(4) le=0][metadata_len(4) le=0]`
fn encode_rls_row(tenant_id: &str, visibility_byte: u8) -> Vec<u8> {
    let tenant_bytes = tenant_id.as_bytes();
    let mut buf = Vec::new();
    buf.push(2u8); // ROW_FORMAT_VERSION（v2）
    buf.extend_from_slice(&(tenant_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(tenant_bytes);
    buf.push(visibility_byte);
    buf.extend_from_slice(&0u32.to_le_bytes()); // dim = 0
    buf.extend_from_slice(&0u32.to_le_bytes()); // metadata_len = 0
    buf
}

// シナリオ 4（PERSIST-3 の電源断拡張）: RLS フィールド（tenant_id / visibility）が
// v2 行レイアウト内に同居した状態のまま、電源断後も無傷で保持される。
// `Storage` の公開 API は電源断シミュレーション用のバックエンド差し替えに対応していない
// ため（`Storage::open` は `redb::Database::create` 固定）、raw `redb::Database` に対して
// `encode_rls_row` で組み立てた v2 レイアウトのバイト列を直接書き込み、再オープン後に
// tenant_id・visibility バイトの位置を検査する（`tests/persistence.rs` の
// `persist3_rls_fields_are_colocated_in_single_row_entry_not_a_separate_table` と同じ手法）。
#[test]
fn power_loss_scenario4_rls_fields_survive_crash_after_commit() {
    const VISIBILITY_PUBLIC: u8 = 0x01;
    const VISIBILITY_PRIVATE: u8 = 0x02;

    let (backend, db) = open_fresh();
    let row_a = encode_rls_row("tenant-a", VISIBILITY_PUBLIC);
    let row_b = encode_rls_row("tenant-b", VISIBILITY_PRIVATE);
    insert_row(&db, 1, &row_a);
    insert_row(&db, 2, &row_b);

    let crash_image = backend.durable_snapshot();
    drop(db);

    let recovered = reopen_from_image(crash_image).expect("reopen after crash must succeed");

    let bytes1 = get_row(&recovered, 1).expect("row 1 must survive crash");
    assert_eq!(
        bytes1, row_a,
        "row 1 のバイト列は電源断前後で完全一致していなければならない"
    );
    let tenant_a_start = 1 + 2;
    let tenant_a_end = tenant_a_start + "tenant-a".len();
    assert_eq!(
        bytes1.get(tenant_a_start..tenant_a_end),
        Some(b"tenant-a".as_slice()),
        "tenant_id は電源断後も期待するオフセットに残っていなければならない"
    );
    assert_eq!(
        bytes1.get(tenant_a_end).copied(),
        Some(VISIBILITY_PUBLIC),
        "visibility バイトは電源断後も tenant_id 直後の期待するオフセットに残っていなければならない"
    );

    let bytes2 = get_row(&recovered, 2).expect("row 2 must survive crash");
    assert_eq!(
        bytes2, row_b,
        "row 2 のバイト列は電源断前後で完全一致していなければならない"
    );
    let tenant_b_start = 1 + 2;
    let tenant_b_end = tenant_b_start + "tenant-b".len();
    assert_eq!(
        bytes2.get(tenant_b_start..tenant_b_end),
        Some(b"tenant-b".as_slice())
    );
    assert_eq!(bytes2.get(tenant_b_end).copied(), Some(VISIBILITY_PRIVATE));
}
