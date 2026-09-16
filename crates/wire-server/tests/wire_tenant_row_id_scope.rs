//! `INSERT` 経路における行 `id` の一意性スコープ（テナント内）契約の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証（TASK-95、対象
//! ビヘイビア: TABLE-12・RLS-9。ポインタ: `docs/spec/05-tasks.md` TASK-95・
//! `docs/spec/04-behavior/data-model.md` TABLE-12・
//! `docs/spec/04-behavior/rls.md` RLS-9）。
//!
//! 意味論の確定オラクルは `crates/engine/tests/row_id_tenant_scope.rs`
//! （`engine::tenant` 直呼び出しによる「他テナント保持 id と未存在 id とで
//! 応答が変化しない」「同一テナント内重複は `23505` で拒否され、応答文言に
//! テナント名・行 id が現れない」ことの確定）であり、本ファイルは同じ規則が
//! **wire フレーミング**（実 TCP・簡易クエリプロトコル）越しにも観測できる
//! ことの追加確認に専念する（`wire_insert_operation_id.rs`・
//! `wire_emergency_response.rs` と同じ「意味論は engine 側オラクルに委ね、
//! フレーミングでの再現に専念する」流儀）。production コードの変更は不要
//! （既存の `sql/exec.rs::TenantWriteError::IdConflict → SqlSurfaceError::
//! IdConflict`（`23505`）写像・固定文言をそのまま `ErrorResponse` へ載せる
//! wire 層の既存経路を確認するのみ）。
//!
//! ファイル末尾（Issue #738）には、応答バイト列の同一性に加えて残る観測
//! チャネルである**レイテンシ分布**が (b) 他テナント保持 id・(c) 未存在 id
//! の間で統計的に区別できないことを検証する層 A（時間非依存の判定ロジック
//! 単体テスト・`make ci` 対象）・層 B（`#[ignore]` 実測。`make
//! wire-tenant-latency`）を追加している。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read as _;
use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

use common::*;

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
    )
}

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-tenant-row-id-scope");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `tenant-b`（id=100）・`tenant-c`（id=200）へ、wire を経由せず
/// `EngineCore::insert_row` で直接 1 行ずつ seed する（`crates/wire-server/tests/
/// wire_emergency_response.rs` と同じ「wire を経由しない直接 API 呼び出し」の
/// 流儀。3 テナント構成にするのは `row_id_tenant_scope.rs` の (i) と同じ理由——
/// 「自テナント（alice/tenant-a）から見て他テナントが `id` を保持しているか
/// 否か」だけを変数にするため）。
fn seed_foreign_tenants(core: &EngineCore) {
    let b = PolicyContext::new(TENANT_B).expect("valid tenant");
    let c = PolicyContext::new(TENANT_C).expect("valid tenant");
    core.insert_row(
        &b,
        TABLE,
        100,
        &RowInput {
            tenant_id: TENANT_B,
            visibility: Visibility::Public,
            embedding: &[0.0, 1.0, 0.0],
            metadata: b"seed-b",
        },
        Some(&OperationId::parse("seed-op-tenant-b").expect("valid operation_id")),
    )
    .expect("seed tenant-b row id=100");
    core.insert_row(
        &c,
        TABLE,
        200,
        &RowInput {
            tenant_id: TENANT_C,
            visibility: Visibility::Public,
            embedding: &[0.0, 0.0, 1.0],
            metadata: b"seed-c",
        },
        Some(&OperationId::parse("seed-op-tenant-c").expect("valid operation_id")),
    )
    .expect("seed tenant-c row id=200");
}

fn spawn_with_alice(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID '{op_id}'"
    )
}

/// 次のメッセージ 1 件を、型バイト＋長さ＋body の生バイト列のまま読む
/// （パース後の文字列比較ではなく、`CommandComplete`/`ErrorResponse` の
/// 全フィールドがバイト単位で一致することを固定するための汎用ヘルパー。
/// `common::read_command_complete` はタグ文字列へパースする専用ヘルパーの
/// ため、本 Issue の「応答全体の同一性」を確認するにはこちらを使う）。
fn read_raw_message(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let mut out = Vec::with_capacity(1 + len);
    out.push(header[0]);
    out.extend_from_slice(&len_buf);
    out.extend_from_slice(&body);
    out
}

/// 対象ビヘイビア: RLS-9。他テナント（tenant-b）が保持する `id` への
/// `INSERT` と、どのテナントも保持しない `id` への `INSERT` とで、wire 応答
/// （`CommandComplete` の生バイト列＝成否・タグ文言のすべて）が完全に一致
/// すること。物理キーが `(tenant_id, id)` で名前空間化されているため、他
/// テナントの行有無を参照する分岐が構造的に存在しないことをフレーミング
/// 越しに固定する。
#[test]
fn rls9_wire_insert_response_bytes_are_identical_for_foreign_held_id_and_absent_id() {
    let (core, _guard) = new_core_with_docs_table();
    seed_foreign_tenants(&core);
    let mut stream = spawn_with_alice(core);

    // (b) 他テナント（tenant-b）が保持する id=100 と同じ id への自テナント
    // 名義 INSERT。
    send_simple_query(&mut stream, &insert_sql(100, "wire-op-foreign-held"));
    let bytes_foreign_held = read_raw_message(&mut stream);
    assert_eq!(
        bytes_foreign_held.first(),
        Some(&b'C'),
        "expected CommandComplete for id held by another tenant, got: {bytes_foreign_held:?}"
    );
    read_ready_for_query(&mut stream);

    // (c) どのテナントも保持しない id=777 への INSERT。
    send_simple_query(&mut stream, &insert_sql(777, "wire-op-absent"));
    let bytes_absent = read_raw_message(&mut stream);
    assert_eq!(
        bytes_absent.first(),
        Some(&b'C'),
        "expected CommandComplete for an absent id, got: {bytes_absent:?}"
    );
    read_ready_for_query(&mut stream);

    assert_eq!(
        bytes_foreign_held, bytes_absent,
        "wire response bytes must be indistinguishable regardless of whether another tenant holds the id"
    );
}

/// 対象ビヘイビア: TABLE-12。他テナント（tenant-b・tenant-c）を事前に seed
/// した状態でも、同一テナント（tenant-a）内での重複 `id` への `INSERT` は
/// `23505` で拒否され、応答本文に他テナント名・行 `id`（重複対象自身の
/// `id`＝42 を含む）を含む識別子が漏えいしないこと。存在情報秘匿の回帰検証
/// （`row_id_tenant_scope.rs::rls9_insert_response_is_identical_...` と同型
/// のアサーション）。重複対象の id は `100`/`200`（他テナント seed 行の id）
/// と数字列として衝突しない `42` を使う（`row_id_tenant_scope.rs` の
/// `dup_with_foreign`/`dup_without_foreign` テストと同じ値。codex-review
/// 指摘: 従来は `id=1` を使っており、他テナント id（100/200）の非漏えいしか
/// 検査できず「重複対象自身の id が現れない」という本テストの目的を満たして
/// いなかった）。
#[test]
fn table12_wire_insert_duplicate_within_own_tenant_is_rejected_with_23505_without_leaking_row_identifiers(
) {
    let (core, _guard) = new_core_with_docs_table();
    seed_foreign_tenants(&core);
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, &insert_sql(42, "wire-op-dup-first"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // 同一 id・別 operation_id（台帳照合と行キー衝突を混同しないための注意点は
    // `wire_insert_operation_id.rs`・`row_id_tenant_scope.rs` と同じ）。
    send_simple_query(&mut stream, &insert_sql(42, "wire-op-dup-second"));
    let bytes = read_raw_message(&mut stream);
    assert_eq!(
        bytes.first(),
        Some(&b'E'),
        "expected ErrorResponse for a duplicate id within the same tenant, got: {bytes:?}"
    );
    let body_str = String::from_utf8_lossy(&bytes);
    assert!(
        body_str.contains("23505"),
        "expected SQLSTATE 23505, got: {body_str:?}"
    );
    assert!(
        !body_str.contains(TENANT_A)
            && !body_str.contains(TENANT_B)
            && !body_str.contains(TENANT_C),
        "error response must not leak a tenant identifier: {body_str:?}"
    );
    assert!(
        !body_str.contains("100") && !body_str.contains("200"),
        "error response must not leak another tenant's row id: {body_str:?}"
    );
    assert!(
        !body_str.contains("42"),
        "error response must not leak the duplicate row's own id: {body_str:?}"
    );
    read_ready_for_query(&mut stream);

    // 接続が維持されていることを確認する（後続の正規クエリが通ること）。
    send_simple_query(&mut stream, &insert_sql(2, "wire-op-after-dup"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}
// ============================================================================
// Issue #738（test(wire)）: (b) 他テナント保持 id・(c) 未存在 id への自テナント
// 名義 `INSERT` の wire 応答**レイテンシ分布**が統計的に区別できないことの
// 機械検証。Issue #737（上記 `rls9_wire_insert_response_bytes_are_identical_
// for_foreign_held_id_and_absent_id`）は応答バイト列の同一性を固定したが、
// 残る観測チャネルはレイテンシであり、TASK-95・TABLE-12・RLS-9 の確定化には
// 「(b) と (c) のレイテンシ分布が統計的に区別できない」ことの追加確認が要る。
//
// 判定は `benchmark-judgement-policy.md`（public・本リポ側の実装既定値）の
// 「固定相対帯」と「実測参照帯（A/A 分割）」の 2 種のノイズ帯を両方超えて
// 初めて `Distinguishable`（fail）とする方式を、タイミング副チャネルの
// 不在検証（同経路であることの確認）へ適用したもの。判定ロジック（`judge`
// 以下）は実測タイマーを一切使わない時間非依存の純関数として層 A
// （`cargo test`・`make ci` 対象）で固定し、計測本体（`#[ignore]`・
// `make wire-tenant-latency`）は層 B として分離する（`tier_latency_accept.rs`
// と同じ層分離方針）。
// ============================================================================

/// `TENANT_LATENCY_ROUNDS` 未指定時の 1 腕あたり計測ラウンド数。
const DEFAULT_ROUNDS: usize = 200;
/// `resolve_rounds` が受理する最小値（200 未満は統計的に信頼できないとして
/// fail-closed で拒否する。本 Issue の実装既定値）。
const MIN_ROUNDS: usize = 200;
/// `TENANT_LATENCY_WARMUP` 未指定時のウォームアップ往復回数（統計から除外）。
const DEFAULT_WARMUP: usize = 20;

/// 固定相対帯（`benchmark-judgement-policy.md` §4 が Issue #401 から継承する
/// 非退行閾値と同じ値を、同一性検証の許容差として転用する。本リポの実装
/// 既定値）。
const FIXED_BAND_MEDIAN: f64 = 0.05;
const FIXED_BAND_P95: f64 = 0.10;

/// 実測参照帯（A/A 分割）の上限値。これを超える場合は環境ノイズが判定に
/// 使えないほど大きいとみなし `Inconclusive` とする（vacuous pass 防止。
/// 固定帯の 5 倍を「参照帯として無意味」とみなす本リポの実装既定値）。
const AA_BAND_MEDIAN_CAP: f64 = 0.25;
const AA_BAND_P95_CAP: f64 = 0.50;

/// `TENANT_LATENCY_ROUNDS` を検証しつつ解決する（時間非依存の純関数。
/// `env::var` の結果を直接受け取るのではなく `Option<&str>` を引数化する
/// ことで層 A から実行時タイマー・env に依存せず単体テストできる。
/// `tier_latency_bench.rs::parse_max_p95_ms` 系と同じ fail-closed 方針）。
fn resolve_rounds(raw: Option<&str>) -> Result<usize, String> {
    match raw {
        None => Ok(DEFAULT_ROUNDS),
        Some(s) => {
            let trimmed = s.trim();
            let n: usize = trimmed.parse().map_err(|_| {
                format!("TENANT_LATENCY_ROUNDS must be a positive integer, got {s:?}")
            })?;
            if n < MIN_ROUNDS {
                return Err(format!(
                    "TENANT_LATENCY_ROUNDS must be >= {MIN_ROUNDS} (statistically unreliable below this), got {n}"
                ));
            }
            Ok(n)
        }
    }
}

/// `TENANT_LATENCY_WARMUP` を検証しつつ解決する（`resolve_rounds` と同じ
/// 方針。ウォームアップは統計対象外のため下限は課さない）。
fn resolve_warmup(raw: Option<&str>) -> Result<usize, String> {
    match raw {
        None => Ok(DEFAULT_WARMUP),
        Some(s) => {
            let trimmed = s.trim();
            trimmed.parse().map_err(|_| {
                format!("TENANT_LATENCY_WARMUP must be a non-negative integer, got {s:?}")
            })
        }
    }
}

/// `wire_concurrency_throughput.rs::percentile` と同式（`idx =
/// round((n-1)*p)`）で再実装し、統計量の算出方法を本リポ内で整合させる。
/// `sorted` は昇順ソート済みであることを呼び出し元が保証する。
fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 計測順（取得順）の偶奇で 2 分割する（A/A 分割の分割方法。同一経路・
/// 同一 run の同一腕を 2 分割することで「ノイズだけでも動きうる幅」の
/// 実測値を得る）。
fn split_even_odd(samples: &[u128]) -> (Vec<u128>, Vec<u128>) {
    let mut even = Vec::with_capacity(samples.len().div_ceil(2));
    let mut odd = Vec::with_capacity(samples.len() / 2);
    for (i, &s) in samples.iter().enumerate() {
        if i % 2 == 0 {
            even.push(s);
        } else {
            odd.push(s);
        }
    }
    (even, odd)
}

/// `a` と `b` の対称相対差（`|a-b| / max(a,b)`）。分母に大小どちらの引数を
/// 渡しても同じ値を返す（Issue #738 codex-review 指摘: 旧実装 `|a/b - 1|`
/// は `b` を分母に固定しており、どちらの腕を第 2 引数に渡すかで判定が
/// 反転しうる非対称バグだった）。両者とも 0 の場合のみ差なしとみなす。
fn relative_diff(a: u128, b: u128) -> f64 {
    let (a, b) = (a as f64, b as f64);
    let denom = a.max(b);
    if denom == 0.0 {
        return 0.0;
    }
    (a - b).abs() / denom
}

/// 分位点ラダー上の 1 点が使う帯の種別（`median` 系はタイトな固定帯・
/// `tail` 系（p90 以上）はサンプル数が薄くなる分だけ緩い固定帯を使う。
/// `judge` の量子化点ごとの帯選択を型で明示する）。
#[derive(Debug, Clone, Copy)]
enum BandKind {
    Median,
    Tail,
}

/// 診断表示（実行記録の可読なサマリ）専用の分位点ラダー。**`judge` 自身の
/// 判定基盤としてはもう使われない**（Issue #738 codex-review 追加指摘:
/// 固定分位点をどれだけ細かく刻んでも、両腕の差分がその刻み幅より狭い
/// 部分母集団として現れれば、ラダー点の**間**を通過して見逃される反例が
/// 常に構築できる——例えば 200 件中 62〜67 件目だけが他方と異なる
/// b=[1000×62,2000×6,3000×132] / c=[1000×62,2500×6,3000×132] は、5% 刻み
/// ラダー〔p30=idx60・p35=idx70〕の間をすり抜ける。刻み幅を狭める対症療法
/// では原理的に解消できないため、`judge` は固定分位点ではなく b・c 両ソート
/// 済み配列の**全順序統計量**（`0..rounds` の全インデックス）を直接比較する
/// 方式へ変更した（`judge_detects_subpopulation_between_any_fixed_quantile_
/// points` 参照）。このラダーは実行記録（層 B の println! 出力）を人間が
/// 読みやすい代表点に絞って提示するためだけに残置する）。
const QUANTILE_LADDER: &[(f64, BandKind)] = &[
    (0.00, BandKind::Median),
    (0.05, BandKind::Median),
    (0.10, BandKind::Median),
    (0.15, BandKind::Median),
    (0.20, BandKind::Median),
    (0.25, BandKind::Median),
    (0.30, BandKind::Median),
    (0.35, BandKind::Median),
    (0.40, BandKind::Median),
    (0.45, BandKind::Median),
    (0.50, BandKind::Median),
    (0.55, BandKind::Median),
    (0.60, BandKind::Median),
    (0.65, BandKind::Median),
    (0.70, BandKind::Median),
    (0.75, BandKind::Median),
    (0.80, BandKind::Median),
    (0.85, BandKind::Median),
    (0.90, BandKind::Tail),
    (0.95, BandKind::Tail),
    (0.99, BandKind::Tail),
];

/// 1 腕のサンプル列を計測順の偶奇で 2 分割し、指定した分位点における
/// 相対差（A/A 帯）を返す。
fn quantile_aa_diff(samples: &[u128], p: f64) -> f64 {
    let (mut even, mut odd) = split_even_odd(samples);
    even.sort_unstable();
    odd.sort_unstable();
    relative_diff(percentile(&even, p), percentile(&odd, p))
}

/// レイテンシ分布同一性の判定結果（計画 §3.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// 両ノイズ帯を超えて区別できる差が観測された（fail。タイミング副
    /// チャネルによる存在情報漏えいの疑い）。
    Distinguishable,
    /// A/A 帯（環境ノイズ）そのものが大きすぎて判定に使えない
    /// （fail。vacuous pass を防ぐための明示的な判定不能）。
    Inconclusive,
    /// 両ノイズ帯を超える差は観測されなかった（pass）。
    Indistinguishable,
}

/// 固定相対帯・A/A 帯上限のペア（median 系・tail 系）。テストから閾値を
/// 差し替えられるよう構造体化する。
#[derive(Debug, Clone, Copy)]
struct Bands {
    fixed_median: f64,
    fixed_p95: f64,
    aa_median_cap: f64,
    aa_p95_cap: f64,
}

impl Default for Bands {
    fn default() -> Self {
        Self {
            fixed_median: FIXED_BAND_MEDIAN,
            fixed_p95: FIXED_BAND_P95,
            aa_median_cap: AA_BAND_MEDIAN_CAP,
            aa_p95_cap: AA_BAND_P95_CAP,
        }
    }
}

impl Bands {
    /// `BandKind` に応じた (固定帯, A/A 帯上限) のペアを返す。
    fn for_kind(&self, kind: BandKind) -> (f64, f64) {
        match kind {
            BandKind::Median => (self.fixed_median, self.aa_median_cap),
            BandKind::Tail => (self.fixed_p95, self.aa_p95_cap),
        }
    }
}

/// 偶奇 2 分割済み（呼び出し元でソート済み）の 2 配列から、指定した
/// 分位点における相対差（A/A 帯）を返す（`quantile_aa_diff` の事前計算版。
/// `judge` の全順序統計量走査で同じ偶奇分割・ソートを毎回作り直さない
/// ための最適化。分割方法・意味は `quantile_aa_diff` と同一）。
fn aa_diff_from_sorted_split(even_sorted: &[u128], odd_sorted: &[u128], p: f64) -> f64 {
    relative_diff(percentile(even_sorted, p), percentile(odd_sorted, p))
}

/// (b)（他テナント保持 id）・(c)（未存在 id）2 腕のサンプル列からレイテンシ
/// 分布の同一性を判定する（時間非依存の純関数。実測タイマー・env を一切
/// 参照しない。`tier_latency_bench.rs::judge` と同じ「計測本体から分離した
/// 判定ロジック」の方針）。`b`・`c` は各腕の生サンプル列（計測順のまま。
/// 内部でソート・A/A 分割の双方に使う）。両腕は同じラウンド数で計測される
/// 契約のため長さ不一致は呼び出し元の不変条件違反として `assert_eq!` で
/// fail-closed に落とす（長さが異なると「同じ分位点」の対応が取れない）。
///
/// 固定分位点のラダーではなく、b・c 両ソート済み配列の**全順序統計量**
/// （インデックス `0..rounds` の全点）を直接比較する（Issue #738
/// codex-review 追加指摘への対応: 固定ラダーは刻み幅をどれだけ狭めても、
/// 両腕の差分がその刻み幅より狭い部分母集団として現れれば、ラダー点の
/// **間**を通過して見逃される反例が原理的に常に構築できる。ソート済み
/// 配列を全インデックスで直接比較すれば、どの部分母集団も必ずどこかの
/// インデックスで捕捉されるため、この種の"間"自体が存在しなくなる）。
/// A/A 帯も同じ全インデックス走査に合わせ、各インデックスの相対位置
/// `idx/(n-1)` を分位点として使う（`quantile_aa_diff` と同じ計算式）。
/// ある位置の A/A 帯が上限を超えていても他の位置の判定は継続し、最終的に
/// 1 点でも上限超過があれば `Inconclusive` を優先する（既存の
/// 「Inconclusive が Distinguishable より優先される」契約を維持する）。
fn judge(b: &[u128], c: &[u128], bands: &Bands) -> Verdict {
    assert_eq!(
        b.len(),
        c.len(),
        "judge requires both arms to have the same sample count (rounds); got b={} c={}",
        b.len(),
        c.len()
    );

    let mut b_sorted = b.to_vec();
    let mut c_sorted = c.to_vec();
    b_sorted.sort_unstable();
    c_sorted.sort_unstable();

    let (mut b_even, mut b_odd) = split_even_odd(b);
    b_even.sort_unstable();
    b_odd.sort_unstable();
    let (mut c_even, mut c_odd) = split_even_odd(c);
    c_even.sort_unstable();
    c_odd.sort_unstable();

    let n = b_sorted.len();
    let mut any_aa_over_cap = false;
    let mut any_distinguishable = false;

    for idx in 0..n {
        let p = if n > 1 {
            idx as f64 / (n - 1) as f64
        } else {
            0.0
        };
        let kind = if p >= 0.90 {
            BandKind::Tail
        } else {
            BandKind::Median
        };
        let (fixed_band, aa_cap) = bands.for_kind(kind);
        let aa = aa_diff_from_sorted_split(&b_even, &b_odd, p)
            .max(aa_diff_from_sorted_split(&c_even, &c_odd, p));

        if aa > aa_cap {
            any_aa_over_cap = true;
            continue;
        }

        let delta = relative_diff(b_sorted[idx], c_sorted[idx]);
        if delta > fixed_band.max(aa) {
            any_distinguishable = true;
        }
    }

    if any_aa_over_cap {
        return Verdict::Inconclusive;
    }
    if any_distinguishable {
        return Verdict::Distinguishable;
    }
    Verdict::Indistinguishable
}

/// 次のメッセージを型バイトのみ検査し（`CommandComplete` 以外は panic）、
/// 送信から `ReadyForQuery` 受信完了までの往復時間（マイクロ秒）を返す
/// （`wire_concurrency_throughput.rs::run_one_query` と同じ計測範囲）。
fn measure_insert_round_trip(stream: &mut TcpStream, sql: &str) -> u128 {
    let start = std::time::Instant::now();
    send_simple_query(stream, sql);
    let bytes = read_raw_message(stream);
    assert_eq!(
        bytes.first(),
        Some(&b'C'),
        "expected CommandComplete during latency measurement, got: {bytes:?}"
    );
    read_ready_for_query(stream);
    start.elapsed().as_micros()
}

/// tenant-b へ `ids` の各行を wire を経由せず `EngineCore::insert_row` で
/// seed する（(b) 腕用。`seed_foreign_tenants` と同じ「wire 非経由の直接
/// API 呼び出し」の流儀。tenant-c への seed は不要——(c) 腕はどのテナントも
/// 保持しない id が前提のため）。
fn seed_foreign_ids(core: &EngineCore, ids: &[u64]) {
    let b = PolicyContext::new(TENANT_B).expect("valid tenant");
    for &id in ids {
        let op_id = format!("lat-seed-b-{id:07}");
        core.insert_row(
            &b,
            TABLE,
            id,
            &RowInput {
                tenant_id: TENANT_B,
                visibility: Visibility::Public,
                embedding: &[0.0, 1.0, 0.0],
                metadata: b"seed-lat-b",
            },
            Some(&OperationId::parse(&op_id).expect("valid operation_id")),
        )
        .expect("seed tenant-b row for latency measurement");
    }
}

/// ABBA 交互実行のための腕識別子（`benchmark-judgement-policy.md` §3 の
/// 交互実行規約。テーブル成長・redb commit コストの時間ドリフト・ペア内の
/// 先行/後続バイアスを両腕へ均等に配る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    ForeignHeld,
    Absent,
}

/// `count` 件ずつを `b,c,c,b` の順で循環させる ABBA スケジュールを組み立てる
/// （時間非依存の純関数。両腕とも `count` 件に達したら終了）。
fn abba_schedule(count: usize) -> Vec<Arm> {
    let block = [Arm::ForeignHeld, Arm::Absent, Arm::Absent, Arm::ForeignHeld];
    let mut schedule = Vec::with_capacity(count * 2);
    let mut b_remaining = count;
    let mut c_remaining = count;
    let mut i = 0usize;
    while b_remaining > 0 || c_remaining > 0 {
        match block[i % block.len()] {
            Arm::ForeignHeld if b_remaining > 0 => {
                schedule.push(Arm::ForeignHeld);
                b_remaining -= 1;
            }
            Arm::Absent if c_remaining > 0 => {
                schedule.push(Arm::Absent);
                c_remaining -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    schedule
}

/// `count` 件ぶんの ABBA 交互実行を 1 フェーズ実行し、各腕の生サンプル列
/// （計測順のまま）を返す。`b_id_base`／`c_id_base` は id の衝突を避ける
/// ための各腕・各フェーズ専用のオフセット（呼び出し元がフェーズ間で重複
/// しない値を渡す）。
fn run_latency_phase(
    stream: &mut TcpStream,
    b_id_base: u64,
    c_id_base: u64,
    count: usize,
) -> (Vec<u128>, Vec<u128>) {
    let schedule = abba_schedule(count);
    let mut b_samples = Vec::with_capacity(count);
    let mut c_samples = Vec::with_capacity(count);
    let mut b_i = 0u64;
    let mut c_i = 0u64;
    for arm in schedule {
        match arm {
            Arm::ForeignHeld => {
                let id = b_id_base + b_i;
                let op_id = format!("lat-b-{id:07}");
                b_samples.push(measure_insert_round_trip(stream, &insert_sql(id, &op_id)));
                b_i += 1;
            }
            Arm::Absent => {
                let id = c_id_base + c_i;
                let op_id = format!("lat-c-{id:07}");
                c_samples.push(measure_insert_round_trip(stream, &insert_sql(id, &op_id)));
                c_i += 1;
            }
        }
    }
    (b_samples, c_samples)
}

/// (b) 他テナント（tenant-b）保持 id・(c) 未存在 id への自テナント名義
/// `INSERT` の wire レイテンシ分布が統計的に区別できないことを検証する
/// 手動専用の計測テスト（Issue #738・TASK-95・TABLE-12・RLS-9）。
///
/// `TENANT_LATENCY_ROUNDS`（既定 200・200 未満は拒否）・
/// `TENANT_LATENCY_WARMUP`（既定 20）で規模を上書きできる。1 プロセス =
/// 1 計測（`benchmark-judgement-policy.md` §5 準拠）。spec 閾値を持たない
/// 情報提供専用の性能検証と同じく CI 非配線（`GITHUB_ACTIONS` 下は
/// fail-closed で拒否する。`knn_wire_profile_accept.rs` 系と同じ方針）。
#[test]
#[ignore]
fn rls9_wire_insert_latency_distribution_is_indistinguishable_for_foreign_held_id_and_absent_id() {
    assert!(
        std::env::var("GITHUB_ACTIONS").is_err(),
        "this manual latency benchmark must not run under GITHUB_ACTIONS (shared/noisy CI environment invalidates the noise bands)"
    );

    let rounds = resolve_rounds(std::env::var("TENANT_LATENCY_ROUNDS").ok().as_deref())
        .expect("TENANT_LATENCY_ROUNDS must be valid");
    let warmup = resolve_warmup(std::env::var("TENANT_LATENCY_WARMUP").ok().as_deref())
        .expect("TENANT_LATENCY_WARMUP must be valid");

    // フェーズ間で id が衝突しないよう、腕・フェーズごとに十分離れた
    // オフセットを割り当てる（warmup・rounds とも現実的な範囲であれば
    // 衝突しない安全マージン）。
    const B_WARMUP_BASE: u64 = 1_000_000;
    const B_MEASURED_BASE: u64 = 2_000_000;
    const C_WARMUP_BASE: u64 = 3_000_000;
    const C_MEASURED_BASE: u64 = 4_000_000;

    let (core, _guard) = new_core_with_docs_table();
    let b_warmup_ids: Vec<u64> = (0..warmup as u64).map(|i| B_WARMUP_BASE + i).collect();
    let b_measured_ids: Vec<u64> = (0..rounds as u64).map(|i| B_MEASURED_BASE + i).collect();
    seed_foreign_ids(&core, &b_warmup_ids);
    seed_foreign_ids(&core, &b_measured_ids);

    let mut stream = spawn_with_alice(core);

    // ウォームアップ（統計から除外。接続直後の cold path を計測区間から
    // 除く）。
    let _ = run_latency_phase(&mut stream, B_WARMUP_BASE, C_WARMUP_BASE, warmup);

    let (b_samples, c_samples) =
        run_latency_phase(&mut stream, B_MEASURED_BASE, C_MEASURED_BASE, rounds);

    let mut b_sorted = b_samples.clone();
    let mut c_sorted = c_samples.clone();
    b_sorted.sort_unstable();
    c_sorted.sort_unstable();

    let bands = Bands::default();
    let verdict = judge(&b_samples, &c_samples, &bands);

    println!("=== rls9_wire_insert_latency_distribution ===");
    println!("rounds={rounds} warmup={warmup}");
    println!("nproc={:?}", std::thread::available_parallelism());
    println!(
        "foreign_held(b)_us: min={} median={} p95={} max={}",
        b_sorted.first().copied().unwrap_or(0),
        percentile(&b_sorted, 0.50),
        percentile(&b_sorted, 0.95),
        b_sorted.last().copied().unwrap_or(0)
    );
    println!(
        "absent(c)_us: min={} median={} p95={} max={}",
        c_sorted.first().copied().unwrap_or(0),
        percentile(&c_sorted, 0.50),
        percentile(&c_sorted, 0.95),
        c_sorted.last().copied().unwrap_or(0)
    );
    // 分位点ラダー代表点の delta・A/A 帯を出力する（人間が読みやすい要約
    // 表示専用。`judge` 自身の判定基盤は Issue #738 codex-review 追加指摘
    // への対応でこのラダーではなく全順序統計量走査へ変更済み——固定分位点
    // をどれだけ細かく刻んでも刻み幅より狭い部分母集団は原理的に見逃し
    // うるため。実際の判定根拠を隠さず記録するため、次に全 `rounds` 点の
    // 走査で delta が最大だったインデックスも出力する）。
    for &(p, kind) in QUANTILE_LADDER {
        let (fixed_band, aa_cap) = bands.for_kind(kind);
        let delta = relative_diff(percentile(&b_sorted, p), percentile(&c_sorted, p));
        let aa = quantile_aa_diff(&b_samples, p).max(quantile_aa_diff(&c_samples, p));
        println!(
            "quantile p={p:.2} b={} c={} delta={delta:.4} aa={aa:.4} fixed_band={fixed_band:.4} aa_cap={aa_cap:.4}",
            percentile(&b_sorted, p),
            percentile(&c_sorted, p)
        );
    }
    // `judge` が実際に使う全順序統計量走査（`0..rounds` の全インデックス）
    // のうち delta が最大だった点を記録する（ラダー代表点表示だけでは
    // `judge` が実際に見ている情報の一部しか示せないため）。
    let full_scan_len = b_sorted.len().min(c_sorted.len());
    let mut worst: Option<(usize, f64, f64, f64)> = None; // (idx, p, delta, aa)
    for idx in 0..full_scan_len {
        let p = if full_scan_len > 1 {
            idx as f64 / (full_scan_len - 1) as f64
        } else {
            0.0
        };
        let delta = relative_diff(b_sorted[idx], c_sorted[idx]);
        let aa = quantile_aa_diff(&b_samples, p).max(quantile_aa_diff(&c_samples, p));
        let is_worse = match worst {
            Some((_, _, best_delta, _)) => delta > best_delta,
            None => true,
        };
        if is_worse {
            worst = Some((idx, p, delta, aa));
        }
    }
    if let Some((idx, p, delta, aa)) = worst {
        println!(
            "full_scan_worst_index idx={idx} p={p:.4} b={} c={} delta={delta:.4} aa={aa:.4}",
            b_sorted[idx], c_sorted[idx]
        );
    }
    println!("verdict={verdict:?}");
    // per-run 生データ（両腕・取得順のまま）を必須で残す
    // （`benchmark-judgement-policy.md` §3 の per-run 生データ必須の教訓・
    // codex-review 指摘への対応。事後の再判定・別の判定方式への差し替えを
    // 可能にする）。
    println!("foreign_held(b)_raw_us_acquisition_order={b_samples:?}");
    println!("absent(c)_raw_us_acquisition_order={c_samples:?}");
    println!(
        "note: shared/CI environment values are reference-only per docs/design/benchmark-judgement-policy.md"
    );

    match verdict {
        Verdict::Indistinguishable => {}
        Verdict::Distinguishable => panic!(
            "wire INSERT round-trip latency is statistically distinguishable between a \
             foreign-tenant-held id and an absent id (potential timing side channel); see \
             the printed summary above for delta/AA band values"
        ),
        Verdict::Inconclusive => panic!(
            "environment noise (A/A band) exceeds the upper bound; the measurement cannot \
             confirm indistinguishability in this run — re-run with less concurrent load or \
             a higher TENANT_LATENCY_ROUNDS"
        ),
    }
}

// --- 層A相当: 時間非依存の判定ロジック単体テスト（`benchmark-judgement-
//     policy.md` §5「1 プロセス = 1 計測」の裏で使う `judge` 自体は実測
//     タイマーに依存しないため `make ci` 対象の通常テストとして固定する） ---

#[cfg(test)]
mod tenant_latency_judge_tests {
    use super::*;

    #[test]
    fn resolve_rounds_defaults_to_200_when_unset() {
        assert_eq!(resolve_rounds(None).unwrap(), 200);
    }

    #[test]
    fn resolve_rounds_accepts_the_minimum_and_rejects_below_it() {
        assert_eq!(resolve_rounds(Some("200")).unwrap(), 200);
        assert!(resolve_rounds(Some("199")).is_err());
    }

    #[test]
    fn resolve_rounds_rejects_non_integer_and_empty() {
        assert!(resolve_rounds(Some("")).is_err());
        assert!(resolve_rounds(Some("abc")).is_err());
        assert!(resolve_rounds(Some("200.5")).is_err());
        assert!(resolve_rounds(Some("-1")).is_err());
    }

    #[test]
    fn resolve_warmup_defaults_to_20_when_unset() {
        assert_eq!(resolve_warmup(None).unwrap(), 20);
    }

    #[test]
    fn resolve_warmup_accepts_zero_and_rejects_non_integer() {
        assert_eq!(resolve_warmup(Some("0")).unwrap(), 0);
        assert!(resolve_warmup(Some("abc")).is_err());
        assert!(resolve_warmup(Some("-1")).is_err());
    }

    #[test]
    fn percentile_matches_wire_concurrency_throughput_formula() {
        let sorted = vec![10u128, 20, 30, 40, 50];
        // idx = round((5-1)*0.5) = 2 -> sorted[2] = 30
        assert_eq!(percentile(&sorted, 0.50), 30);
        // idx = round((5-1)*0.95) = round(3.8) = 4 -> sorted[4] = 50
        assert_eq!(percentile(&sorted, 0.95), 50);
        assert_eq!(percentile(&[], 0.50), 0);
    }

    #[test]
    fn split_even_odd_splits_by_acquisition_order_parity() {
        let (even, odd) = split_even_odd(&[1u128, 2, 3, 4, 5]);
        assert_eq!(even, vec![1, 3, 5]);
        assert_eq!(odd, vec![2, 4]);
    }

    #[test]
    fn abba_schedule_cycles_foreign_held_absent_absent_foreign_held() {
        let schedule = abba_schedule(2);
        assert_eq!(
            schedule,
            vec![Arm::ForeignHeld, Arm::Absent, Arm::Absent, Arm::ForeignHeld]
        );
        // 各腕ちょうど count 件ずつ。
        assert_eq!(
            schedule.iter().filter(|a| **a == Arm::ForeignHeld).count(),
            2
        );
        assert_eq!(schedule.iter().filter(|a| **a == Arm::Absent).count(), 2);
    }

    #[test]
    fn abba_schedule_handles_uneven_tail_when_one_arm_is_exhausted() {
        // count=1: 最初の 2 スロットで両腕とも埋まり、以降の block 要素は
        // 「既に埋まった腕」を指すためスキップされ、schedule 長は 2 のまま。
        let schedule = abba_schedule(1);
        assert_eq!(schedule, vec![Arm::ForeignHeld, Arm::Absent]);
    }

    fn identical_samples(n: usize, value: u128) -> Vec<u128> {
        vec![value; n]
    }

    /// ケース 1: 同一分布 → `Indistinguishable`。
    #[test]
    fn judge_reports_indistinguishable_for_identical_distributions() {
        let b = identical_samples(200, 1000);
        let c = identical_samples(200, 1000);
        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Indistinguishable);
    }

    /// ケース 2: 両帯を超える差（例: (b) を 1.3 倍）→ `Distinguishable`。
    #[test]
    fn judge_reports_distinguishable_when_one_arm_is_scaled_up_beyond_both_bands() {
        let b = identical_samples(200, 1300);
        let c = identical_samples(200, 1000);
        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Distinguishable);
    }

    /// ケース 3: 固定帯を超えるが A/A 帯内 → `Indistinguishable`（ノイズ帯内。
    /// A/A 帯自体を固定帯より広くとることで「固定帯超過だが環境ノイズの
    /// 範囲内」の状況を作る）。
    #[test]
    fn judge_reports_indistinguishable_when_delta_exceeds_fixed_band_but_within_aa_band() {
        // b: 偶数番目 920・奇数番目 1080（A/A 帯 ≈0.148 が固定帯 0.05 を
        // 上回るように広げる。b 全体の中央値は 1080 となり c の 1000 との
        // 差 0.08 は固定帯 0.05 を超えるが A/A 帯 0.148 には収まる）。
        let b: Vec<u128> = (0..200)
            .map(|i| if i % 2 == 0 { 920 } else { 1080 })
            .collect();
        let c = identical_samples(200, 1000);
        // delta_median = |median(b)/median(c) - 1| は固定帯 0.05 を超えるが、
        // b の A/A 帯（偶奇差）がそれを上回るよう仕組んであるため
        // `max(fixed, aa)` の比較で吸収される。
        let verdict = judge(&b, &c, &Bands::default());
        assert_eq!(verdict, Verdict::Indistinguishable);
    }

    /// ケース 4: A/A 帯が上限超過 → `Inconclusive`。
    #[test]
    fn judge_reports_inconclusive_when_aa_band_exceeds_the_upper_cap() {
        // 偶奇差が極端（100 と 10000）で A/A 帯の上限（0.25／0.50）を
        // 大きく超える腕を作る。
        let b: Vec<u128> = (0..200)
            .map(|i| if i % 2 == 0 { 100 } else { 10_000 })
            .collect();
        let c = identical_samples(200, 1000);
        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Inconclusive);
    }

    /// `Inconclusive` は `Distinguishable` より優先される（両方の条件を
    /// 同時に満たしうる入力でも、環境ノイズが判定不能な大きさである以上、
    /// 「区別できた」とは主張しない）。
    #[test]
    fn judge_prioritizes_inconclusive_over_distinguishable() {
        let b: Vec<u128> = (0..200)
            .map(|i| if i % 2 == 0 { 100 } else { 10_000 })
            .collect();
        let c = identical_samples(200, 1000);
        // b の中央値は (100+10000)/2 付近で c の 1000 と大きく異なりうるが、
        // それでも Inconclusive が優先される。
        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Inconclusive);
    }

    #[test]
    fn relative_diff_handles_zero_denominator() {
        assert_eq!(relative_diff(0, 0), 0.0);
        assert_eq!(relative_diff(1, 0), 1.0);
        assert_eq!(relative_diff(0, 1), 1.0);
    }

    /// Cursor Bugbot 指摘（Issue #738）: 旧実装 `|a/b - 1|` は分母が第 2
    /// 引数に固定される非対称形で、引数の順序を入れ替えると異なる値を
    /// 返しうるバグだった。新実装（`|a-b| / max(a,b)`）は引数の順序に
    /// 依存しないことを固定する。
    #[test]
    fn relative_diff_is_symmetric_regardless_of_argument_order() {
        assert_eq!(relative_diff(1000, 1300), relative_diff(1300, 1000));
        assert_eq!(relative_diff(500, 2000), relative_diff(2000, 500));
        assert_eq!(relative_diff(7, 7), 0.0);
    }

    /// codex-review 指摘（Issue #738）: median・p95 の 2 点比較だけでは、
    /// 200 件中 80 件だけが他方と大きく異なる二峰性分布（存在情報が下位
    /// 分位点にのみ現れるケース）を median・p95 が偶然一致することで
    /// 見逃してしまう反例。分位点ラダー全体を走査する新 `judge` は
    /// この反例を `Distinguishable` として検出できることを固定する。
    #[test]
    fn judge_detects_bimodal_subpopulation_hidden_from_median_and_p95() {
        // b: 前半 80 件が 500（他方に存在しない値域）・後半 120 件が 1000。
        // median（idx=round(199*0.5)=100 → 1000 側）・p95（idx=round(199*0.95)
        // =189 → 1000 側）はいずれも c の 1000 と一致するが、下位分位点
        // （p10・p25）には 500 側の値が現れる。
        let mut b: Vec<u128> = Vec::with_capacity(200);
        b.extend(std::iter::repeat_n(500u128, 80));
        b.extend(std::iter::repeat_n(1000u128, 120));
        let c = identical_samples(200, 1000);

        // 旧実装（median・p95 のみ）ならここは見逃していたはずの反例。
        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Distinguishable);
    }

    /// 固定オフセット（4%）は固定帯（5%）以内で pass する既定動作の確認
    /// （codex-review コメントで挙げられた例。バグではなく許容差の意図
    /// どおりの挙動であることを固定する）。
    #[test]
    fn judge_reports_indistinguishable_for_small_constant_offset_within_fixed_band() {
        let b = identical_samples(200, 1040);
        let c = identical_samples(200, 1000);
        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Indistinguishable);
    }

    /// codex-review 追加指摘（Issue #738）: 8 点ラダー（10% 刻み中心）は
    /// 固定分位点「の間」だけに現れる部分母集団を見逃す反例。
    ///
    /// b は取得順で 1000×60 件・2000×38 件・3000×102 件（計 200 件）の
    /// ブロック、c は同じ並びで中央ブロックだけ 2500 に置き換えたもの
    /// （1000×60・2500×38・3000×102）。旧 8 点ラダー（0/10/25/50/75/90/95/99%）
    /// はいずれも累積比率 0%・30.15%（境界と一致するがブロック先頭側の
    /// 1000 を指す）〜48.74% の中央ブロック区間を跨いで通過してしまい、
    /// median（idx=100 → 3000）・p95（idx=189 → 3000）を含むどの固定点でも
    /// b・c が一致してしまうため `Indistinguishable` を誤って返す
    /// （このブロックは全体の 19% を占め、2000 対 2500 は相対差 20% で
    /// 実際には区別可能）。現在の `judge` は固定ラダーではなく全順序統計量
    /// 走査（`0..rounds` の全インデックス）で判定するため、この反例も
    /// 60〜97 件目の各インデックスで直接捕捉できることを固定する
    /// （固定ラダー自体は診断表示専用として残置しているため、この反例で
    /// 旧 8 点ラダーが見逃す事実自体は変わらず、以下でそれも確認する）。
    #[test]
    fn judge_detects_gap_between_quantile_ladder_points() {
        let mut b: Vec<u128> = Vec::with_capacity(200);
        b.extend(std::iter::repeat_n(1000u128, 60));
        b.extend(std::iter::repeat_n(2000u128, 38));
        b.extend(std::iter::repeat_n(3000u128, 102));

        let mut c: Vec<u128> = Vec::with_capacity(200);
        c.extend(std::iter::repeat_n(1000u128, 60));
        c.extend(std::iter::repeat_n(2500u128, 38));
        c.extend(std::iter::repeat_n(3000u128, 102));

        // 反例の前提: 旧 8 点ラダーではすべての固定点で b・c が一致する
        // （このアサーション自体が「なぜ旧ラダーが見逃すか」の根拠）。
        const OLD_LADDER: &[(f64, BandKind)] = &[
            (0.00, BandKind::Median),
            (0.10, BandKind::Median),
            (0.25, BandKind::Median),
            (0.50, BandKind::Median),
            (0.75, BandKind::Median),
            (0.90, BandKind::Tail),
            (0.95, BandKind::Tail),
            (0.99, BandKind::Tail),
        ];
        let mut b_sorted = b.clone();
        let mut c_sorted = c.clone();
        b_sorted.sort_unstable();
        c_sorted.sort_unstable();
        for &(p, _) in OLD_LADDER {
            assert_eq!(
                percentile(&b_sorted, p),
                percentile(&c_sorted, p),
                "old 8-point ladder must miss this counterexample at p={p}"
            );
        }

        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Distinguishable);
    }

    /// codex-review 追加指摘（PR #797・Issue #738）: 5% 刻みへ狭めた
    /// ラダーでも、両腕の分位点が偶然一致する位置の**間**にだけ現れる
    /// より狭い部分母集団（全体の 3%＝6/200 件）は依然として見逃しうる
    /// 反例。b=[1000×62,2000×6,3000×132]・c=[1000×62,2500×6,3000×132]
    /// （差分ブロックは 62〜67 件目）は、p30（idx=round(199*0.30)=60）・
    /// p35（idx=round(199*0.35)=70）のいずれも 60〜67 件目の 1000/2000 側
    /// （b）・1000/2500 側（c）ではなくブロック境界を跨いだ位置を指すため、
    /// 固定ラダーの刻み幅をどれだけ狭めても同型の反例が原理的に構築できる
    /// ことを示す（刻み幅より部分母集団の幅を狭く取ればよいだけのため）。
    /// 全順序統計量走査に置き換えた `judge` はこの反例も 62〜67 件目の
    /// 各インデックスで直接捕捉できることを固定する。
    #[test]
    fn judge_detects_subpopulation_narrower_than_any_fixed_quantile_ladder_step() {
        let mut b: Vec<u128> = Vec::with_capacity(200);
        b.extend(std::iter::repeat_n(1000u128, 62));
        b.extend(std::iter::repeat_n(2000u128, 6));
        b.extend(std::iter::repeat_n(3000u128, 132));

        let mut c: Vec<u128> = Vec::with_capacity(200);
        c.extend(std::iter::repeat_n(1000u128, 62));
        c.extend(std::iter::repeat_n(2500u128, 6));
        c.extend(std::iter::repeat_n(3000u128, 132));

        // 反例の前提: 現行の 5% 刻みラダー（診断表示専用。`QUANTILE_LADDER`）
        // でもすべての固定点で b・c が一致する（このアサーション自体が
        // 「なぜ固定ラダー方式そのものが原理的に不十分か」の根拠）。
        let mut b_sorted = b.clone();
        let mut c_sorted = c.clone();
        b_sorted.sort_unstable();
        c_sorted.sort_unstable();
        for &(p, _) in QUANTILE_LADDER {
            assert_eq!(
                percentile(&b_sorted, p),
                percentile(&c_sorted, p),
                "5%-step ladder must still miss this narrower counterexample at p={p}"
            );
        }

        assert_eq!(judge(&b, &c, &Bands::default()), Verdict::Distinguishable);
    }
}
