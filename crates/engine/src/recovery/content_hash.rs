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
//! 長さプレフィクスだけでは**複数行を 1 ハッシュへ連結する**操作（[`for_typed_insert_batch`]）
//! の行境界までは一意に定まらない（ある行の列データが次の行の固定長フィールドへ
//! 「はみ出して」再解釈され得る。codex-review P1 指摘・PR #823）。1 ハッシュ = 1 行
//! （列データが常に入力全体の末尾）の操作（[`for_typed_insert`]・
//! [`for_replace_by_text_key`]）はこの曖昧性の対象外のため、対応は
//! [`for_typed_insert_batch`] のドキュメント（行ごとの列数プレフィクス）に限定する。
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
    Truncate = 6,
    /// `tenant::upsert_typed_rows_unchecked` 用（SQL-20・TASK-193、Issue #872）。
    /// [`for_typed_upsert`] ドキュメント参照。
    Upsert = 7,
    UpdateColumns = 8,
    /// 述語つき `UPDATE ... WHERE`（SQL-19・TASK-192、Issue #871・RECOVER-11）用。
    /// 単一行 `id` 完全一致形（[`OpTag::Update`]）とは別ドメインに分離する
    /// （ADR `docs/design/multi-row-dml-operation-id.md` §4.1）。[`for_update_where`]
    /// ドキュメント参照。
    UpdateWhere = 9,
    /// 述語つき `DELETE ... WHERE`（SQL-19・TASK-192、Issue #871・RECOVER-11）用。
    /// 単一行 `id` 完全一致形（[`OpTag::Delete`]）とは別ドメインに分離する
    /// （ADR 同上）。[`for_delete_where`] ドキュメント参照。
    DeleteWhere = 10,
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

/// [`HashInputBuilder`] の参照実装（Issue #399 以前の一括処理版。バッチ全体を
/// `Vec<u8>` へ都度 `extend_from_slice` で連結し、`finish` で [`sha256_reference`]
/// （パディングのため全体をもう一度複製する二重コピー構造）を 1 回呼ぶ）。
/// `#[cfg(test)]`。本番経路は経由しない（理由は [`for_insert`] 参照）。同じ
/// `push_bytes`／`push_u64` 呼び出しパターンを共有する [`for_insert_batch_encoded_reference`]
/// から、production の [`HashInputBuilder`]（ストリーミング版）と同一のフィールド
/// 境界（行ごとに `push_u64` 1 回＋ `push_bytes` 1 回）で A/B 計測できるようにする
/// （codex-review 指摘・PR #419: 単一の巨大バッファへの 1 回 `update` 呼び出しでは
/// 本番経路〔行・フィールド単位の多数回 `update`〕を再現しない）。
#[cfg(test)]
struct HashInputBuilderReference(Vec<u8>);

#[cfg(test)]
impl HashInputBuilderReference {
    fn new(tag: OpTag) -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(DOMAIN_TAG);
        buf.push(tag as u8);
        HashInputBuilderReference(buf)
    }

    fn push_bytes(&mut self, field: &[u8]) -> Result<(), StorageError> {
        let len = u32::try_from(field.len())
            .map_err(|_| StorageError::Codec("content hash field too large".to_string()))?;
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(field);
        Ok(())
    }

    fn push_u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn push_raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn finish(self) -> ContentHash {
        ContentHash(sha256_reference(&self.0))
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
        // タグ 3・4（Integer・BigInt）は Issue #881 の予約割り当て。TABLE-13・
        // Issue #882 計画 F9: Real=5・Double=6（正規化後の LE ビット列）。
        // BOOLEAN は TABLE-13 の宣言順で 7 とする（Issue #883。他の型と衝突しない
        // 新規タグ）。
        Value::Real(v) => {
            b.push_u8(5);
            b.push_raw(&v.to_le_bytes());
        }
        Value::Double(v) => {
            b.push_u8(6);
            b.push_raw(&v.to_le_bytes());
        }
        Value::Bool(b_val) => {
            b.push_u8(7);
            b.push_u8(u8::from(*b_val));
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

/// [`for_insert_batch_encoded`] と同一のフィールド境界（行ごとに `push_u64` 1 回＋
/// `push_bytes` 1 回）を、[`HashInputBuilderReference`]（Issue #399 以前の一括処理版）
/// で計算する A/B 計測専用の参照実装。`#[cfg(test)]`。`encoded_row` は呼び出し元が
/// 用意した `&[u8]` をそのまま受け取り、production の `for_insert_batch_encoded` と
/// 引数・呼び出しパターンを完全に揃える（codex-review 指摘・PR #419）。
#[cfg(test)]
fn for_insert_batch_encoded_reference(rows: &[(u64, &[u8])]) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilderReference::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, encoded_row) in rows {
        b.push_u64(*id);
        b.push_bytes(encoded_row)?;
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

/// [`for_typed_insert_batch`]・`tenant::insert_typed_rows_unchecked` が共有する
/// 1 行分のハッシュ材料（`(id, visibility, embedding, 非 VECTOR 列の列名付き
/// ペア列)`）。`clippy::type_complexity` 回避のための型エイリアス（意味は
/// タプルの並びそのもの。`tenant.rs` からも参照する）。
pub(crate) type TypedInsertBatchRow<'a> = (u64, Visibility, &'a [f32], &'a [(&'a str, &'a Value)]);

/// `insert_typed_rows_unchecked` 用（Issue #771・TASK-178・NOSQL-6。TASK-101・
/// RECOVER-10 の型付き挿入経路をバッチへ拡張したもの）。1 つの `operation_id` が
/// 要求記載順の複数行をまとめて覆う点は [`OpTag::InsertBatch`]（[`for_insert_batch_encoded`]
/// と共有するタグ。「新規挿入をバッチでまとめて行う」操作として同じ操作種別に
/// 属する）で表す。入力: 要求記載順の `(id, visibility, embedding, 非 VECTOR 列の
/// 列名付きペア列)`。各行のハッシュ材料は [`for_typed_insert`] と同一の組み立て
/// （`push_u64(id)` → `push_u8(visibility)` → [`push_vector`] → 列数プレフィクス →
/// [`push_named_scalar_columns`]）を行ごとに連結する（列名ベース・PR #248 の教訓を
/// 踏襲。`ALTER TABLE ADD COLUMN` を挟んだ再送でも列幅がずれない）。並び替えた
/// 同一集合の再送は意図的に区別する（[`for_insert_batch_encoded`] と同じ設計。
/// 件数プレフィクスに続けて行ごとのフィールドを連結する）。
///
/// 行境界の曖昧性回避（codex-review P1 指摘・PR #823）: [`push_named_scalar_columns`]
/// は 1 行分の列を「列数を明示しない、末尾まで連結するだけ」の形式で書く。
/// [`for_typed_insert`]／[`for_replace_by_text_key`] のように 1 ハッシュ = 1 行
/// （列データが常に入力全体の末尾）ならこれで一意に復元できるが、本関数は複数行を
/// 1 ハッシュへ連結するため、ある行の列データが**次の行**の `id`／`visibility`／
/// `embedding` 長さへ「はみ出して」再解釈されても、`push_bytes` の長さプレフィクス
/// 契約自体は内部的に自己無矛盾なまま別の行分割として復元できてしまい、異なる
/// 行数・列配置を持つ 2 つのバッチが同一バイト列（同一ハッシュ）に還元されうる
/// （回帰テスト `for_typed_insert_batch_rejects_row_boundary_reinterpretation`
/// 参照）。この曖昧性は行ごとの列**数**を明示すれば解消するため、
/// [`push_named_scalar_columns`] 自体（[`for_typed_insert`]／[`for_replace_by_text_key`]
/// も共有する既存レイアウト）は変更せず、本関数（Issue #771 で新設された
/// バッチ入力レイアウトのみ）に列数プレフィクス（`Value::Null` 除外後の非 NULL 列数。
/// `push_named_scalar_columns` が実際に書き込む列数と一致させる）を追加する形で
/// 閉じる。
pub(crate) fn for_typed_insert_batch(
    rows: &[TypedInsertBatchRow<'_>],
) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilder::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, visibility, embedding, columns) in rows {
        b.push_u64(*id);
        b.push_u8(visibility.to_byte());
        push_vector(&mut b, embedding)?;
        let non_null_columns = columns
            .iter()
            .filter(|(_, value)| !matches!(value, Value::Null))
            .count();
        let non_null_columns = u32::try_from(non_null_columns).map_err(|_| {
            StorageError::Codec("content hash batch row column count too large".to_string())
        })?;
        b.push_raw(&non_null_columns.to_le_bytes());
        push_named_scalar_columns(&mut b, columns)?;
    }
    Ok(b.finish())
}

/// [`for_typed_upsert`] の `SET` 右辺（ハッシュ入力専用の最小表現。SQL-20・
/// TASK-193、Issue #872）。本モジュールは `sql` に依存しない設計を維持するため、
/// `sql::allowlist::UpsertValue` をそのまま受け取らず、呼び出し元
/// （`tenant::upsert_typed_rows_unchecked`）が変換した最小 enum を受け取る。
pub(crate) enum UpsertAssignmentHashValue<'a> {
    /// `EXCLUDED.<col>`。ハッシュには新規挿入しようとした行の値ではなく
    /// **参照元の列名**を含める（同一 `EXCLUDED.<col>` 参照は新規行の内容が
    /// 変われば行ハッシュ側〔`rows` レイアウト〕で自然に区別されるため、ここで
    /// 実際の値を重複してハッシュ化する必要はない。列名自体が異なれば当然
    /// ハッシュも変わる）。
    Excluded(&'a str),
    Literal(&'a Value),
}

/// [`for_typed_upsert`] の衝突分岐（SQL-20・TASK-193、Issue #872）。
/// `sql::allowlist::OnConflictAction` に対応する最小表現。
pub(crate) enum UpsertHashAction<'a> {
    DoNothing,
    /// `(対象列名, 右辺)` の宣言順スライス（並べ替えない。`Parser::parse_
    /// upsert_assignment` の宣言順をそのまま保持する契約は
    /// `sql::parser::bind_upsert_assignments` が担う）。
    DoUpdate(&'a [(String, UpsertAssignmentHashValue<'a>)]),
}

/// `tenant::upsert_typed_rows_unchecked` 用（SQL-20・TASK-193、Issue #872）。
/// `INSERT ... ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...` を単一の
/// 「新規挿入かもしれないし更新かもしれない」操作として、[`OpTag::Upsert`]
/// タグの下でハッシュ化する（[`OpTag::Insert`]／[`OpTag::Update`] のいずれとも
/// 共有しない専用タグ。同一 `VALUES` でも plain `INSERT`／`DO NOTHING`／
/// `DO UPDATE`／`SET` 内容差は必ず異なるハッシュになる——衝突分岐そのものを
/// 先頭で `push_u8` してから行データを連結するため）。
///
/// 入力レイアウト: `push_u8(action_tag)`（`0` = DO NOTHING、`1` = DO UPDATE）
/// → DO UPDATE のみ `push_raw(assignment_count)` に続けて各割当を宣言順で
/// `push_bytes(target_column_name)` → `push_u8(value_kind)`（`0` = `EXCLUDED`、
/// `1` = リテラル）→ `EXCLUDED` なら `push_bytes(src_column_name)`、リテラル
/// なら [`push_value`] → その後は [`for_typed_insert_batch`] と**完全に同一**の
/// 行データレイアウト（件数プレフィクス＋行ごとの `(id, visibility, embedding,
/// 列数プレフィクス, 列名付きスカラー列)`）を連結する。単一行 UPSERT も
/// `rows.len() == 1` としてこのレイアウトを使う（`for_typed_insert` へは
/// 委譲しない——plain `INSERT` と同一 `operation_id` での UPSERT 再送を
/// `OpTag` の違いで機械的に内容不一致〔`22023`〕として検出させるため）。
/// 行境界の曖昧性回避（[`for_typed_insert_batch`] ドキュメント参照）は行数
/// プレフィクスを持つ本レイアウトにもそのまま適用される。
pub(crate) fn for_typed_upsert(
    action: &UpsertHashAction<'_>,
    rows: &[TypedInsertBatchRow<'_>],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Upsert);
    match action {
        UpsertHashAction::DoNothing => b.push_u8(0),
        UpsertHashAction::DoUpdate(assignments) => {
            b.push_u8(1);
            let count = u32::try_from(assignments.len()).map_err(|_| {
                StorageError::Codec("content hash upsert assignment count too large".to_string())
            })?;
            b.push_raw(&count.to_le_bytes());
            for (name, value) in assignments.iter() {
                b.push_bytes(name.as_bytes())?;
                match value {
                    UpsertAssignmentHashValue::Excluded(src) => {
                        b.push_u8(0);
                        b.push_bytes(src.as_bytes())?;
                    }
                    UpsertAssignmentHashValue::Literal(v) => {
                        b.push_u8(1);
                        push_value(&mut b, v)?;
                    }
                }
            }
        }
    }

    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    b.push_raw(&count.to_le_bytes());
    for (id, visibility, embedding, columns) in rows {
        b.push_u64(*id);
        b.push_u8(visibility.to_byte());
        push_vector(&mut b, embedding)?;
        let non_null_columns = columns
            .iter()
            .filter(|(_, value)| !matches!(value, Value::Null))
            .count();
        let non_null_columns = u32::try_from(non_null_columns).map_err(|_| {
            StorageError::Codec("content hash batch row column count too large".to_string())
        })?;
        b.push_raw(&non_null_columns.to_le_bytes());
        push_named_scalar_columns(&mut b, columns)?;
    }
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

/// `tenant::update_row_columns_unchecked` 用（Issue #865・SQL-17・TASK-191。
/// 部分更新 `UPDATE <table> SET <col> = <lit>[, ...] WHERE id = <n>` の実行結線が
/// 使う専用ハッシュ。既存の [`for_update_encoded`]（`update_row_unchecked` が使う
/// 全行置換 API 向け。マージ後の行全体を対象）とは意図的に別 [`OpTag`] を持たせる
/// （ドメイン分離。同一 `operation_id` を全行置換 UPDATE と部分更新 UPDATE の
/// 双方で使い回した場合に、内容が実質同じでも異なる操作として `22023` へ倒す）。
///
/// 入力はクライアント要求由来の内容のみ（DB の現在状態＝マージ後の行内容には
/// 依存しない。本モジュールドキュメント「正規化の方針」参照）: `id` と、SET 句に
/// 書かれた（列名, 値）ペア列を**呼び出し元が渡した順のまま**（本関数自身は
/// 並べ替えない）連結する。列の位置ではなく名前で連結する理由・`Value::Null` 列の
/// 扱いは [`push_named_scalar_columns`] と共有する（`ALTER TABLE ADD COLUMN`
/// 耐性）。
///
/// 呼び出し元 `tenant::update_row_columns_unchecked` は表層をまたいだ再送の
/// 判定を安定させるため、以降の台帳記録・照合にはスキーマの列 index 順へ
/// 正規化した列スライスを渡す（`for_typed_insert` がスキーマ列順を渡す既存契約と
/// 同じ考え方。Issue #876 レビュー指摘）。加えて、この正規化を導入する**前**に
/// 台帳へ記録されたエントリ（SET 句の宣言順のままハッシュ計算されている）とも
/// 照合できるよう、宣言順のまま呼び出した本関数の結果を `legacy_hash` として
/// `ledger::record_in_txn_accepting` へ渡す（互換性の詳細は呼び出し元のコメント
/// 参照）。本関数自体はどちらの用途で呼ばれても同じ「渡された順のまま連結する」
/// 契約のままであり、`for_update_columns_differs_by_declared_order` で固定した
/// 単体契約は変えていない。
///
/// 行境界の曖昧性回避（[`for_typed_insert_batch`] と同じ理由）: 本関数は 1 回の
/// 呼び出しで 1 行分の SET 句しか扱わないため複数行の境界問題は生じないが、
/// SET 句の列**数**（`Value::Null` 除外後）を先頭にプレフィクスすることで、将来
/// `SET col = NULL` を許容する変更が入った場合でも [`push_named_scalar_columns`]
/// の「末尾まで連結するだけ」の形式が列挙順・列数のいずれについても一意に復元
/// できる状態を維持する（[`for_typed_insert_batch`] の行ごとの列数プレフィクスと
/// 同じ設計）。
pub(crate) fn for_update_columns(
    id: u64,
    columns: &[(&str, &Value)],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::UpdateColumns);
    b.push_u64(id);
    let non_null_columns = columns
        .iter()
        .filter(|(_, value)| !matches!(value, Value::Null))
        .count();
    let non_null_columns = u32::try_from(non_null_columns).map_err(|_| {
        StorageError::Codec("content hash update column count too large".to_string())
    })?;
    b.push_raw(&non_null_columns.to_le_bytes());
    push_named_scalar_columns(&mut b, columns)?;
    Ok(b.finish())
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
/// `truncate_table_unchecked`（SQL-22。`TRUNCATE TABLE <table> USING
/// OPERATION_ID '<id>'`）用。入力: なし（台帳キー自体が `(tenant, table,
/// operation_id)` でテーブル名を既に一意に識別しており、TRUNCATE 要求は
/// クライアント由来の可変フィールドをテーブル名以外に持たないため、他の
/// `for_*` と異なり本体に何も追記しない。同一 `operation_id` への TRUNCATE
/// 再送は常に内容一致となり `23505`（`DuplicateOperationId`）に収束する
/// （`22023` は構造的に到達不能だが、他操作と同じハッシュ機構を再利用することで
/// 台帳照合の実装を一貫させる）。
pub(crate) fn for_truncate() -> ContentHash {
    HashInputBuilder::new(OpTag::Truncate).finish()
}

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

/// フィールド長超過（`push_bytes`／件数プレフィクスの `u32` 上限超過）を
/// `wire_code` `54000` へ写像する（[`for_update_where`]／[`for_delete_where`]
/// 系の共通ヘルパー。実際には上流の既存上限〔`MAX_METADATA_FILTERS`・
/// `MAX_EXPR_NODES`・`MAX_UDF_PARAMS`・`MAX_SESSION_UDFS`〕により事実上到達
/// しないが、`push_bytes` 自体の fail-closed 契約を崩さないよう明示的に
/// 写像する）。
fn dml_hash_field_too_large() -> crate::sql::allowlist::SqlSurfaceError {
    crate::sql::allowlist::SqlSurfaceError::payload_too_large("content hash field too large")
}

/// 述語つき `UPDATE ... WHERE`（SQL-19・TASK-192、Issue #871・対象ビヘイビア:
/// RECOVER-11）の内容照合ハッシュ。ADR `docs/design/multi-row-dml-operation-id.md`
/// （Issue #868）§4 のレイアウトをそのまま実装する。
///
/// 他の `for_*` と異なり、本関数（および [`for_delete_where`]）は
/// `sql::allowlist::WherePredicate`・`sql::udf_call::UdfRegistry` を直接受け取る
/// （ADR §4.2 が提案するシグネチャをそのまま採用）。`WHERE` の構文形（束縛前）を
/// ハッシュ源にする契約（ADR §4.4「構文段 AST を直接ハッシュする理由」）上、
/// `recovery` モジュールが `sql` モジュールへ依存する形になるが、`sql` は既に
/// `recovery`（`content_hash`・`ledger`・`required_op_id`）へ依存しているため、
/// 同一クレート内のモジュール参照としてはどちらの向きも許容される（Rust が
/// 禁止するのはクレート単位の循環のみ）。
///
/// 入力: `table`（テーブル名）・`assignments`（`SET` 句の宣言順 `(列名, リテラル)`
/// 対応。`ValidatedPredicateUpdate::assignments()` が保持する順序そのまま）・
/// `where_predicates`（`WHERE` 句の宣言順。同じく並べ替えない）・`udf_registry`
/// （呼び出し元セッションの `UdfRegistry`。`WHERE` 式中の `Call` を解決するために
/// 使う。ADR §4.4.1）。
///
/// **計算位置（ADR §5.1 からの意図的な変更）**: ADR は `bind_update_form`／
/// `bind_predicate_delete` の内部で計算し束縛済み型（`BoundPredicateUpdate`／
/// `BoundPredicateDelete`）へ保持させることを推奨するが、これらの型の
/// コンストラクタ（`pub fn new`／`pub(crate) fn new`）は既に固定された引数
/// （束縛済み `MetadataFilter`／`BoundExpr`）を取り、生の `WherePredicate`／
/// `UdfRegistry` を経由しない別入口（NoSQL 表層直接構築、Issue #876）を将来
/// 持つ設計であるため、これらの型へ `content_hash` フィールドを追加すると
/// `new` の契約が割れる。本実装は `core.rs::EngineCore`（`Validated*` 形と
/// `session.udfs()` の両方を持つ唯一の呼び出し元）が束縛の直前に 1 回だけ
/// 呼び出し、`sql::exec::execute_predicate_update`／`execute_predicate_delete`
/// へ `&ContentHash` として渡す（詳細は `docs/design/predicate-dml-exec.md`
/// 参照）。
///
/// **エラー型（ADR §4.2 からの意図的な変更）**: ADR は `StorageError` を返す
/// シグネチャを示すが、本実装は `sql::allowlist::SqlSurfaceError` を直接返す。
/// WASM UDF 呼び出しの拒否（ADR §4.4.1「WASM UDF は本節の対象外」）は
/// `wire_code` を伴う SQL 表層のエラーであり、`StorageError` には対応する
/// variant が存在しない。ADR は ERR-2 分類として `0A000` を挙げているが、
/// `sql::allowlist::SqlSurfaceError` には `0A000` を返す variant が存在しない
/// （`grep -n '"0A000"' crates/engine/src/sql/allowlist.rs` で不在を確認済み。
/// `0A000` は NoSQL 表層の op 許可リスト専用）ため、本実装は
/// `SqlSurfaceError::unsupported`（許可形状外・`42601`）へ写像する。
pub(crate) fn for_update_where(
    table: &str,
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    let mut b = HashInputBuilder::new(OpTag::UpdateWhere);
    b.push_bytes(table.as_bytes())
        .map_err(|_| dml_hash_field_too_large())?;
    push_dml_assignments(&mut b, assignments)?;
    push_dml_where_predicates(&mut b, where_predicates, udf_registry)?;
    Ok(b.finish())
}

/// 述語つき `DELETE ... WHERE`（SQL-19・TASK-192、Issue #871・対象ビヘイビア:
/// RECOVER-11）の内容照合ハッシュ。[`for_update_where`] と同じ `WHERE` 直列化
/// （[`push_dml_where_predicates`]）を共有し、`SET` 割当を持たない点のみが
/// 異なる（ADR §4.3「2. `SET` 割当（`UPDATE` のみ。`DELETE` には無い）」）。
/// タグは [`OpTag::DeleteWhere`]（[`OpTag::UpdateWhere`] とは別ドメイン）。
pub(crate) fn for_delete_where(
    table: &str,
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    let mut b = HashInputBuilder::new(OpTag::DeleteWhere);
    b.push_bytes(table.as_bytes())
        .map_err(|_| dml_hash_field_too_large())?;
    push_dml_where_predicates(&mut b, where_predicates, udf_registry)?;
    Ok(b.finish())
}

/// `SET` 割当（宣言順）を連結する（ADR §4.3 の 2 番）。件数プレフィクス（u32 LE）
/// → 各要素につき `push_bytes(列名)`・リテラル種別タグ（`String`＝1・`Number`＝2）・
/// `push_bytes(リテラル生文字列)`。列名は宣言どおりの大文字小文字のまま連結する
/// （`bind_set_assignments` の厳密一致と揃える）。`push_named_scalar_columns`
/// （`Value::Null` を除外する実装）は再利用しない——将来 `SET col = NULL` が
/// 追加されたときに黙って脱落させない前方ガード（ADR §4.3 参照）。
fn push_dml_assignments(
    b: &mut HashInputBuilder,
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::InsertLiteral;

    let count = u32::try_from(assignments.len()).map_err(|_| dml_hash_field_too_large())?;
    b.push_raw(&count.to_le_bytes());
    for (name, literal) in assignments {
        b.push_bytes(name.as_bytes())
            .map_err(|_| dml_hash_field_too_large())?;
        match literal {
            InsertLiteral::String(s) => {
                b.push_u8(1);
                b.push_bytes(s.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            InsertLiteral::Number(s) => {
                b.push_u8(2);
                b.push_bytes(s.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            InsertLiteral::Bool(v) => {
                b.push_u8(3);
                b.push_u8(u8::from(*v));
            }
        }
    }
    Ok(())
}

/// `WHERE` 述語列（宣言順）を連結する（ADR §4.3 の 3 番）。`for_update_where`・
/// `for_delete_where` が共有する。件数プレフィクス（u32 LE）に続けて各
/// `WherePredicate` を種別タグ＋フィールドで直列化し、`Expression` 述語が
/// 参照する UDF の推移閉包（ADR §4.4.1）を集めて末尾に追記する。
fn push_dml_where_predicates(
    b: &mut HashInputBuilder,
    predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::WherePredicate;

    let count = u32::try_from(predicates.len()).map_err(|_| dml_hash_field_too_large())?;
    b.push_raw(&count.to_le_bytes());

    // 参照 UDF 集合（名前の辞書順。ADR §4.4.1「5. 決定的な順序」）。
    let mut referenced: std::collections::BTreeMap<String, crate::sql::udf_call::UdfDefinition> =
        std::collections::BTreeMap::new();

    for pred in predicates {
        match pred {
            WherePredicate::Equality { column, value } => {
                b.push_u8(1);
                b.push_bytes(column.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
                b.push_bytes(value.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            WherePredicate::Prefix { column, pattern } => {
                b.push_u8(2);
                b.push_bytes(column.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
                b.push_bytes(pattern.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            WherePredicate::PredicateCall { name } => {
                b.push_u8(3);
                b.push_bytes(name.to_ascii_lowercase().as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            WherePredicate::Expression(expr) => {
                b.push_u8(4);
                push_dml_expr(b, expr, None)?;
                collect_referenced_udfs(expr, udf_registry, &mut referenced)?;
            }
            // `BoolColumn`（`WHERE flag`）と `BoolEquality { value: true }`
            // （`WHERE flag = true`）は評価結果としては同一だが、構文が異なる
            // ため安全側に倒し別タグ・別ハッシュとする（Issue #883）。
            WherePredicate::BoolEquality { column, value } => {
                b.push_u8(5);
                b.push_bytes(column.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
                b.push_u8(u8::from(*value));
            }
            WherePredicate::BoolColumn { column } => {
                b.push_u8(6);
                b.push_bytes(column.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
        }
    }

    // 参照 UDF 定義セクション（ADR §4.4.1「6.」）。参照 UDF が無ければ件数
    // プレフィクスの 0 すら書かない（UDF を呼ばない文は本節導入前とビット同一の
    // ハッシュになる互換性を保つ）。
    if !referenced.is_empty() {
        let udf_count = u32::try_from(referenced.len()).map_err(|_| dml_hash_field_too_large())?;
        b.push_raw(&udf_count.to_le_bytes());
        for (name, def) in referenced.iter() {
            b.push_bytes(name.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            let param_count =
                u32::try_from(def.params.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&param_count.to_le_bytes());
            // パラメータ名は小文字化してから連結する（`push_dml_expr` の `Ident`
            // 側パラメータ参照・`bind_expr_in` の呼び出し引数解決と同じ大文字小文字
            // 非区別契約に揃える。ここを原文のまま連結すると `x`／`X` のように
            // 意味的に同一なパラメータ宣言が異なる content_hash を生み、同一
            // `operation_id` の正当な再送を内容不一致〔`22023`〕として誤拒否する）。
            for p in &def.params {
                b.push_bytes(p.to_ascii_lowercase().as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            push_dml_expr(b, &def.body, Some(&def.params))?;
        }
    }

    Ok(())
}

/// `Call` の呼び出し先を解決し、宣言的 UDF なら参照 UDF 集合（`out`）へ追加して
/// 本体を再帰的に走査する（推移閉包。ADR §4.4.1「4.」）。WASM UDF に解決される
/// 呼び出しは許可形状外として拒否する（`get_wasm` → `get` の順で必ず判定する。
/// ADR §4.4.1「2.」の順序を維持しないと WASM UDF の名前が誤って「組み込み関数」
/// 側へ分類されてしまう）。`UdfRegistry` は追記専用（自身より前に登録済みの
/// UDF のみを呼べる）ため巡回はなく、この再帰は必ず停止する。
fn collect_referenced_udfs(
    expr: &crate::sql::udf_call::Expr,
    udf_registry: &crate::sql::udf_call::UdfRegistry,
    out: &mut std::collections::BTreeMap<String, crate::sql::udf_call::UdfDefinition>,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::udf_call::Expr;

    match expr {
        Expr::Number(_) | Expr::Ident(_) => Ok(()),
        Expr::Call { name, args } => {
            if udf_registry.get_wasm(name).is_some() {
                return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "WASM UDF calls are not supported in predicate-form UPDATE/DELETE WHERE clauses",
                ));
            }
            if let Some(def) = udf_registry.get(name) {
                let lower = name.to_ascii_lowercase();
                if let std::collections::btree_map::Entry::Vacant(entry) = out.entry(lower) {
                    entry.insert(def.clone());
                    collect_referenced_udfs(&def.body, udf_registry, out)?;
                }
            }
            for arg in args {
                collect_referenced_udfs(arg, udf_registry, out)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_referenced_udfs(lhs, udf_registry, out)?;
            collect_referenced_udfs(rhs, udf_registry, out)
        }
    }
}

/// `Expr` をタグ付き前置順で直列化する（ADR §4.4）。`params` が `Some` の場合、
/// `Ident` が参照 UDF 自身のパラメータ（大文字小文字を区別せず照合）を指すときに
/// 限り小文字化して連結する（ADR §4.4.1「6.」。`bind_expr_in` のパラメータ解決が
/// 大文字小文字を無視するため、同じ UDF の異なる大文字小文字綴りが異なるハッシュに
/// ならないようにする）。`WHERE` 直下（`params == None`）の `Ident`（列参照）は
/// 大文字小文字を区別したまま連結する（ADR §4.4 既定規則）。
fn push_dml_expr(
    b: &mut HashInputBuilder,
    expr: &crate::sql::udf_call::Expr,
    params: Option<&[String]>,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::udf_call::Expr;

    match expr {
        Expr::Number(raw) => {
            b.push_u8(1);
            let v = crate::sql::udf_call::parse_number_literal(raw)?;
            b.push_raw(&v.to_bits().to_le_bytes());
        }
        Expr::Ident(name) => {
            b.push_u8(2);
            let is_param = params
                .map(|ps| ps.iter().any(|p| p.eq_ignore_ascii_case(name)))
                .unwrap_or(false);
            if is_param {
                b.push_bytes(name.to_ascii_lowercase().as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            } else {
                b.push_bytes(name.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
        }
        Expr::Call { name, args } => {
            b.push_u8(3);
            b.push_bytes(name.to_ascii_lowercase().as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            let count = u32::try_from(args.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&count.to_le_bytes());
            for arg in args {
                push_dml_expr(b, arg, params)?;
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            b.push_u8(4);
            b.push_u8(dml_binop_tag(*op));
            push_dml_expr(b, lhs, params)?;
            push_dml_expr(b, rhs, params)?;
        }
    }
    Ok(())
}

/// `BinOp` を 1 バイトへ写像する（ADR §4.4）。
fn dml_binop_tag(op: crate::sql::udf_call::BinOp) -> u8 {
    use crate::sql::udf_call::BinOp;
    match op {
        BinOp::Add => 1,
        BinOp::Sub => 2,
        BinOp::Mul => 3,
        BinOp::Div => 4,
        BinOp::Gt => 5,
        BinOp::Lt => 6,
        BinOp::Ge => 7,
        BinOp::Le => 8,
        BinOp::Eq => 9,
    }
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

    // for_truncate は入力を持たないため常に同一値を返す（決定性の固定）。
    // OpTag が異なる for_delete とはハッシュが一致しないことも合わせて固定する
    // （ドメイン分離タグ・OpTag のみで区別できることの確認）。
    #[test]
    fn for_truncate_is_deterministic_and_differs_from_other_ops() {
        assert_eq!(for_truncate(), for_truncate());
        assert_ne!(for_truncate(), for_delete(1));
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
    // TABLE-13・Issue #882 計画 F9: Real=5／Double=6 のタグ割り当てを固定する
    // golden テスト。既存タグ（Null=0／Text=1／Vector=2）と衝突せず、かつ
    // 同じビットパターンを持つ異なる型（`Real(1.0)` と `Double(1.0)`）を
    // 区別できることを固定する。
    #[test]
    fn for_typed_insert_real_and_double_tags_are_stable_and_distinct() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let real_col = Value::Real(1.0);
        let double_col = Value::Double(1.0);
        let text_col = Value::Text("1".to_string());
        let cols_real: [(&str, &Value); 1] = [("v", &real_col)];
        let cols_double: [(&str, &Value); 1] = [("v", &double_col)];
        let cols_text: [(&str, &Value); 1] = [("v", &text_col)];

        let h_real = for_typed_insert(7, Visibility::Public, &embedding, &cols_real).expect("hash");
        let h_double =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_double).expect("hash");
        let h_text = for_typed_insert(7, Visibility::Public, &embedding, &cols_text).expect("hash");

        // 同じ論理値（1.0）でも型タグが異なれば別ハッシュになる（タグが
        // ハッシュ入力に混ざっている証拠）。
        assert_ne!(h_real, h_double);
        assert_ne!(h_real, h_text);
        assert_ne!(h_double, h_text);

        // 再現性: 同一入力から同一ハッシュが得られる（タグ値が偶然の実行時
        // 揺れでない）。
        let h_real_again =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_real).expect("hash");
        assert_eq!(h_real, h_real_again);
    }

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

    // Issue #771: `for_typed_insert_batch` は要求記載順を入力に含めるため、
    // 同一集合でも並び替えると別ハッシュになる（`for_insert_batch_encoded` と
    // 同じ設計判断。並び替えた同一集合の再送は意図的に区別する）。
    #[test]
    fn for_typed_insert_batch_differs_when_row_order_changes() {
        let embedding_a = [1.0_f32, 0.0, 0.0];
        let embedding_b = [0.0_f32, 1.0, 0.0];
        let title = Value::Text("hello".to_string());
        let cols: [(&str, &Value); 1] = [("title", &title)];
        let rows_forward: [TypedInsertBatchRow<'_>; 2] = [
            (1, Visibility::Private, &embedding_a, &cols),
            (2, Visibility::Private, &embedding_b, &cols),
        ];
        let rows_reversed: [TypedInsertBatchRow<'_>; 2] = [
            (2, Visibility::Private, &embedding_b, &cols),
            (1, Visibility::Private, &embedding_a, &cols),
        ];
        let h_forward = for_typed_insert_batch(&rows_forward).expect("hash");
        let h_reversed = for_typed_insert_batch(&rows_reversed).expect("hash");
        assert_ne!(h_forward, h_reversed);
    }

    // 行ごとの `visibility` の違いはバッチ全体のハッシュへ反映される
    // （`for_typed_insert_differs_by_visibility` の複数行版）。
    #[test]
    fn for_typed_insert_batch_differs_by_row_visibility() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let cols: [(&str, &Value); 1] = [("title", &title)];
        let rows_public: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Public, &embedding, &cols)];
        let rows_private: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Private, &embedding, &cols)];
        let h_public = for_typed_insert_batch(&rows_public).expect("hash");
        let h_private = for_typed_insert_batch(&rows_private).expect("hash");
        assert_ne!(h_public, h_private);
    }

    // `for_typed_insert` と同じく、`ALTER TABLE ADD COLUMN` 相当（未提供 →
    // `Value::Null` の追加列）を挟んでもバッチ全体のハッシュは不変（列名ベース・
    // `Null` 除外の設計を複数行版でも維持する）。
    #[test]
    fn for_typed_insert_batch_is_stable_across_added_nullable_column() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let before: [(&str, &Value); 1] = [("title", &title)];
        let rows_before: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Public, &embedding, &before)];
        let h_before = for_typed_insert_batch(&rows_before).expect("hash");

        let null_note = Value::Null;
        let after: [(&str, &Value); 2] = [("title", &title), ("note", &null_note)];
        let rows_after: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Public, &embedding, &after)];
        let h_after = for_typed_insert_batch(&rows_after).expect("hash");

        assert_eq!(h_before, h_after);
    }

    // codex-review P1 指摘（PR #823）の回帰固定: 行境界を示す列数プレフィクスが
    // 無いと、ある行の列データが「次の行」の `id`／`visibility`／`embedding` 長さへ
    // 意図的にはみ出す形で再解釈でき、実際には異なる行 id 集合へ書き込む 2 つの
    // バッチが同一バイト列（同一ハッシュ）へ還元されてしまう。
    //
    // 具体的には以下の 2 バッチを手計算で構築する（コメント中の 16 進バイト列は
    // `push_bytes`（4 バイト LE 長さ＋本体）・`push_value`（Null=0／Text=1 タグ＋
    // 長さ付き本体）のレイアウトから逆算した値。空ベクトル・空文字列値のみを使い
    // フィールド境界の算術を単純化してある）:
    //
    // - batch A: [(id1, "WXYZ"→"" の列あり), (id2a, 列なし)]
    // - batch B: [(id1, 列なし), (id2b, "PQRS"→"" の列あり)]
    //
    // id1 は共通だが、batch A は id2a を、batch B は id2b（id2a とは異なる値）を
    // 書き込む——つまり書き込み対象の行 id 集合そのものが異なる。列数プレフィクス
    // 導入前は、batch A の「行1の列データ」が batch B の「行2のヘッダ（id/vis/
    // vector 長）」として、batch A の「行2のヘッダ」が batch B の「行2の列データ」
    // として、それぞれ再解釈可能なバイト列になるよう choose してあり、両者は
    // バイト単位で完全に一致していた（本テストは列数プレフィクス導入後、この
    // 2 バッチが異なるハッシュになることを固定する）。
    #[test]
    fn for_typed_insert_batch_rejects_row_boundary_reinterpretation() {
        let id1 = 42u64;
        // id2a = u64::from_le_bytes([namelen=4, 'P','Q','R','S']) の逆算値。
        let id2a = u64::from_le_bytes([0x04, 0x00, 0x00, 0x00, b'P', b'Q', b'R', b'S']);
        // id2b = u64::from_le_bytes([namelen=4, 'W','X','Y','Z']) の逆算値。
        let id2b = u64::from_le_bytes([0x04, 0x00, 0x00, 0x00, b'W', b'X', b'Y', b'Z']);
        assert_ne!(id2a, id2b, "test construction requires distinct row ids");

        let empty_vec: [f32; 0] = [];
        let wxyz_value = Value::Text(String::new());
        let pqrs_value = Value::Text(String::new());
        let wxyz_col: [(&str, &Value); 1] = [("WXYZ", &wxyz_value)];
        let pqrs_col: [(&str, &Value); 1] = [("PQRS", &pqrs_value)];
        let no_cols: [(&str, &Value); 0] = [];

        let rows_a: [TypedInsertBatchRow<'_>; 2] = [
            (id1, Visibility::Public, &empty_vec, &wxyz_col),
            (id2a, Visibility::Public, &empty_vec, &no_cols),
        ];
        let rows_b: [TypedInsertBatchRow<'_>; 2] = [
            (id1, Visibility::Public, &empty_vec, &no_cols),
            (id2b, Visibility::Public, &empty_vec, &pqrs_col),
        ];

        let h_a = for_typed_insert_batch(&rows_a).expect("hash");
        let h_b = for_typed_insert_batch(&rows_b).expect("hash");
        assert_ne!(
            h_a, h_b,
            "batches writing to different row-id sets must not collide"
        );
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

    // [`for_insert_batch_encoded_reference`]（Issue #399 以前の一括処理版・A/B 計測
    // 専用）が production の [`for_insert_batch_encoded`]（ストリーミング版）と
    // 同一ダイジェストを返すことをピン留めする（codex-review 指摘・PR #419）。
    #[test]
    fn for_insert_batch_encoded_reference_matches_streaming() {
        let encoded_a: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
        let encoded_b: Vec<u8> = (0..700u32).map(|i| ((i * 3) % 256) as u8).collect();
        let hash_input: [(u64, &[u8]); 2] = [(1, &encoded_a), (2, &encoded_b)];

        let h_streaming = for_insert_batch_encoded(&hash_input).expect("hash");
        let h_reference = for_insert_batch_encoded_reference(&hash_input).expect("hash");
        assert_eq!(h_streaming, h_reference);
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

    // --- for_update_columns（Issue #865・SQL-17・TASK-191） --------------------

    #[test]
    fn for_update_columns_is_deterministic() {
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        assert_eq!(
            for_update_columns(1, &cols).expect("hash"),
            for_update_columns(1, &cols).expect("hash")
        );
    }

    #[test]
    fn for_update_columns_differs_by_id() {
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        assert_ne!(
            for_update_columns(1, &cols).expect("hash"),
            for_update_columns(2, &cols).expect("hash")
        );
    }

    #[test]
    fn for_update_columns_differs_by_value() {
        let ja = Value::Text("ja".to_string());
        let en = Value::Text("en".to_string());
        let cols_ja: [(&str, &Value); 1] = [("lang", &ja)];
        let cols_en: [(&str, &Value); 1] = [("lang", &en)];
        assert_ne!(
            for_update_columns(1, &cols_ja).expect("hash"),
            for_update_columns(1, &cols_en).expect("hash")
        );
    }

    #[test]
    fn for_update_columns_differs_by_declared_order() {
        let a = Value::Text("a".to_string());
        let b = Value::Text("b".to_string());
        let forward: [(&str, &Value); 2] = [("col_a", &a), ("col_b", &b)];
        let reversed: [(&str, &Value); 2] = [("col_b", &b), ("col_a", &a)];
        assert_ne!(
            for_update_columns(1, &forward).expect("hash"),
            for_update_columns(1, &reversed).expect("hash")
        );
    }

    // Vector 列を含む SET も同じ経路で受理できることを確認する（判断 C）。
    #[test]
    fn for_update_columns_accepts_vector_values() {
        let vec_a: Value = Value::Vector(vec![1.0, 2.0]);
        let vec_b: Value = Value::Vector(vec![1.0, 2.5]);
        let cols_a: [(&str, &Value); 1] = [("embedding", &vec_a)];
        let cols_b: [(&str, &Value); 1] = [("embedding", &vec_b)];
        assert_ne!(
            for_update_columns(1, &cols_a).expect("hash"),
            for_update_columns(1, &cols_b).expect("hash")
        );
    }

    // 専用 OpTag（UpdateColumns）を持つため、同じ id・同種の列名付きペアでも
    // 既存の Update（全行置換・`for_update_encoded`）・Insert とはハッシュが
    // 一致しない（ドメイン分離。判断 C）。
    #[test]
    fn for_update_columns_differs_from_other_op_tags() {
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let h_update_columns = for_update_columns(1, &cols).expect("hash");

        let h_update = for_update_encoded(1, b"unrelated-encoded-row").expect("hash");
        assert_ne!(h_update_columns, h_update);

        let h_insert = for_typed_insert(1, Visibility::Private, &[], &cols).expect("hash");
        assert_ne!(h_update_columns, h_insert);
    }

    // 列数プレフィクスにより、列境界の再解釈（異なる列数・並びが同一バイト列へ
    // 還元される事故）が起きないことを固定する（`for_typed_insert_batch_rejects_row_boundary_reinterpretation`
    // と同種の回帰テスト）。
    #[test]
    fn for_update_columns_null_valued_columns_are_excluded_from_the_count_prefix() {
        let non_null = Value::Text("x".to_string());
        let null = Value::Null;
        let with_null: [(&str, &Value); 2] = [("a", &non_null), ("b", &null)];
        let without_null: [(&str, &Value); 1] = [("a", &non_null)];
        // NULL 列は push_named_scalar_columns が除外するため、列数プレフィクス・
        // ハッシュ入力とも NULL 列を含まない形と一致する。
        assert_eq!(
            for_update_columns(1, &with_null).expect("hash"),
            for_update_columns(1, &without_null).expect("hash")
        );
    }

    // Issue #399 受け入れ 2: ストリーミング版（本番経路 `for_insert_batch_encoded`）と
    // 参照実装（旧・一括処理版 `for_insert_batch_encoded_reference`。バッチ全体を
    // `Vec` へ連結してからパディングする）の処理時間を、台帳ハッシュ対象と同オーダー
    // （1,000 行 × 約 0.5KB ≈ 500KB）の入力で手元比較するための手動専用テスト
    // （CI 非配線・既定 ignore）。`cargo test --release -p fandhe-vector-db-engine --lib
    // recovery::content_hash::tests::sha256_streaming_vs_reference_manual_timing
    // -- --ignored --nocapture` で実行する。
    //
    // codex-review 指摘（PR #419）: 従来はここで事前構築済みの 500KB 単一バッファへ
    // `update`/一括ハッシュを 1 回だけ呼んでおり、本番経路（`HashInputBuilder` が
    // 行・フィールド単位で `push_u64`／`push_bytes` を多数回呼ぶ）を再現していなかった。
    // 本版は `for_insert_batch_encoded`（本番。ストリーミング）と
    // `for_insert_batch_encoded_reference`（旧・一括処理版。同じ行・フィールド境界を
    // 共有する `#[cfg(test)]` 参照実装）を、同一の 1,000 行入力へ通す形で比較する。
    #[test]
    #[ignore = "手動計測専用（CI 非配線。--ignored --nocapture で明示実行する）"]
    fn sha256_streaming_vs_reference_manual_timing() {
        use std::time::Instant;

        // 1 行あたり約 0.5KB（実運用の embedding + metadata の encode 済みバイト列
        // と同オーダー）× 1,000 行 ≈ 500KB。行ごとに長さを僅かに変えて境界処理
        // （パディング境界近辺）も網羅する。
        let rows_owned: Vec<(u64, Vec<u8>)> = (0..1_000u64)
            .map(|id| {
                let len = 480 + (id % 41) as usize; // 480..=520 バイトで揺らす
                let row: Vec<u8> = (0..len as u32)
                    .map(|i| ((i.wrapping_add(id as u32)) % 256) as u8)
                    .collect();
                (id, row)
            })
            .collect();
        let hash_input: Vec<(u64, &[u8])> = rows_owned
            .iter()
            .map(|(id, row)| (*id, row.as_slice()))
            .collect();
        let total_bytes: usize = rows_owned.iter().map(|(_, row)| row.len()).sum();
        let iterations = 200;

        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(
                for_insert_batch_encoded_reference(std::hint::black_box(&hash_input))
                    .expect("hash"),
            );
        }
        let reference_elapsed = start.elapsed();

        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(
                for_insert_batch_encoded(std::hint::black_box(&hash_input)).expect("hash"),
            );
        }
        let streaming_elapsed = start.elapsed();

        println!(
            "for_insert_batch_encoded_reference (旧・一括処理版): {reference_elapsed:?} \
             ({:.3} ns/byte)",
            reference_elapsed.as_nanos() as f64 / (total_bytes as f64 * iterations as f64)
        );
        println!(
            "for_insert_batch_encoded (新・ストリーミング版・本番経路): {streaming_elapsed:?} \
             ({:.3} ns/byte)",
            streaming_elapsed.as_nanos() as f64 / (total_bytes as f64 * iterations as f64)
        );
        assert_eq!(
            for_insert_batch_encoded_reference(&hash_input).expect("hash"),
            for_insert_batch_encoded(&hash_input).expect("hash")
        );
    }

    // --- for_typed_upsert（SQL-20・TASK-193、Issue #872） ---

    #[test]
    fn for_typed_upsert_is_deterministic() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let a = for_typed_upsert(&UpsertHashAction::DoNothing, &rows).expect("hash");
        let b = for_typed_upsert(&UpsertHashAction::DoNothing, &rows).expect("hash");
        assert_eq!(a, b);
    }

    #[test]
    fn for_typed_upsert_differs_from_plain_insert_for_same_values() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let plain_insert =
            for_typed_insert(1, Visibility::Private, &embedding, &cols).expect("hash");
        let do_nothing = for_typed_upsert(&UpsertHashAction::DoNothing, &rows).expect("hash");
        assert_ne!(plain_insert, do_nothing);
    }

    #[test]
    fn for_typed_upsert_differs_between_do_nothing_and_do_update() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let assignments = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang),
        )];
        let do_nothing = for_typed_upsert(&UpsertHashAction::DoNothing, &rows).expect("hash");
        let do_update =
            for_typed_upsert(&UpsertHashAction::DoUpdate(&assignments), &rows).expect("hash");
        assert_ne!(do_nothing, do_update);
    }

    #[test]
    fn for_typed_upsert_differs_by_set_assignment_content() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang_ja = Value::Text("ja".to_string());
        let lang_en = Value::Text("en".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang_ja)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let assignments_ja = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang_ja),
        )];
        let assignments_en = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang_en),
        )];
        let h_ja =
            for_typed_upsert(&UpsertHashAction::DoUpdate(&assignments_ja), &rows).expect("hash");
        let h_en =
            for_typed_upsert(&UpsertHashAction::DoUpdate(&assignments_en), &rows).expect("hash");
        assert_ne!(h_ja, h_en);
    }

    #[test]
    fn for_typed_upsert_differs_between_excluded_and_literal_with_same_target_column() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let assignments_excluded = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Excluded("lang"),
        )];
        let assignments_literal = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang),
        )];
        let h_excluded =
            for_typed_upsert(&UpsertHashAction::DoUpdate(&assignments_excluded), &rows)
                .expect("hash");
        let h_literal = for_typed_upsert(&UpsertHashAction::DoUpdate(&assignments_literal), &rows)
            .expect("hash");
        assert_ne!(h_excluded, h_literal);
    }

    #[test]
    fn for_typed_upsert_differs_when_row_order_changes() {
        let embedding_a = [1.0_f32, 0.0, 0.0];
        let embedding_b = [0.0_f32, 1.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows_forward: [TypedInsertBatchRow<'_>; 2] = [
            (1, Visibility::Private, &embedding_a, &cols),
            (2, Visibility::Private, &embedding_b, &cols),
        ];
        let rows_reversed: [TypedInsertBatchRow<'_>; 2] = [
            (2, Visibility::Private, &embedding_b, &cols),
            (1, Visibility::Private, &embedding_a, &cols),
        ];
        let h_forward =
            for_typed_upsert(&UpsertHashAction::DoNothing, &rows_forward).expect("hash");
        let h_reversed =
            for_typed_upsert(&UpsertHashAction::DoNothing, &rows_reversed).expect("hash");
        assert_ne!(h_forward, h_reversed);
    }

    // --- for_update_where / for_delete_where（Issue #871・SQL-19・TASK-192） --------

    /// [`crate::wasm_udf::WasmUdfBackend`] のテスト専用モック実装。呼び出されたら
    /// 即座に成功を返す（`collect_referenced_udfs` の拒否判定は呼び出し前の
    /// レジストリ照会段で完結するため、本体の計算内容はテストの関心事ではない）。
    #[derive(Debug)]
    struct StubWasmBackend;

    impl crate::wasm_udf::WasmUdfBackend for StubWasmBackend {
        fn call_vector_scalar(
            &self,
            _v: &[f32],
            scalar: f64,
        ) -> Result<f64, crate::wasm_udf::WasmUdfError> {
            Ok(scalar)
        }
    }

    /// `id > 0`（[`WherePredicate::Expression`] の許可形状：比較演算子を頂点に持つ
    /// 木）を土台に、左辺だけを差し替えた式述語を組み立てるテストヘルパー。
    fn where_predicate_id_gt_zero(
        lhs: crate::sql::udf_call::Expr,
    ) -> crate::sql::allowlist::WherePredicate {
        use crate::sql::udf_call::{BinOp, Expr};
        crate::sql::allowlist::WherePredicate::Expression(Expr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(lhs),
            rhs: Box::new(Expr::Number("0".to_string())),
        })
    }

    /// ADR §4.4.1「WASM UDF は本節の対象外」の拒否判定（`collect_referenced_udfs` の
    /// `get_wasm` → `get` の順序）を固定する。`for_delete_where`・`for_update_where`
    /// いずれも `WHERE` 直列化を共有するため、WASM UDF 拒否は両者で同じ経路を通る。
    #[test]
    fn for_delete_where_rejects_where_referencing_wasm_udf() {
        use crate::sql::udf_call::{define_wasm_function, Expr, UdfRegistry};
        use std::sync::Arc;

        let mut registry = UdfRegistry::default();
        define_wasm_function(&mut registry, "wasm_fn", Arc::new(StubWasmBackend))
            .expect("register wasm udf");

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "wasm_fn".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let err = for_delete_where("t", &[predicate], &registry)
            .expect_err("WASM UDF calls must be rejected in predicate-form WHERE clauses");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn for_update_where_rejects_where_referencing_wasm_udf() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::{define_wasm_function, Expr, UdfRegistry};
        use std::sync::Arc;

        let mut registry = UdfRegistry::default();
        define_wasm_function(&mut registry, "wasm_fn", Arc::new(StubWasmBackend))
            .expect("register wasm udf");

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "wasm_fn".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });
        let value = InsertLiteral::String("ja".to_string());
        let assignments: [(&str, &InsertLiteral); 1] = [("lang", &value)];

        let err = for_update_where("t", &assignments, &[predicate], &registry)
            .expect_err("WASM UDF calls must be rejected in predicate-form WHERE clauses");
        assert_eq!(err.wire_code(), "42601");
    }

    /// 宣言的 UDF を 2 段（`outer` が `inner` を呼ぶ）で登録したレジストリを
    /// 組み立てるテストヘルパー。`param_case` はパラメータ宣言の綴りを変えるために
    /// 使い、大文字小文字畳み込み（ADR §4.4.1「6.」）の検証に使う。
    fn registry_with_two_layer_udf(
        param_case: fn(&str) -> String,
        inner_op: crate::sql::udf_call::BinOp,
    ) -> crate::sql::udf_call::UdfRegistry {
        use crate::sql::udf_call::{define_function, BinOp, Expr, UdfRegistry};

        let mut registry = UdfRegistry::default();
        let inner_param = param_case("x");
        define_function(
            &mut registry,
            "inner",
            std::slice::from_ref(&inner_param),
            &Expr::Binary {
                op: inner_op,
                lhs: Box::new(Expr::Ident(inner_param.clone())),
                rhs: Box::new(Expr::Number("1".to_string())),
            },
        )
        .expect("define inner");

        let outer_param = param_case("y");
        define_function(
            &mut registry,
            "outer",
            std::slice::from_ref(&outer_param),
            &Expr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(Expr::Call {
                    name: "inner".to_string(),
                    args: vec![Expr::Ident(outer_param.clone())],
                }),
                rhs: Box::new(Expr::Number("2".to_string())),
            },
        )
        .expect("define outer");
        registry
    }

    /// ADR §4.4.1「4. 推移閉包」「6. 決定的な順序・大文字小文字畳み込み」の
    /// ピン留め：`inner`/`outer` の意味が同一なら、パラメータ宣言の大文字小文字が
    /// 異なっていてもハッシュは一致する。
    #[test]
    fn for_delete_where_transitive_udf_closure_is_case_insensitive_on_params() {
        use crate::sql::udf_call::{BinOp, Expr};

        let lower = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Add);
        let upper = registry_with_two_layer_udf(|s| s.to_uppercase(), BinOp::Add);

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "outer".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let h_lower =
            for_delete_where("t", std::slice::from_ref(&predicate), &lower).expect("hash");
        let h_upper = for_delete_where("t", &[predicate], &upper).expect("hash");
        assert_eq!(
            h_lower, h_upper,
            "same UDF closure must hash identically regardless of declared parameter case"
        );
    }

    /// 上のテストは `define_function`（production の唯一の登録経路）がパラメータ名を
    /// 定義時点で必ず小文字へ正規化する（`sql::udf_call::define_function` の
    /// `normalized_params`）ため、`push_dml_where_predicates` の UDF 定義
    /// セクション自体が大文字小文字を畳み込む必要性を検証できていなかった
    /// （codex-review P1 指摘。`define_function` を経由しない場合でも定義
    /// セクションの直列化自体が大文字小文字を畳み込む契約であることを、正規化を
    /// 経由しない [`crate::sql::udf_call::insert_raw_definition_for_test`] で
    /// 直接ピン留めする）。
    #[test]
    fn for_delete_where_udf_definition_section_folds_param_case_independent_of_define_function() {
        use crate::sql::udf_call::{
            insert_raw_definition_for_test, Expr, UdfDefinition, UdfRegistry,
        };

        let mut lower = UdfRegistry::default();
        insert_raw_definition_for_test(
            &mut lower,
            "raw_fn",
            UdfDefinition {
                params: vec!["x".to_string()],
                body: Expr::Ident("x".to_string()),
            },
        );

        let mut upper = UdfRegistry::default();
        insert_raw_definition_for_test(
            &mut upper,
            "raw_fn",
            UdfDefinition {
                params: vec!["X".to_string()],
                body: Expr::Ident("X".to_string()),
            },
        );

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "raw_fn".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let h_lower =
            for_delete_where("t", std::slice::from_ref(&predicate), &lower).expect("hash");
        let h_upper = for_delete_where("t", &[predicate], &upper).expect("hash");
        assert_eq!(
            h_lower, h_upper,
            "definition-section serialization itself must fold parameter name case, \
             independent of any upstream normalization by define_function"
        );
    }

    /// 同じ形（`outer` が `inner` を呼ぶ）でも `inner` の本体演算子が異なれば
    /// （推移閉包の中身が変われば）ハッシュが変わることを固定する。
    #[test]
    fn for_delete_where_transitive_udf_closure_differs_when_inner_body_changes() {
        use crate::sql::udf_call::{BinOp, Expr};

        let add_registry = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Add);
        let sub_registry = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Sub);

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "outer".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let h_add =
            for_delete_where("t", std::slice::from_ref(&predicate), &add_registry).expect("hash");
        let h_sub = for_delete_where("t", &[predicate], &sub_registry).expect("hash");
        assert_ne!(
            h_add, h_sub,
            "differing UDF closures (inner body operator) must not collapse to the same hash"
        );
    }

    /// ADR §4.4.1「参照 UDF が無ければ件数プレフィクスの 0 すら書かない」契約の
    /// ピン留め：`WHERE` が UDF を一切呼ばない場合、セッションに UDF が登録済みか
    /// どうか（登録の有無・登録内容）に関わらずハッシュは完全に一致する。
    #[test]
    fn for_delete_where_hash_is_unaffected_by_unreferenced_udf_registrations() {
        use crate::sql::allowlist::WherePredicate;
        use crate::sql::udf_call::{BinOp, UdfRegistry};

        let predicate = WherePredicate::Equality {
            column: "lang".to_string(),
            value: "ja".to_string(),
        };

        let empty_registry = UdfRegistry::default();
        let populated_registry = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Add);

        let h_empty =
            for_delete_where("t", std::slice::from_ref(&predicate), &empty_registry).expect("hash");
        let h_populated = for_delete_where("t", &[predicate], &populated_registry).expect("hash");
        assert_eq!(
            h_empty, h_populated,
            "UDF section must be omitted entirely when no UDF is referenced by WHERE"
        );
    }
}
