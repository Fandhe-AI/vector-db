//! 台帳エントリの内容照合ハッシュ（TASK-101、対象ビヘイビア: RECOVER-10。ポインタ:
//! `docs/spec/05-tasks.md` TASK-101・`docs/spec/04-behavior/recovery.md` RECOVER-10）。
//!
//! `recovery::ledger`（TASK-93・RECOVER-2）の台帳は複合キー（テナント・テーブル・
//! `operation_id`）のみを保持し、書き込み内容そのものは照合できない。過去に commit
//! 済みの `operation_id` が**内容の異なる**書き込みへ誤って再利用された場合、
//! 再送ベースの回復確認（RECOVER-7 系）が誤判定する余地がある。本モジュールは
//! クライアント要求由来の内容のみから決定的な [`ContentHash`] を構成し、
//! `ledger::record_in_txn` が「同一 `operation_id`・同一内容の再送（`23505`）」と
//! 「同一 `operation_id`・内容不一致の誤用（`22023`）」を区別できるようにする。
//!
//! 依存追加なし: 本タスクは自動運転で実行されユーザー承認を得られないため、
//! 依存追加（`.claude/rules/dependency-policy.md`）を避ける安全側の判断として
//! SHA-256 をこのモジュール内で安全 Rust により自作実装する（`unsafe` 不使用）。
//! FIPS 180-4 の公開テストベクタで正当性を機械検証する（下部 `tests` モジュール）。
//!
//! ## 正規化（canonical 化）の方針
//!
//! ハッシュ入力は**クライアント要求由来の内容のみ**から構成し、DB の現在状態
//! （既存行の有無・削除対象集合等）に依存する値を含めない。これにより、同一の
//! クライアント要求が再送された場合は常に同一ハッシュへ決定的に写像される
//! （再送検知の前提条件）。
//!
//! 連結曖昧性（例: `"ab"+"c"` と `"a"+"bc"` が同一バイト列になる事故）を構造的に
//! 排除するため、[`HashInputBuilder`] はすべての可変長フィールドへ長さプレフィクス
//! （4 バイト LE）を付けてから連結する。先頭にドメイン分離タグ（固定文字列）＋
//! 操作種別タグ（1 バイト）を置き、他コンテキストでの SHA-256 利用や操作種別間の
//! 衝突を避ける。
//!
//! 呼び出し元は `crate::tenant::*_unchecked`（6 箇所。TASK-93 の台帳追記と同一の
//! write トランザクション内でハッシュ計算済みの値を渡す設計）。各操作種別の入力
//! レイアウトは対応する `for_*` 関数のコメントを参照。
//!
//! `encode_row` を経由する操作種別（挿入・バッチ挿入・更新）は、呼び出し元が
//! **1 回だけ** `encode_row` した結果（`&[u8]`）を受け取る `for_*_encoded` 系を
//! production から呼ぶ（Issue #397。encoded バイト列は台帳ハッシュと redb 書き込みの
//! 双方で共有され、以前存在した「ハッシュ計算用と書き込み用でそれぞれ 1 回ずつ、
//! 合計 2 回 `encode_row` する」二重実行を排除する）。`RowInput` を受けて内部で
//! `encode_row` する旧形（`for_insert`／`for_insert_batch`／`for_update`）は
//! `#[cfg(test)]` の参照実装として残し、`for_*_encoded` との等価性テストにのみ使う。

use crate::row_codec::Value;
#[cfg(test)]
use crate::storage::{encode_row, RowInput};
use crate::storage::{StorageError, Visibility};

/// SHA-256 ダイジェスト（32 バイト）を保持する台帳内容ハッシュ。中身の生バイト列は
/// [`ContentHash::as_bytes`] 経由でのみ参照する（`recovery::ledger` の台帳値
/// エンコード・照合専用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentHash([u8; 32]);

impl ContentHash {
    /// 生の 32 バイトダイジェストへの参照。台帳エントリのエンコード（`ledger.rs`）と
    /// 既存エントリとの照合にのみ使う。
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 保存済みダイジェスト（`ledger.rs` がデコードした生バイト列）と一致するかを判定する。
    pub(crate) fn matches(&self, stored: &[u8; 32]) -> bool {
        &self.0 == stored
    }

    /// `ledger.rs` の単体テスト専用: 任意バイト列から直接構成する（`RowInput`/
    /// `TableSchema` を用意せずに「内容の違いで異なるハッシュになる」ことだけを
    /// 検証したいケース向け）。本番経路（`for_insert` 等）は使わない。
    #[cfg(test)]
    pub(crate) fn for_test(seed: &[u8]) -> Self {
        ContentHash(sha256(seed))
    }
}

/// ドメイン分離タグ。他コンテキストでの SHA-256 利用との衝突を避ける固定文字列
/// （バージョン番号を含み、将来レイアウトを変更する場合は新タグへ切り替える）。
const DOMAIN_TAG: &[u8] = b"vector-db/op_ledger/content_hash/v1";

/// 操作種別タグ（1 バイト）。同一フィールド列でも操作種別が異なれば必ず異なる
/// ハッシュになるよう、[`HashInputBuilder::new`] が先頭に埋め込む。
#[repr(u8)]
#[derive(Clone, Copy)]
enum OpTag {
    Insert = 1,
    InsertBatch = 2,
    Update = 3,
    Delete = 4,
    ReplaceByTextKey = 5,
}

/// 長さプレフィクス付きフィールド連結でハッシュ入力を組み立てるビルダー
/// （本モジュールドキュメントの「正規化の方針」参照）。
///
/// Issue #399: 内部を `Vec<u8>`（バッチ全体を一度連結し、`finish` でさらに
/// パディング用に全体を複製する二重コピー構造）から [`Sha256`] のストリーミング
/// 更新へ置換した。各 `push_*` は `Sha256::update` を直接呼ぶため、バッチ全体
/// （数百 KB 規模）を保持する中間バッファは存在しない。出力（ダイジェスト）は
/// 旧実装（`#[cfg(test)] sha256_reference`）と等価であることをテストで機械検証する。
struct HashInputBuilder(Sha256);

impl HashInputBuilder {
    fn new(tag: OpTag) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN_TAG);
        hasher.update(&[tag as u8]);
        HashInputBuilder(hasher)
    }

    /// 可変長バイト列を「4 バイト LE 長さ＋本体」で連結する。untrusted 入力由来の
    /// 長さを上限検証してからアロケーションに使う契約（coding-rust.md）に従い、
    /// `u32` へ収まらない場合は `Err` で拒否する（`encode_row` 等の既存エンコーダが
    /// 既に検証済みの値のみが渡る想定だが、本関数単体でも fail-closed を保つ）。
    fn push_bytes(&mut self, field: &[u8]) -> Result<(), StorageError> {
        let len = u32::try_from(field.len())
            .map_err(|_| StorageError::Codec("content hash field too large".to_string()))?;
        self.0.update(&len.to_le_bytes());
        self.0.update(field);
        Ok(())
    }

    fn push_u64(&mut self, v: u64) {
        self.0.update(&v.to_le_bytes());
    }

    fn push_u8(&mut self, v: u8) {
        self.0.update(&[v]);
    }

    /// 長さプレフィクスなしで生バイト列をそのまま流し込む（バッチ件数プレフィクス
    /// 等、[`push_bytes`] の長さプレフィクス契約に合わない箇所専用の内部 API）。
    fn push_raw(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(self) -> ContentHash {
        ContentHash(self.0.finalize())
    }
}

/// `row_codec::Value` 1 個をタグ＋長さプレフィクス付きで連結する。スキーマに
/// 依存せず（`Value` の判別子のみで境界を確定できる）、ハッシュ対象を DB 状態
/// （カタログのスキーマ定義）から独立させる。
fn push_value(b: &mut HashInputBuilder, v: &Value) -> Result<(), StorageError> {
    match v {
        Value::Null => b.push_u8(0),
        Value::Text(text) => {
            b.push_u8(1);
            b.push_bytes(text.as_bytes())?;
        }
        Value::Vector(vector) => {
            b.push_u8(2);
            push_vector(b, vector)?;
        }
    }
    Ok(())
}

/// `f32` ベクトル 1 個を長さプレフィクス付きで連結する（[`push_value`] の
/// `Value::Vector` 分岐と、埋め込みだけを単独で受け取る [`for_typed_insert`] の
/// 双方から使う共通実装）。
fn push_vector(b: &mut HashInputBuilder, vector: &[f32]) -> Result<(), StorageError> {
    let len = u32::try_from(vector.len())
        .map_err(|_| StorageError::Codec("content hash vector too large".to_string()))?;
    b.push_raw(&len.to_le_bytes());
    // f32 を 1 要素ずつ update するとバッチ規模のベクトルで呼び出し回数が
    // 膨らむため、固定長スタックバッファ（64 要素分）へ詰めてからまとめて
    // update する（Issue #399。ヒープ確保は増やさない）。
    let mut scratch = [0u8; 256];
    let mut filled = 0usize;
    for f in vector {
        let bytes = f.to_le_bytes();
        if filled + 4 > scratch.len() {
            b.push_raw(&scratch[..filled]);
            filled = 0;
        }
        scratch[filled..filled + 4].copy_from_slice(&bytes);
        filled += 4;
    }
    if filled > 0 {
        b.push_raw(&scratch[..filled]);
    }
    Ok(())
}

/// スキーマの非 `VECTOR` 列（列名, 値）ペア列から、`Value::Null` の列を除外して
/// 列名＋値の順に連結する（TASK-101・RECOVER-10 追加修正。cursor bugbot 指摘・
/// PR #248: `sql::parser::bind_insert`/`bind_file_insert` は列値配列を
/// `schema.columns.len()` 幅・位置インデックス基準で構築するため、間に
/// `ALTER TABLE ADD COLUMN`（常に nullable。`catalog::alter_table_add_column`
/// 参照）が挟まると同一クライアント要求の再送でも配列幅・各列の位置がずれ、
/// 位置ベースでハッシュ化すると内容一致のはずの再送が不一致
/// （`OperationIdContentMismatch`・`22023`）に誤判定されてしまう）。
///
/// 列の**位置ではなく名前**で連結することで、新規追加列（クライアントの元の
/// 文には現れない → 束縛時は常に `Value::Null` で埋まる）を素通しで除外し、
/// 既存列だけの並びをスキーマ変更前後で不変に保つ。クライアントが既存の
/// nullable 列へ明示的に `NULL` を送った場合と、その列がまだスキーマに存在
/// しなかった場合とで同一ハッシュになるが、いずれも永続化される行の内容
/// （`row_codec::encode_scalar_columns` が書く当該列のプレゼンスバイト）は
/// 同一であり、内容一致判定の観点で区別する必要はない。
fn push_named_scalar_columns(
    b: &mut HashInputBuilder,
    columns: &[(&str, &Value)],
) -> Result<(), StorageError> {
    for (name, value) in columns {
        if matches!(value, Value::Null) {
            continue;
        }
        b.push_bytes(name.as_bytes())?;
        push_value(b, value)?;
    }
    Ok(())
}

/// `insert_row_unchecked` 用（TASK-101 対象経路 1）。入力: `(id, encoded_row)`。
/// `encoded_row` は [`crate::storage::encode_row`] の出力（`tenant_id`・`visibility`・
/// `embedding`・`metadata` を含む正準表現）を**呼び出し元が 1 回だけ計算した結果**
/// （Issue #397。以前は本関数が内部で `encode_row` し、呼び出し元が redb 書き込み用に
/// 同じ行をもう一度 `encode_row` していた二重実行を、呼び出し元が事前エンコードした
/// 結果をここと redb 書き込みの双方で共有する形に変更した。ハッシュ入力バイト列の
/// レイアウトは変更前と完全一致する）。
pub(crate) fn for_insert_encoded(id: u64, encoded_row: &[u8]) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Insert);
    b.push_u64(id);
    b.push_bytes(encoded_row)?;
    Ok(b.finish())
}

/// [`for_insert_encoded`] の参照実装（`RowInput` から内部で `encode_row` する旧形。
/// production からは呼ばれず、`for_insert_encoded` との等価性テストのみに使うため
/// `#[cfg(test)]`。Issue #353 が `sql/udf_call::eval` を同様の位置づけで残置した
/// 運用に倣う）。
#[cfg(test)]
pub(crate) fn for_insert(id: u64, row: &RowInput<'_>) -> Result<ContentHash, StorageError> {
    let encoded = encode_row(row)?;
    for_insert_encoded(id, &encoded)
}

/// `insert_rows_unchecked` 用（TASK-101 対象経路 2）。バッチ全体で 1 ハッシュ。
/// 入力: 要求記載順の `(id, encoded_row)` 列（順序も入力に含める。並び替えた同一集合の
/// 再送を意図的に区別する設計）。`encoded_row` は呼び出し元が事前に 1 回だけ
/// `encode_row` した結果（[`for_insert_encoded`] と同じ理由。Issue #397）。
pub(crate) fn for_insert_batch_encoded(rows: &[(u64, &[u8])]) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilder::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, encoded_row) in rows {
        b.push_u64(*id);
        b.push_bytes(encoded_row)?;
    }
    Ok(b.finish())
}

/// [`for_insert_batch_encoded`] の参照実装（`#[cfg(test)]`。理由は [`for_insert`] 参照）。
#[cfg(test)]
pub(crate) fn for_insert_batch(rows: &[(u64, RowInput<'_>)]) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilder::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, row) in rows {
        let encoded = encode_row(row)?;
        b.push_u64(*id);
        b.push_bytes(&encoded)?;
    }
    Ok(b.finish())
}

/// `insert_typed_row_unchecked` 用（TASK-101 対象経路 3）。入力: `id`・`visibility`・
/// `VECTOR` 列の埋め込み・非 `VECTOR` 列の（列名, 値）ペア列（`Value::Null` は
/// [`push_named_scalar_columns`] が除外する）。操作種別タグは [`OpTag::Insert`] を
/// 共有する（行形 `INSERT` と宣言的 `INSERT` は同じ「新規挿入」操作であり、経路の
/// 違いでハッシュ空間を分ける必要がない）。
///
/// [`for_insert`] と異なり `storage::encode_row`（`schema.columns.len()` 幅の
/// `row_codec::encode_scalar_columns` 出力を `metadata` に含む正準表現）を経由
/// **しない**（cursor bugbot 指摘・PR #248。[`push_named_scalar_columns`]
/// ドキュメント参照）。呼び出し元 `tenant::insert_typed_row_unchecked` は
/// `values`（`sql::parser::bind_insert` が現在のスキーマ幅で構築した配列）から
/// 列名付きペアを組み立てて渡す。
///
/// `visibility` はハッシュ入力に明示的に含める（codex-review P1・cursor bugbot
/// 指摘・PR #248: `insert_typed_row_unchecked` が実際に永続化する行には
/// `visibility` が含まれるが、旧実装では `id`・embedding・非 `VECTOR` 列しか
/// ハッシュへ渡していなかった。そのため同一 `(tenant, table, operation_id)` で
/// 内容は同一のまま `visibility` だけを変えた再送が、内容不一致の `22023` では
/// なく内容一致の `23505` と誤判定されてしまう。`for_replace_by_text_key` と
/// 同様に [`crate::storage::Visibility::to_byte`] で 1 バイトへ写像して連結する）。
pub(crate) fn for_typed_insert(
    id: u64,
    visibility: Visibility,
    embedding: &[f32],
    columns: &[(&str, &Value)],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Insert);
    b.push_u64(id);
    b.push_u8(visibility.to_byte());
    push_vector(&mut b, embedding)?;
    push_named_scalar_columns(&mut b, columns)?;
    Ok(b.finish())
}

/// `update_row_unchecked` 用（TASK-101 対象経路 4）。入力: `(id, encoded_row)`。
/// `encoded_row` は呼び出し元が事前に 1 回だけ `encode_row` した結果
/// （[`for_insert_encoded`] と同じ理由。Issue #397）。
pub(crate) fn for_update_encoded(id: u64, encoded_row: &[u8]) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Update);
    b.push_u64(id);
    b.push_bytes(encoded_row)?;
    Ok(b.finish())
}

/// [`for_update_encoded`] の参照実装（`#[cfg(test)]`。理由は [`for_insert`] 参照）。
#[cfg(test)]
pub(crate) fn for_update(id: u64, row: &RowInput<'_>) -> Result<ContentHash, StorageError> {
    let encoded = encode_row(row)?;
    for_update_encoded(id, &encoded)
}

/// `delete_row_unchecked` 用（TASK-101 対象経路 5）。入力: `id` のみ（削除要求は
/// 対象 id 以外にクライアント由来の内容を持たない）。
pub(crate) fn for_delete(id: u64) -> ContentHash {
    let mut b = HashInputBuilder::new(OpTag::Delete);
    b.push_u64(id);
    b.finish()
}

/// `replace_typed_rows_by_text_key`（ファイル形 `INSERT` の置換経路）用（TASK-101
/// 対象経路 6）。入力: `(key_column, key_value, visibility, path, body,
/// template_columns)`。削除対象集合・採番される id 等の DB 状態由来の値に加え、
/// **チャンク化・埋め込み結果（`replace_typed_rows_by_text_key` へ渡る
/// 派生済み行データ）も含めない**（codex-review P1 指摘・PR #248。`chunking`
/// 設定や `Embedder` の応答は同一のクライアント要求に対しても実行時に変わり得る
/// ため、これらをハッシュへ含めると再送の内容一致判定が偽陰性
/// （`OperationIdContentMismatch` の誤検出）を起こす。ハッシュ入力は
/// クライアントが `INSERT` 文で実際に送った値
/// （`path`・`body`・その他の Text 列値＝`template_columns`。`path`/`body`/VECTOR
/// 列は `sql::parser::bind_file_insert` により `template_columns` 由来の
/// `template_values` 中で常に `Value::Null` に正規化済みのため、`path`/`body` は
/// 別引数として明示的に渡す）のみから決定的に構成する。本モジュールドキュメント
/// 「正規化の方針」参照。
///
/// `template_values`（`schema.columns.len()` 幅・位置インデックス基準の配列）を
/// 直接ハッシュしない（cursor bugbot 指摘・PR #248。[`push_named_scalar_columns`]
/// ドキュメント参照: `ALTER TABLE ADD COLUMN` を挟むと同一クライアント要求の
/// 再送でも配列幅・位置がずれる）。呼び出し元 `tenant::replace_typed_rows_by_text_key`
/// が現在のスキーマから（列名, 値）ペアへ変換して渡す。
pub(crate) fn for_replace_by_text_key(
    key_column: &str,
    key_value: &str,
    visibility: Visibility,
    path: &str,
    body: &str,
    template_columns: &[(&str, &Value)],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::ReplaceByTextKey);
    b.push_bytes(key_column.as_bytes())?;
    b.push_bytes(key_value.as_bytes())?;
    b.push_u8(visibility.to_byte());
    b.push_bytes(path.as_bytes())?;
    b.push_bytes(body.as_bytes())?;
    push_named_scalar_columns(&mut b, template_columns)?;
    Ok(b.finish())
}

// ---------------------------------------------------------------------------
// SHA-256（FIPS 180-4）自作実装。
//
// 依存追加が承認制のため（`.claude/rules/dependency-policy.md`）、本タスクの
// 自動運転下では既存の依存最小方針を維持する側に倒し、標準ライブラリのみで
// 実装する。`unsafe` は使わず、固定サイズ配列・`wrapping_*` 演算（FIPS 180-4 が
// 定める mod 2^32 加算そのもの。未定義動作にはならない）で構成する。
//
// Issue #399: バッチ全体（数百 KB 規模）を `Vec<u8>` へ一度連結してからパディング
// のためにさらに複製する旧実装（2 回の全量コピー）を、[`Sha256`] の
// ブロック単位ストリーミング更新へ再構成した。呼び出し側（[`HashInputBuilder`]）は
// 中間 `Vec` を持たず各フィールドを直接 `update` する。メッセージスケジュールも
// 64 語配列ではなく 16 語ローリング配列（`w[t & 15]`）にして固定サイズの境界
// チェックだけで済むようにした。出力（ダイジェスト）は下部 `sha256_reference`
// （旧実装をそのまま残した参照実装）と完全に等価であることを `tests` モジュールの
// FIPS ベクタ・境界長網羅・分割 `update` 等価性テストで機械検証する。
// ---------------------------------------------------------------------------

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 1 ブロック（64 バイト）ぶんの圧縮関数（FIPS 180-4 6.2.2 節）。メッセージ
/// スケジュールは 64 語配列ではなく 16 語のローリングバッファ（`w[t & 15]`）で
/// 保持する。`t >= 16` のラウンドでは、更新前の `w[t & 15]` が
/// （16 引くごとに同じスロットへ戻ってくるため）ちょうど `w[t - 16]` を保持して
/// いることを利用し、そのスロットへ新しい `w[t]` を上書きしてから同じラウンドの
/// 圧縮に使う（Issue #399）。
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
        // `as_chunks::<4>()` は固定長 4 バイト配列を返すため `from_be_bytes` は
        // 失敗しない。添字直接アクセスの代わりに `get_mut` で明示的に処理する
        // （coding-rust.md「untrusted 入力の扱い」と同じ規律を内部処理にも適用する）。
        if let Some(slot) = w.get_mut(i) {
            *slot = u32::from_be_bytes(*word);
        }
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *state;

    for t in 0..64usize {
        let idx = t & 15;
        if t >= 16 {
            let w15 = w[(t - 15) & 15];
            let w2 = w[(t - 2) & 15];
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            // 上書き前の w[idx] は w[t - 16]（ローリングバッファでは同一スロット
            // を 16 ラウンドごとに再利用する）。
            let prev16 = w[idx];
            w[idx] = prev16
                .wrapping_add(s0)
                .wrapping_add(w[(t - 7) & 15])
                .wrapping_add(s1);
        }

        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let k = K.get(t).copied().unwrap_or(0);
        let temp1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(k)
            .wrapping_add(w[idx]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(hh);
}

/// ストリーミング更新型の SHA-256 状態（Issue #399）。[`HashInputBuilder`] の
/// 各 `push_*` から `update` を直接呼ぶことで、旧実装が行っていた
/// 「バッチ全体を `Vec` へ連結 → パディングのため再度複製」という 2 回の
/// 全量コピーを排除する。固定長スタックバッファ（64 バイト）のみを使い、
/// 入力長に比例したヒープ確保は行わない。
struct Sha256 {
    state: [u32; 8],
    /// 64 バイト未満の未処理端数（`buffered` バイトぶんのみ有効）。
    buffer: [u8; 64],
    buffered: usize,
    /// 入力バイト総数。`finalize` でビット長（`wrapping_mul(8)`）へ変換する
    /// （既存の `pad()` と同じ契約。エンジン内部のハッシュ対象が実運用上
    /// `u64::MAX / 8` バイトへ到達することはない）。
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            state: H0,
            buffer: [0u8; 64],
            buffered: 0,
            total_len: 0,
        }
    }

    /// `total_len` を増やさずにバイト列をブロックバッファへ吸収する（`update` と
    /// `finalize` のパディング処理が共有する内部処理）。
    fn absorb(&mut self, mut data: &[u8]) {
        if self.buffered > 0 {
            let need = 64 - self.buffered;
            let take = need.min(data.len());
            if let Some(slot) = self.buffer.get_mut(self.buffered..self.buffered + take) {
                slot.copy_from_slice(&data[..take]);
            }
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                compress(&mut self.state, &block);
                self.buffered = 0;
            }
        }

        let (chunks, remainder) = data.as_chunks::<64>();
        for chunk in chunks {
            compress(&mut self.state, chunk);
        }

        if !remainder.is_empty() {
            if let Some(slot) = self.buffer.get_mut(..remainder.len()) {
                slot.copy_from_slice(remainder);
            }
            self.buffered = remainder.len();
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        self.absorb(data);
    }

    /// FIPS 180-4 5.1.1 節のパディング（`0x80` 1 バイト → 零埋め → 8 バイト BE
    /// ビット長）をブロックバッファ経由で適用してからダイジェストを取り出す。
    fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.absorb(&[0x80]);

        const ZEROS: [u8; 64] = [0u8; 64];
        let zero_pad = if self.buffered <= 56 {
            56 - self.buffered
        } else {
            56 + 64 - self.buffered
        };
        if let Some(zeros) = ZEROS.get(..zero_pad) {
            self.absorb(zeros);
        }
        self.absorb(&bit_len.to_be_bytes());

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            let bytes = word.to_be_bytes();
            let start = i * 4;
            if let Some(slot) = out.get_mut(start..start + 4) {
                slot.copy_from_slice(&bytes);
            }
        }
        out
    }
}

/// [`Sha256`] の参照実装（Issue #399 以前の一括処理版。バッチ全体を `Vec` へ
/// 連結してからパディングする旧実装をそのまま残す）。production からは
/// 呼ばれず、ストリーミング版との等価性テストにのみ使うため `#[cfg(test)]`。
#[cfg(test)]
fn sha256_reference(input: &[u8]) -> [u8; 32] {
    fn pad(input: &[u8]) -> Vec<u8> {
        let bit_len = (input.len() as u64).wrapping_mul(8);
        let mut msg = input.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0x00);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        msg
    }

    let padded = pad(input);
    let mut state = H0;
    for chunk in padded.as_chunks::<64>().0 {
        compress(&mut state, chunk);
    }

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        let bytes = word.to_be_bytes();
        let start = i * 4;
        if let Some(slot) = out.get_mut(start..start + 4) {
            slot.copy_from_slice(&bytes);
        }
    }
    out
}

/// 一括ハッシュのヘルパー（テスト専用。production は [`HashInputBuilder`] が
/// [`Sha256::update`] をフィールドごとに直接呼ぶため、この一括版は経由しない。
/// テストヘルパー [`ContentHash::for_test`] と `tests` モジュールの NIST/FIPS
/// 既知ダイジェスト検証・境界長網羅・分割 `update` 等価性テストで使う）。
#[cfg(test)]
fn sha256(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // FIPS 180-4 附属の公開テストベクタ（SHA-256("abc")）。
    #[test]
    fn sha256_matches_fips_test_vector_abc() {
        let digest = sha256(b"abc");
        assert_eq!(
            hex(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // 空文字列の既知ダイジェスト（NIST 公開値）。
    #[test]
    fn sha256_matches_known_digest_for_empty_input() {
        let digest = sha256(b"");
        assert_eq!(
            hex(&digest),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // FIPS 180-4 の複数ブロックにまたがるテストベクタ
    // （"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"）。
    #[test]
    fn sha256_matches_fips_test_vector_two_blocks() {
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        let digest = sha256(input);
        assert_eq!(
            hex(&digest),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    // Issue #399 追加: FIPS 180-4 附属の 896 bit（4 ブロックにまたがる）テストベクタ。
    #[test]
    fn sha256_matches_fips_test_vector_four_blocks() {
        let input = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        let digest = sha256(input);
        assert_eq!(
            hex(&digest),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    // Issue #399 追加: NIST 公開の 1,000,000 × 'a' 反復テストベクタ。ストリーミング
    // 版の分割 `update`（`absorb` のブロック境界処理）を長大入力で検証する。
    #[test]
    fn sha256_matches_nist_million_a_vector() {
        let input = vec![b'a'; 1_000_000];
        let digest = sha256(&input);
        assert_eq!(
            hex(&digest),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    // Issue #399: ストリーミング版と参照実装（一括処理版）が境界長 0..=200 バイト
    // で完全一致することを機械検証する（55/56/63/64/65/119/120 バイト等の
    // パディング分岐を網羅する）。決定的 LCG で生成した入力を使う。
    #[test]
    fn sha256_streaming_matches_reference_for_boundary_lengths() {
        let mut state: u64 = 0x2545F4914F6CDD1D;
        let mut next_byte = || {
            // xorshift* 相当の決定的 LCG（暗号強度は不要。境界長網羅の入力生成専用）。
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        };
        for len in 0..=200usize {
            let input: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            assert_eq!(
                sha256(&input),
                sha256_reference(&input),
                "mismatch at len={len}"
            );
        }
        for &len in &[4096usize, 65_537] {
            let input: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            assert_eq!(
                sha256(&input),
                sha256_reference(&input),
                "mismatch at len={len}"
            );
        }
    }

    // Issue #399: 同一入力を異なる粒度（1・3・63・64・65・100 バイト刻み）で
    // 分割 `update` した結果が、一括 `update` と一致することを検証する
    // （`Sha256::absorb` のバッファ境界処理のピン留め）。
    #[test]
    fn sha256_streaming_split_update_matches_one_shot_for_various_chunk_sizes() {
        let input: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let expected = sha256_reference(&input);

        for chunk_size in [1usize, 3, 63, 64, 65, 100] {
            let mut hasher = Sha256::new();
            for chunk in input.chunks(chunk_size) {
                hasher.update(chunk);
            }
            let digest = hasher.finalize();
            assert_eq!(digest, expected, "mismatch at chunk_size={chunk_size}");
        }
    }

    // 操作種別が違えば同じフィールド列でも異なるハッシュになる（連結曖昧性排除の
    // ピン留め）。
    #[test]
    fn different_op_tags_produce_different_hashes() {
        let mut insert_b = HashInputBuilder::new(OpTag::Insert);
        insert_b.push_u64(1);
        let insert_hash = insert_b.finish();

        let mut update_b = HashInputBuilder::new(OpTag::Update);
        update_b.push_u64(1);
        let update_hash = update_b.finish();

        assert_ne!(insert_hash, update_hash);
    }

    // 長さプレフィクスにより "ab"+"c" と "a"+"bc" が同一ハッシュにならない
    // （連結曖昧性排除のピン留め）。
    #[test]
    fn length_prefix_prevents_concatenation_ambiguity() {
        let mut b1 = HashInputBuilder::new(OpTag::Insert);
        b1.push_bytes(b"ab").expect("push");
        b1.push_bytes(b"c").expect("push");
        let h1 = b1.finish();

        let mut b2 = HashInputBuilder::new(OpTag::Insert);
        b2.push_bytes(b"a").expect("push");
        b2.push_bytes(b"bc").expect("push");
        let h2 = b2.finish();

        assert_ne!(h1, h2);
    }

    // 同一内容の再送は同一ハッシュ（決定性）。
    #[test]
    fn for_insert_is_deterministic_for_same_input() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0],
            metadata: b"meta",
        };
        let h1 = for_insert(7, &row).expect("hash");
        let h2 = for_insert(7, &row).expect("hash");
        assert_eq!(h1, h2);
    }

    // id の違いは異なるハッシュを生む。
    #[test]
    fn for_insert_differs_by_id() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0],
            metadata: b"meta",
        };
        let h1 = for_insert(7, &row).expect("hash");
        let h2 = for_insert(8, &row).expect("hash");
        assert_ne!(h1, h2);
    }

    // for_delete は id のみで決定的に定まる。
    #[test]
    fn for_delete_is_deterministic_and_differs_by_id() {
        assert_eq!(for_delete(1), for_delete(1));
        assert_ne!(for_delete(1), for_delete(2));
    }

    // for_replace_by_text_key はクライアント要求由来の body/template_values の
    // 違いを区別する。
    #[test]
    fn for_replace_by_text_key_differs_by_body() {
        let lang = Value::Text("en".to_string());
        let template: [(&str, &Value); 1] = [("lang", &lang)];
        let h1 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "body a",
            &template,
        )
        .expect("hash");
        let h2 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "body b",
            &template,
        )
        .expect("hash");
        assert_ne!(h1, h2);
    }

    // チャンク化・埋め込みの結果（行データそのもの）が変わっても、クライアント
    // 要求（path/body/template_values）が同一なら同一ハッシュ（P1 修正のピン留め:
    // codex-review 指摘・PR #248。`chunking` 設定や `Embedder` の応答差でハッシュが
    // 変わると、同一要求の再送が `OperationIdContentMismatch` に誤判定される）。
    #[test]
    fn for_replace_by_text_key_is_independent_of_chunking_and_embedding_output() {
        let lang = Value::Text("en".to_string());
        let template: [(&str, &Value); 1] = [("lang", &lang)];
        let h1 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &template,
        )
        .expect("hash");
        let h2 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &template,
        )
        .expect("hash");
        assert_eq!(h1, h2);
    }

    // cursor bugbot 指摘・PR #248 のピン留め: `ALTER TABLE ADD COLUMN`（常に
    // nullable）で新規列が追加されても、それに触れていないクライアント要求の
    // 再送ハッシュは不変（新規列は Value::Null として渡り、
    // push_named_scalar_columns が素通しで除外するため）。
    #[test]
    fn for_replace_by_text_key_is_stable_across_added_nullable_column() {
        let lang = Value::Text("en".to_string());
        // スキーマ変更前: `lang` 列のみ。
        let before: [(&str, &Value); 1] = [("lang", &lang)];
        let h_before = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &before,
        )
        .expect("hash");

        // スキーマ変更後（`ALTER TABLE ADD COLUMN note TEXT` 相当）: 同一クライアント
        // 要求の再送では `note` 列は未提供のため Value::Null で束縛される。
        let null_note = Value::Null;
        let after: [(&str, &Value); 2] = [("lang", &lang), ("note", &null_note)];
        let h_after = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &after,
        )
        .expect("hash");

        assert_eq!(h_before, h_after);
    }

    // for_typed_insert も同じ理由で、追加された nullable 列（未提供 → Value::Null）
    // の有無にハッシュが影響されない。
    #[test]
    fn for_typed_insert_is_stable_across_added_nullable_column() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let before: [(&str, &Value); 1] = [("title", &title)];
        let h_before = for_typed_insert(7, Visibility::Public, &embedding, &before).expect("hash");

        let null_note = Value::Null;
        let after: [(&str, &Value); 2] = [("title", &title), ("note", &null_note)];
        let h_after = for_typed_insert(7, Visibility::Public, &embedding, &after).expect("hash");

        assert_eq!(h_before, h_after);
    }

    // 一方、実際に異なる値が入れば当然ハッシュも変わる（区別できないほど鈍化
    // していないことの確認）。
    #[test]
    fn for_typed_insert_differs_by_column_value() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title_a = Value::Text("hello".to_string());
        let title_b = Value::Text("world".to_string());
        let cols_a: [(&str, &Value); 1] = [("title", &title_a)];
        let cols_b: [(&str, &Value); 1] = [("title", &title_b)];
        let h1 = for_typed_insert(7, Visibility::Public, &embedding, &cols_a).expect("hash");
        let h2 = for_typed_insert(7, Visibility::Public, &embedding, &cols_b).expect("hash");
        assert_ne!(h1, h2);
    }

    // codex-review P1・cursor bugbot 指摘（PR #248）の回帰固定: visibility のみが
    // 異なる再送は、内容一致（23505）ではなく内容不一致（22023）として区別できな
    // ければならない（RECOVER-10 の内容照合契約）。
    #[test]
    fn for_typed_insert_differs_by_visibility() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let cols: [(&str, &Value); 1] = [("title", &title)];
        let h_public = for_typed_insert(7, Visibility::Public, &embedding, &cols).expect("hash");
        let h_private = for_typed_insert(7, Visibility::Private, &embedding, &cols).expect("hash");
        assert_ne!(h_public, h_private);
    }

    // Issue #397 のピン留め: `for_insert_encoded` は「呼び出し元が事前エンコードした
    // 結果を渡す」新形、`for_insert` は「内部で `encode_row` する」旧形（参照実装）。
    // 同じ論理内容に対して常に同一ハッシュを返すことを確認し、事前エンコード共有化が
    // ハッシュ入力バイト列を変えていないことを機械検証する（既存台帳エントリとの
    // 互換の根拠）。
    #[test]
    fn for_insert_encoded_matches_reference_impl() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0, 4.0],
            metadata: b"meta",
        };
        let encoded = encode_row(&row).expect("encode");
        let h_new = for_insert_encoded(7, &encoded).expect("hash");
        let h_ref = for_insert(7, &row).expect("hash");
        assert_eq!(h_new, h_ref);
    }

    #[test]
    fn for_insert_batch_encoded_matches_reference_impl() {
        let row_a = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0],
            metadata: b"meta-a",
        };
        let row_b = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[4.0, 5.0, 6.0],
            metadata: b"",
        };
        let rows: [(u64, RowInput<'_>); 2] = [(1, row_a), (2, row_b)];
        let h_ref = for_insert_batch(&rows).expect("hash");

        let encoded_a = encode_row(&row_a).expect("encode");
        let encoded_b = encode_row(&row_b).expect("encode");
        let hash_input: [(u64, &[u8]); 2] = [(1, &encoded_a), (2, &encoded_b)];
        let h_new = for_insert_batch_encoded(&hash_input).expect("hash");

        assert_eq!(h_new, h_ref);
    }

    // 空バッチも一致する（境界値）。
    #[test]
    fn for_insert_batch_encoded_matches_reference_impl_for_empty_batch() {
        let empty_rows: [(u64, RowInput<'_>); 0] = [];
        let h_ref = for_insert_batch(&empty_rows).expect("hash");
        let empty_hash_input: [(u64, &[u8]); 0] = [];
        let h_new = for_insert_batch_encoded(&empty_hash_input).expect("hash");
        assert_eq!(h_new, h_ref);
    }

    #[test]
    fn for_update_encoded_matches_reference_impl() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[9.0, 8.0, 7.0],
            metadata: b"updated",
        };
        let encoded = encode_row(&row).expect("encode");
        let h_new = for_update_encoded(3, &encoded).expect("hash");
        let h_ref = for_update(3, &row).expect("hash");
        assert_eq!(h_new, h_ref);
    }

    // Issue #399 受け入れ 2: ストリーミング版（本番経路）と参照実装（旧・一括処理版。
    // バッチ全体を `Vec` へ連結してからパディングする）の処理時間を、
    // 台帳ハッシュ対象と同オーダー（1,000 行 × 約 0.5KB ≈ 500KB）の入力で
    // 手元比較するための手動専用テスト（CI 非配線・既定 ignore。
    // `docs/design/ingest-stage-profile.md`「Issue #399 追記」節の実測手順・
    // 前後比較表の根拠）。`cargo test --release -p engine --lib
    // recovery::content_hash::tests::sha256_streaming_vs_reference_manual_timing
    // -- --ignored --nocapture` で実行する。
    #[test]
    #[ignore = "手動計測専用（CI 非配線。--ignored --nocapture で明示実行する）"]
    fn sha256_streaming_vs_reference_manual_timing() {
        use std::time::Instant;

        let input: Vec<u8> = (0..500_000u32).map(|i| (i % 256) as u8).collect();
        let iterations = 200;

        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(sha256_reference(std::hint::black_box(&input)));
        }
        let reference_elapsed = start.elapsed();

        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(sha256(std::hint::black_box(&input)));
        }
        let streaming_elapsed = start.elapsed();

        println!(
            "sha256_reference: {reference_elapsed:?} ({:.3} ns/byte)",
            reference_elapsed.as_nanos() as f64 / (input.len() as f64 * iterations as f64)
        );
        println!(
            "sha256 (streaming): {streaming_elapsed:?} ({:.3} ns/byte)",
            streaming_elapsed.as_nanos() as f64 / (input.len() as f64 * iterations as f64)
        );
        assert_eq!(sha256_reference(&input), sha256(&input));
    }
}
