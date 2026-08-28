//! 計測系結合テスト（時間・CPU 負荷に依存する回帰テスト）専用の直列化ヘルパー
//! （Issue #281 codex-review P2 指摘）。
//!
//! 背景: `tests/incremental_recall.rs`（TASK-121）が固有に持っていたタイミング用
//! ロック（プロセス内 `Mutex` ＋クロスプロセスファイルロック）は、そのファイル自身の
//! 3 テストしか直列化できず、`tests/incremental_write_perf.rs`（TASK-143・
//! PERSIST-2）等、同じく既存コーパス／既存行数に対する処理時間を比較する他の
//! integration-test バイナリはロックを取得しないため、`cargo test` が別プロセスとして
//! 並列起動するそれらのバイナリと計測区間が並走し得た。本モジュールへ一本化し、
//! 計測系テストファイルはすべて `#[path = "../src/test_util/timing_lock.rs"]` で
//! 同一ソースを取り込むことで、「同一の直列化資源（ロックファイルパス）を共有する」
//! という規約に従わせる。
//!
//! 呼び出し文脈: `tests/*.rs` から `#[path = "../src/test_util/timing_lock.rs"]
//! mod timing_lock;` で取り込む（`temp_db.rs` と同じ取り込み方式。本ファイルは
//! 意図的に `crate::` を参照せず `std` のみに依存する。dependency-policy 準拠）。
//!
//! 直列化の範囲: プロセス内 `Mutex`（`TIMING_LOCK`）は取り込んだテストバイナリ
//! ごとに個別の静的変数となるため、同一バイナリ内のテストのみを直列化する。
//! [`acquire_timing_lock`] が追加で取得する OS ファイルロック
//! （[`timing_lock_path`] が指す固定パスへの `File::lock`）はプロセス境界を越えて
//! 効くため、本モジュールを取り込む全 integration-test バイナリ・全 `cargo test`
//! プロセスをまたいで直列化する。
//!
//! 残存する既知の限界: 本ロックを取得しない他のテスト（本モジュールを取り込まない
//! 通常のユニットテスト・結合テスト）は、計測中も並列に走り得る。これらを含めた
//! 完全な隔離（例: `cargo test -- --test-threads=1` の強制）は計測系テストの
//! スコープを超えるため対象外とする。
#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// 本モジュールを取り込んだテストバイナリ内のテストを直列化するプロセス内ロック。
/// `#[path]` 取り込みのため、取り込んだバイナリごとに別インスタンスとなる
/// （バイナリ内直列化のみを担い、バイナリ間の直列化は [`acquire_timing_lock`] の
/// ファイルロックが担う）。
static TIMING_LOCK: Mutex<()> = Mutex::new(());

/// [`acquire_timing_lock`] が返すガード。Drop 順でファイルロック解放 → プロセス内
/// Mutex 解放となる（`File` の Drop で OS ロックが解放されるため明示 unlock は不要）。
pub struct TimingLockGuard {
    _process_guard: MutexGuard<'static, ()>,
    _lock_file: File,
}

/// クロスプロセス直列化用ロックファイルのパス。`CARGO_MANIFEST_DIR/target` 配下に
/// 固定し、`cargo clean` で自然に掃除される（一時ファイル配置は
/// `temp_db::unique_db_path` と同じ流儀）。ファイル自体の内容は使わず、OS のファイル
/// ロック機構（`File::lock`。Unix は `flock` 相当）の対象としてのみ使う。
///
/// 本モジュールを取り込む全ファイル・全プロセスが同一パスを指すことで、計測系
/// テスト全体で共有できる直列化資源とする（Issue #281 codex-review 指摘）。
pub fn timing_lock_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("incremental-recall-timing.lock")
}

/// 計測系テスト専用の直列化ガードを取得する（時間計測の開始前に必ず呼ぶ）。
/// プロセス内 Mutex（同一バイナリ内テストの直列化・poison 復帰）に加え、
/// [`timing_lock_path`] に対する OS ファイルロック（他 integration-test バイナリ・
/// 他 `cargo test` プロセスとの直列化）を取得する。
pub fn acquire_timing_lock() -> TimingLockGuard {
    let process_guard = TIMING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = timing_lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create timing lock directory");
    }
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .expect("open cross-process timing lock file");
    lock_file.lock().expect("acquire cross-process timing lock");
    TimingLockGuard {
        _process_guard: process_guard,
        _lock_file: lock_file,
    }
}
