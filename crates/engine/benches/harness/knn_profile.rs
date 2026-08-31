//! KNN 経路の段別内訳プロファイル（Issue #362。段別内訳の記録先は
//! `docs/design/knn-stage-profile.md`。SQL 表層経由 KNN のレイテンシ内訳
//! （redb 走査・ヘッダデコード・f32 デコード・arena 構築・距離計算・Top-k 選出）を
//! 実測するためのハーネス。`knn_profile_bench.rs`（時間依存の実測入口）と
//! `tests/knn_profile_accept.rs`（時間非依存の回帰）の双方から `#[path]` で
//! 取り込まれる共有ソース（`harness/mod.rs` 冒頭コメントと同じ取り込み方針）。
//!
//! # 行バイト列レイアウトのベンチ内再実装（ドリフト対策）
//!
//! [`decode_row_reimpl`] は `crates/engine/src/storage.rs` の行フォーマット v2
//! （`[version u8][tenant_len u16 LE][tenant][visibility u8][dim u32 LE]
//! [f32 * dim LE][metadata_len u32 LE][metadata]`）を、`storage.rs` の内部 API
//! （`pub(crate)`）を使わずに再実装したものである。S2/S3 段（ヘッダデコード・f32
//! デコード）の内訳を、ベンチが独立コンパイル単位（`cargo bench` バイナリ）から
//! 直接測るための唯一の手段（本体クレートの `pub(crate)` API はベンチから呼べない。
//! `docs/design/knn-stage-profile.md`「前提調査の要点」節参照）。
//!
//! 再実装は将来 `storage.rs` 側のレイアウトが変わった場合に静かに誤測定へ陥る
//! （ドリフト）リスクを持つ。これを検出するため、`tests/knn_profile_accept.rs` が
//! 本関数の出力を `engine::storage::Storage::scan()`（pub API・正本のフル
//! デコード）の結果と突き合わせ、`make ci` の回帰対象にする（`.claude/rules/
//! coding-rust.md`「untrusted 入力の扱い」に準じ、長さフィールドは上限検証してから
//! アロケーションに使い、`checked_*` 演算のみを使う）。
//!
//! `std` のみに依存する（`harness` の他モジュールと同じ理由。`engine::` を
//! 参照しない）。

use std::fmt;
use std::time::Duration;

/// [`decode_row_reimpl`] が拒否する行フォーマットの上限（`storage.rs` の
/// `MAX_EMBEDDING_DIM`・`MAX_METADATA_LEN` と同種の防御を、ベンチ内デコード
/// 再実装側にも独立に持たせる。`tenant_len` はフィールド自体が `u16`（2 バイト）
/// のため型の上限＝値の上限で追加のガードは不要。値は本番の上限より十分大きく
/// 取り、正当な計測入力を拒否しないようにしつつ、破損バイト列からの無制限確保
/// だけを防ぐ）。
pub const MAX_REIMPL_DIM: u32 = 1_000_000;
pub const MAX_REIMPL_METADATA_LEN: u32 = 64 * 1024 * 1024;

/// 本モジュールのエラー型（`harness` 全体の `stats::BenchError` とは責務が異なる。
/// `sql_c1.rs::SqlC1Error` と同じ理由で独立させる）。
#[derive(Debug, Clone, PartialEq)]
pub enum KnnProfileError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された（`hybrid_latency.rs::
    /// refuse_under_github_actions` と同一パターン）。
    RefusedUnderGitHubActions,
    /// ベンチ内デコード再実装が行バイト列を解釈できなかった（破損・レイアウト
    /// ドリフトのいずれか）。
    Codec(String),
    /// 段別の所要時間が単調非減少であるべき箇所で減少していた（測定異常）。
    NonMonotonicStages {
        earlier: &'static str,
        later: &'static str,
    },
    /// 行数が 0 のため ns/行への換算ができない。
    ZeroRows,
}

impl fmt::Display for KnnProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KnnProfileError::RefusedUnderGitHubActions => write!(
                f,
                "knn_profile_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); this bench is manual-only and not wired into any workflow"
            ),
            KnnProfileError::Codec(msg) => write!(f, "row decode failed: {msg}"),
            KnnProfileError::NonMonotonicStages { earlier, later } => write!(
                f,
                "stage timings are not monotonically non-decreasing: {earlier} > {later}"
            ),
            KnnProfileError::ZeroRows => write!(f, "cannot compute ns/row for zero rows"),
        }
    }
}

impl std::error::Error for KnnProfileError {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`hybrid_latency_bench.rs`・
/// `sql_c1_bench.rs` と同じ defense-in-depth。本ベンチは `.github/workflows/*`
/// へ配線しないが、誤って CI 経由で実行された場合の安全弁として起動直後に呼ぶ）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), KnnProfileError> {
    if under_github_actions {
        return Err(KnnProfileError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// [`decode_row_reimpl`] の結果（ヘッダ＋embedding。metadata は本ベンチの
/// 段別分解では使わないため長さのみ保持し、バイト列は所有化しない）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReimplDecodedRow {
    pub tenant_id: String,
    /// `storage.rs::Visibility::PUBLIC_BYTE`（`0x01`）と一致するかどうか。値そのものは
    /// `storage.rs` 側が `pub(crate)` で非公開のため、本モジュールはバイト値 `1` を
    /// 「公開」の意味として独立に定義する（[`is_public_byte`]。ドリフト検出は
    /// アクセプトテストが `Storage::scan()` との突き合わせで行う）。
    pub is_public: bool,
    pub embedding: Vec<f32>,
    pub metadata_len: usize,
}

/// `storage.rs::Visibility::PUBLIC_BYTE` 相当の値（モジュール冒頭コメント参照）。
fn is_public_byte(byte: u8) -> bool {
    byte == 1
}

/// 行バイト列のヘッダ部（バージョン・`tenant_len`・`tenant_id`・`visibility`）だけを
/// デコードする（S2 段の再実装本体）。`storage.rs::decode_row_header` と同じ
/// フィールド検証順序・エラー条件を踏襲する。返り値はヘッダデコード後のオフセット。
///
/// 呼び出し文脈: [`decode_row_reimpl`]（S3 相当のフルデコード）の前段として、
/// `knn_profile_bench.rs` の S2 段が単独でも呼ぶ（S2 は embedding を読まないため、
/// この関数だけを計測ループへ渡す）。
pub fn decode_header_reimpl(buf: &[u8]) -> Result<(String, bool, usize), KnnProfileError> {
    let version = *buf
        .first()
        .ok_or_else(|| KnnProfileError::Codec("row buffer is empty".to_string()))?;
    if version != 2 {
        return Err(KnnProfileError::Codec(format!(
            "unsupported row format version: {version}"
        )));
    }
    let mut offset = 1usize;

    let tenant_len_end = offset
        .checked_add(2)
        .ok_or_else(|| KnnProfileError::Codec("offset overflow at tenant_len".to_string()))?;
    let tenant_len_bytes = buf
        .get(offset..tenant_len_end)
        .ok_or_else(|| KnnProfileError::Codec("truncated at tenant_len".to_string()))?;
    let tenant_len_arr: [u8; 2] = tenant_len_bytes
        .try_into()
        .map_err(|_| KnnProfileError::Codec("tenant_len is not 2 bytes".to_string()))?;
    let tenant_len = u16::from_le_bytes(tenant_len_arr);
    offset = tenant_len_end;

    let tenant_end = offset
        .checked_add(tenant_len as usize)
        .ok_or_else(|| KnnProfileError::Codec("offset overflow at tenant_id".to_string()))?;
    let tenant_bytes = buf
        .get(offset..tenant_end)
        .ok_or_else(|| KnnProfileError::Codec("truncated at tenant_id".to_string()))?;
    let tenant_id = std::str::from_utf8(tenant_bytes)
        .map_err(|_| KnnProfileError::Codec("tenant_id is not valid UTF-8".to_string()))?
        .to_string();
    offset = tenant_end;

    let visibility_byte = *buf
        .get(offset)
        .ok_or_else(|| KnnProfileError::Codec("truncated at visibility".to_string()))?;
    offset = offset
        .checked_add(1)
        .ok_or_else(|| KnnProfileError::Codec("offset overflow after visibility".to_string()))?;

    Ok((tenant_id, is_public_byte(visibility_byte), offset))
}

/// [`decode_header_reimpl`] に続けて `dim`・embedding（f32 LE 配列）・metadata 長を
/// デコードする（S3 段の再実装本体）。`out_embedding` は呼び出し元が複数行にわたり
/// 使い回すスクラッチ（`storage.rs::decode_row_embedding_and_metadata_into` と同じ
/// 「2 行目以降は再確保しない」設計を再実装側でも踏襲する）。
pub fn decode_row_reimpl(
    buf: &[u8],
    out_embedding: &mut Vec<f32>,
) -> Result<ReimplDecodedRow, KnnProfileError> {
    let (tenant_id, is_public, mut offset) = decode_header_reimpl(buf)?;

    let dim_end = offset
        .checked_add(4)
        .ok_or_else(|| KnnProfileError::Codec("offset overflow at dim".to_string()))?;
    let dim_bytes = buf
        .get(offset..dim_end)
        .ok_or_else(|| KnnProfileError::Codec("truncated at dim".to_string()))?;
    let dim_arr: [u8; 4] = dim_bytes
        .try_into()
        .map_err(|_| KnnProfileError::Codec("dim is not 4 bytes".to_string()))?;
    let dim = u32::from_le_bytes(dim_arr);
    if dim > MAX_REIMPL_DIM {
        return Err(KnnProfileError::Codec(format!(
            "dim {dim} exceeds reimpl limit {MAX_REIMPL_DIM}"
        )));
    }
    offset = dim_end;

    let embedding_bytes_len = (dim as usize)
        .checked_mul(4)
        .ok_or_else(|| KnnProfileError::Codec("embedding byte length overflow".to_string()))?;
    let embedding_end = offset
        .checked_add(embedding_bytes_len)
        .ok_or_else(|| KnnProfileError::Codec("offset overflow at embedding".to_string()))?;
    let embedding_bytes = buf
        .get(offset..embedding_end)
        .ok_or_else(|| KnnProfileError::Codec("truncated at embedding".to_string()))?;
    out_embedding.clear();
    out_embedding.reserve(dim as usize);
    for chunk in embedding_bytes.as_chunks::<4>().0 {
        out_embedding.push(f32::from_le_bytes(*chunk));
    }
    offset = embedding_end;

    let metadata_len_end = offset
        .checked_add(4)
        .ok_or_else(|| KnnProfileError::Codec("offset overflow at metadata_len".to_string()))?;
    let metadata_len_bytes = buf
        .get(offset..metadata_len_end)
        .ok_or_else(|| KnnProfileError::Codec("truncated at metadata_len".to_string()))?;
    let metadata_len_arr: [u8; 4] = metadata_len_bytes
        .try_into()
        .map_err(|_| KnnProfileError::Codec("metadata_len is not 4 bytes".to_string()))?;
    let metadata_len = u32::from_le_bytes(metadata_len_arr);
    if metadata_len > MAX_REIMPL_METADATA_LEN {
        return Err(KnnProfileError::Codec(format!(
            "metadata_len {metadata_len} exceeds reimpl limit {MAX_REIMPL_METADATA_LEN}"
        )));
    }

    Ok(ReimplDecodedRow {
        tenant_id,
        is_public,
        embedding: out_embedding.clone(),
        metadata_len: metadata_len as usize,
    })
}

/// 1 段あたりの ns/行換算（`total` を `rows` で割る。`rows == 0` は
/// [`KnnProfileError::ZeroRows`]）。
pub fn ns_per_row(total: Duration, rows: usize) -> Result<f64, KnnProfileError> {
    if rows == 0 {
        return Err(KnnProfileError::ZeroRows);
    }
    Ok(total.as_secs_f64() * 1e9 / rows as f64)
}

/// 2 段間の差分を ns/行へ換算する（`later >= earlier` を要求する。段別分解は
/// 「前段の処理を含んだ累積時間」を測る設計（S1 ⊆ S2 ⊆ S3 ⊆ S4）のため、
/// 逆転は測定異常として `Err` を返し呼び出し元に知らせる——黙って絶対値を
/// 取らない。fail-closed。`earlier_name`/`later_name` はエラーメッセージ用）。
pub fn stage_diff_ns_per_row(
    earlier: Duration,
    later: Duration,
    rows: usize,
    earlier_name: &'static str,
    later_name: &'static str,
) -> Result<f64, KnnProfileError> {
    let diff = later
        .checked_sub(earlier)
        .ok_or(KnnProfileError::NonMonotonicStages {
            earlier: earlier_name,
            later: later_name,
        })?;
    ns_per_row(diff, rows)
}

/// 1 段の実測結果を人間可読な 1 行へ整形する（stdout 出力用。本ベンチは spec 由来の
/// 閾値を持たない情報提供専用のため、実測値をそのまま出力してよい
/// （`.claude/rules/spec-confidentiality.md` のオーナー判断範囲。モジュール冒頭
/// コメント参照）。
pub fn render_stage_line(name: &str, rows: usize, median: Duration, ns_per_row: f64) -> String {
    format!(
        "stage({name}): rows={rows} median={:.3}ms ns_per_row={ns_per_row:.1}",
        median.as_secs_f64() * 1e3
    )
}

/// 段間差分（`stage_diff_ns_per_row`）の結果を 1 行へ整形する。
pub fn render_diff_line(from: &str, to: &str, diff_ns_per_row: f64) -> String {
    format!("diff({from}->{to}): ns_per_row={diff_ns_per_row:.1}")
}

/// S1〜S4 の走査行数がすべて一致することを検証する（整合性検証。行数が食い違う
/// 場合は「同じテーブル・同じスナップショットを走査できていない」ことを意味し、
/// 段別分解の前提が崩れているため fail-closed に `Err` を返す）。
pub fn assert_scan_row_counts_match(
    counts: &[(&'static str, usize)],
) -> Result<(), KnnProfileError> {
    let Some((_, first)) = counts.first() else {
        return Ok(());
    };
    for (name, count) in counts {
        if count != first {
            return Err(KnnProfileError::Codec(format!(
                "row count mismatch at stage {name}: expected {first}, got {count}"
            )));
        }
    }
    Ok(())
}
