//! `engine::arena::VectorArena` の経路比較テスト（TASK-87、対象ビヘイビア: TABLE-8。
//! ポインタ: `docs/spec/05-tasks.md` TASK-87・`docs/spec/04-behavior/data-model.md`
//! TABLE-8）。
//!
//! 「コールドスタート時に一度だけアリーナを構築し、以降の検索はアリーナ上のスライスを
//! 参照する経路」が「クエリの都度 `Storage::scan` で全行を読み直してデコードする経路」
//! より十分速いことを CI で検出可能にする（`tests/incremental_write_perf.rs`（TASK-143）
//! と同一の計測方針: ウォームアップ 1 回を除外・複数ラウンドの中央値比較・
//! `Duration::saturating_mul` の整数比較で判定・判定閾値は本テスト固有の計測パラメータで
//! spec の実測比そのものは転記しない）。
//!
//! 規模の選定: `Storage::scan()` は総バイト量 64MiB 超で `ScanLimitExceeded` を返す
//! （`storage.rs` 参照）。本テストの行数・次元は、その上限に対して十分な余裕を残し、かつ
//! debug ビルドでも CI 実行時間が長くなりすぎないよう小さく抑えている
//! （ROWS × DIM × 4 バイト ≈ 5.1 MiB で 64MiB に対して十分小さい）。
//!
//! ヘルパ（`unique_db_path` / `CleanupGuard` / 決定論的な埋め込み生成 / 中央値）は
//! `tests/incremental_write_perf.rs` の既存方針に倣い、このファイル内に複製する
//! （ヘルパ共通化はスコープ外）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engine::arena::VectorArena;
use engine::storage::{RowInput, Storage, Visibility};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-task87-arena-perf-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const TENANT_ID: &str = "tenant-a";

/// 行数・次元（モジュールドキュメントの規模選定を参照）。
const ROWS: u64 = 5_000;
const DIM: usize = 128;

/// 1 ラウンドで実行するクエリ本数。
const QUERY_COUNT: usize = 40;

/// ノイズ対策として、両経路それぞれを複数回計測し中央値を取る回数。
const MEASUREMENT_ROUNDS: usize = 3;

/// 判定閾値の分母（アリーナ経路は都度読み直し経路の `1 / RATIO_THRESHOLD_DENOM` 以下の
/// 時間で完了すること）。本テストの計測パラメータであり、アサーション弱体化は行わない
/// （`.claude/rules/coding-rust.md` 参照）。
const RATIO_THRESHOLD_DENOM: u32 = 4;

/// 外部クレート非依存の決定的擬似乱数生成器（xorshift32）。テストデータ生成にのみ使う。
struct Xorshift32(u32);

impl Xorshift32 {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u32() as f64 / u32::MAX as f64) as f32
    }
}

fn make_vector(rng: &mut Xorshift32, dim: usize) -> Vec<f32> {
    (0..dim).map(|_| rng.next_f32()).collect()
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values[values.len() / 2]
}

/// 単純な内積（テスト内の素朴なスコアリング。検索カーネル本体は後続タスクの管轄。
/// モジュールドキュメント参照）。
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn best_score_over_arena(arena: &VectorArena, query: &[f32]) -> f32 {
    let mut best = f32::MIN;
    for i in 0..arena.len() {
        let v = arena.vector(i).expect("index within arena bounds");
        let score = dot(v, query);
        if score > best {
            best = score;
        }
    }
    best
}

fn best_score_over_rescan(storage: &Storage, query: &[f32]) -> f32 {
    let rows = storage.scan().expect("scan within configured limits");
    let mut best = f32::MIN;
    for row in &rows {
        let score = dot(&row.embedding, query);
        if score > best {
            best = score;
        }
    }
    best
}

fn seed_storage(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    let mut rng = Xorshift32(0x2545_f491);
    let rows: Vec<(u64, Vec<f32>)> = (0..ROWS)
        .map(|id| (id, make_vector(&mut rng, DIM)))
        .collect();
    let batch: Vec<(u64, RowInput<'_>)> = rows
        .iter()
        .map(|(id, emb)| {
            (
                *id,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: emb,
                    metadata: b"m",
                },
            )
        })
        .collect();
    storage.put_batch(&batch).expect("seed dataset");
    storage
}

fn make_queries(seed: u32) -> Vec<Vec<f32>> {
    let mut rng = Xorshift32(seed | 1);
    (0..QUERY_COUNT)
        .map(|_| make_vector(&mut rng, DIM))
        .collect()
}

// 対象ビヘイビア: TABLE-8。「コールドスタート時に一度だけアリーナを構築し、以降の
// クエリはアリーナ走査で完結する経路」が「クエリの都度 Storage::scan で全行を
// 読み直しデコードする経路」より十分速いことを、判定閾値（RATIO_THRESHOLD_DENOM）で
// 検証する。
#[test]
fn table8_arena_query_path_completes_within_ratio_threshold_of_rescan_path() {
    let path = unique_db_path("dataset");
    let _cleanup = CleanupGuard(path.clone());
    let storage = seed_storage(&path);

    // ウォームアップ 1 回（ファイルシステムキャッシュ等の初回コストを計測から除外する。
    // 既存 perf テスト tests/incremental_write_perf.rs と同方針）。
    {
        let warmup_queries = make_queries(0xabad_1dea);
        let arena = VectorArena::build(&storage, DIM as u32).expect("warmup build arena");
        for q in &warmup_queries {
            std::hint::black_box(best_score_over_arena(&arena, q));
        }
        for q in &warmup_queries {
            std::hint::black_box(best_score_over_rescan(&storage, q));
        }
    }

    let mut arena_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    let mut rescan_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);

    for round in 0..MEASUREMENT_ROUNDS as u32 {
        let queries = make_queries(0x9e37_79b9u32.wrapping_add(round));

        // 経路 (a): コールドスタート・アリーナを一度 build し、各クエリはアリーナ走査で
        // 完結する。build 自体もこの経路のコストとして計測に含める（都度読み直し経路の
        // 各クエリが redb からの読み直しコストを含むのと対称にするため）。
        let started = Instant::now();
        let arena = VectorArena::build(&storage, DIM as u32).expect("build arena (measured)");
        for q in &queries {
            std::hint::black_box(best_score_over_arena(&arena, q));
        }
        arena_durations.push(started.elapsed());

        // 経路 (b): 各クエリごとに Storage::scan() で全行を読み直しデコードする。
        let started = Instant::now();
        for q in &queries {
            std::hint::black_box(best_score_over_rescan(&storage, q));
        }
        rescan_durations.push(started.elapsed());
    }

    let t_arena = median(arena_durations);
    let t_rescan = median(rescan_durations);
    let ratio = t_arena.as_secs_f64() / t_rescan.as_secs_f64().max(f64::EPSILON);

    // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
    println!(
        "table8 arena vs rescan perf: t_arena={t_arena:?} t_rescan={t_rescan:?} ratio={ratio:.4}"
    );

    assert!(
        t_arena.saturating_mul(RATIO_THRESHOLD_DENOM) <= t_rescan,
        "arena query path ({t_arena:?}) must complete within 1/{RATIO_THRESHOLD_DENOM} of the \
         rescan path ({t_rescan:?}), ratio={ratio:.4}"
    );
}
