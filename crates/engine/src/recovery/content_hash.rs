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

use crate::row_codec::Value;
use crate::storage::{encode_row, RowInput, StorageError, Visibility};

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
struct HashInputBuilder(Vec<u8>);

impl HashInputBuilder {
    fn new(tag: OpTag) -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(DOMAIN_TAG);
        buf.push(tag as u8);
        HashInputBuilder(buf)
    }

    /// 可変長バイト列を「4 バイト LE 長さ＋本体」で連結する。untrusted 入力由来の
    /// 長さを上限検証してからアロケーションに使う契約（coding-rust.md）に従い、
    /// `u32` へ収まらない場合は `Err` で拒否する（`encode_row` 等の既存エンコーダが
    /// 既に検証済みの値のみが渡る想定だが、本関数単体でも fail-closed を保つ）。
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

    fn push_u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn finish(self) -> ContentHash {
        ContentHash(sha256(&self.0))
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
            let len = u32::try_from(vector.len())
                .map_err(|_| StorageError::Codec("content hash vector too large".to_string()))?;
            b.0.extend_from_slice(&len.to_le_bytes());
            for f in vector {
                b.0.extend_from_slice(&f.to_le_bytes());
            }
        }
    }
    Ok(())
}

/// `insert_row_unchecked` 用（TASK-101 対象経路 1）。入力: `(id, encoded_row)`。
/// `encoded_row` は [`crate::storage::encode_row`] の出力（`tenant_id`・`visibility`・
/// `embedding`・`metadata` を含む正準表現。呼び出し元がクライアント要求から構築した
/// [`RowInput`] をそのままエンコードしたもの）。
pub(crate) fn for_insert(id: u64, row: &RowInput<'_>) -> Result<ContentHash, StorageError> {
    let encoded = encode_row(row)?;
    let mut b = HashInputBuilder::new(OpTag::Insert);
    b.push_u64(id);
    b.push_bytes(&encoded)?;
    Ok(b.finish())
}

/// `insert_rows_unchecked` 用（TASK-101 対象経路 2）。バッチ全体で 1 ハッシュ。
/// 入力: 要求記載順の `(id, encoded_row)` 列（順序も入力に含める。並び替えた同一集合の
/// 再送を意図的に区別する設計）。
pub(crate) fn for_insert_batch(rows: &[(u64, RowInput<'_>)]) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilder::new(OpTag::InsertBatch);
    b.0.extend_from_slice(&count.to_le_bytes());
    for (id, row) in rows {
        let encoded = encode_row(row)?;
        b.push_u64(*id);
        b.push_bytes(&encoded)?;
    }
    Ok(b.finish())
}

/// `insert_typed_row_unchecked` 用（TASK-101 対象経路 3）。型付き列値から構築した
/// [`RowInput`] を [`for_insert`] と同じレイアウトでハッシュする（操作種別タグは
/// [`OpTag::Insert`] を共有する: 行形 `INSERT` と宣言的 `INSERT` は同じ「新規挿入」
/// 操作であり、経路の違いでハッシュ空間を分ける必要がない）。
pub(crate) fn for_typed_insert(id: u64, row: &RowInput<'_>) -> Result<ContentHash, StorageError> {
    for_insert(id, row)
}

/// `update_row_unchecked` 用（TASK-101 対象経路 4）。入力: `(id, encoded_row)`。
pub(crate) fn for_update(id: u64, row: &RowInput<'_>) -> Result<ContentHash, StorageError> {
    let encoded = encode_row(row)?;
    let mut b = HashInputBuilder::new(OpTag::Update);
    b.push_u64(id);
    b.push_bytes(&encoded)?;
    Ok(b.finish())
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
/// template_values)`。削除対象集合・採番される id 等の DB 状態由来の値に加え、
/// **チャンク化・埋め込み結果（`replace_typed_rows_by_text_key` へ渡る
/// 派生済み行データ）も含めない**（codex-review P1 指摘・PR #248。`chunking`
/// 設定や `Embedder` の応答は同一のクライアント要求に対しても実行時に変わり得る
/// ため、これらをハッシュへ含めると再送の内容一致判定が偽陰性
/// （`OperationIdContentMismatch` の誤検出）を起こす。ハッシュ入力は
/// クライアントが `INSERT` 文で実際に送った値
/// （`path`・`body`・その他の Text 列値＝`template_values`。`path`/`body`/VECTOR
/// 列は `sql::parser::bind_file_insert` により `template_values` 中で常に
/// `Value::Null` に正規化済みのため、`path`/`body` は別引数として明示的に渡す）
/// のみから決定的に構成する。本モジュールドキュメント「正規化の方針」参照。
pub(crate) fn for_replace_by_text_key(
    key_column: &str,
    key_value: &str,
    visibility: Visibility,
    path: &str,
    body: &str,
    template_values: &[Value],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::ReplaceByTextKey);
    b.push_bytes(key_column.as_bytes())?;
    b.push_bytes(key_value.as_bytes())?;
    b.push_u8(visibility.to_byte());
    b.push_bytes(path.as_bytes())?;
    b.push_bytes(body.as_bytes())?;
    let col_count = u32::try_from(template_values.len())
        .map_err(|_| StorageError::Codec("content hash column count too large".to_string()))?;
    b.0.extend_from_slice(&col_count.to_le_bytes());
    for v in template_values {
        push_value(&mut b, v)?;
    }
    Ok(b.finish())
}

// ---------------------------------------------------------------------------
// SHA-256（FIPS 180-4）自作実装。
//
// 依存追加が承認制のため（`.claude/rules/dependency-policy.md`）、本タスクの
// 自動運転下では既存の依存最小方針を維持する側に倒し、標準ライブラリのみで
// 実装する。`unsafe` は使わず、固定サイズ配列・`wrapping_*` 演算（FIPS 180-4 が
// 定める mod 2^32 加算そのもの。未定義動作にはならない）で構成する。
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

/// FIPS 180-4 5.1.1 節のパディング。長さフィールドは入力バイト長（`usize`）から
/// `u64` ビット長へ変換する。エンジン内部のハッシュ対象（台帳エントリ・行データ）は
/// 実運用上 `u64::MAX / 8` バイトに到達し得ないため `checked` 変換は不要（`as u64` は
/// 64 bit ターゲットの `usize` から可逆）。
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

fn sha256(input: &[u8]) -> [u8; 32] {
    let padded = pad(input);
    let mut h = H0;

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            // `as_chunks::<4>()` は固定長 4 バイト配列を返すため `from_be_bytes` は
            // 失敗しない。添字直接アクセスの代わりに `get_mut` で明示的に処理する
            // （coding-rust.md「untrusted 入力の扱い」と同じ規律を内部処理にも適用する）。
            if let Some(slot) = w.get_mut(i) {
                *slot = u32::from_be_bytes(*word);
            }
        }
        for i in 16..64 {
            let w15 = w[i - 15];
            let w2 = w[i - 2];
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
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

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        let bytes = word.to_be_bytes();
        let start = i * 4;
        if let Some(slot) = out.get_mut(start..start + 4) {
            slot.copy_from_slice(&bytes);
        }
    }
    out
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
        let template = vec![Value::Text("en".to_string())];
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
        let template = vec![Value::Text("en".to_string())];
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
}
