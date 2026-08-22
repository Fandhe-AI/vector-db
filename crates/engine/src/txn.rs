//! 宣言済みトランザクション分離レベルの API（TASK-88、対象ビヘイビア: TABLE-3。
//! ポインタ: `docs/spec/05-tasks.md` TASK-88・`docs/spec/04-behavior/data-model.md`
//! TABLE-3）。`storage.rs` の PERSIST-4 分離レベル注記と同一根拠
//! （`docs/spec/04-behavior/persistence.md` PERSIST-4、TASK-140 で検証済み）。
//!
//! 責務境界: `Storage` が保持する `redb::Database` の読み取り/書き込みトランザクション
//! 境界を、独自ロック層を追加せず `redb` の契約のまま型として公開する。後続の SQL
//! surface・カタログ層（TASK-85〜）から、宣言済み分離レベルの確認とトランザクション
//! ハンドルの取得に使われる想定であり、本モジュールはポリシー評価（RLS 事前フィルタ等）を
//! 行わない（`storage.rs` と同方針）。

use redb::{ReadableDatabase, ReadableTable};

use crate::storage::{
    decode_row, encode_row, Row, RowInput, Storage, StorageError, BATCH_LOG_TABLE, ROWS_TABLE,
};

/// engine が宣言する分離レベル（対象ビヘイビア: TABLE-3）。
///
/// 現時点でバリアントは 1 つのみ。値の追加は `redb` の契約から外れる分離レベルへの
/// 拡張を意味するため、破壊的変更として扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// 単一ライタ（書き込みは直列化）・スナップショット読み取り
    /// （読み取りは開始時点でコミット済みの状態のみを見る）。
    SingleWriterSnapshotRead,
}

impl std::fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // japanese-style.md: プログラム出力文字列は英語とする。
            IsolationLevel::SingleWriterSnapshotRead => {
                write!(f, "single-writer, snapshot-read")
            }
        }
    }
}

impl Storage {
    /// 宣言済みの分離レベルを返す（TABLE-3「分離レベルを確認する」操作）。
    ///
    /// `redb` の契約（`begin_write` の排他ロックによる直列化・`begin_read` のスナップ
    /// ショット分離）をそのまま宣言する固定値であり、本メソッドの呼び出し自体は何も
    /// 検証しない。実際に直列化・スナップショット性が成立していることの検証は
    /// [`Storage::begin_read`]・[`Storage::begin_write`] を使う統合テストの責務
    /// （`crates/engine/tests/txn_isolation.rs` 参照）。
    pub fn isolation_level(&self) -> IsolationLevel {
        IsolationLevel::SingleWriterSnapshotRead
    }

    /// スナップショット読み取りトランザクションを開始する（TABLE-3）。
    ///
    /// 戻り値の [`ReadSnapshot`] は開始時点でコミット済みの状態のみを見る。以降に
    /// 他のライタがコミットしても、このスナップショットが返す結果には反映されない
    /// （`redb::ReadTransaction` の契約をそのまま公開する）。
    pub fn begin_read(&self) -> crate::storage::Result<ReadSnapshot> {
        let txn = self.db().begin_read()?;
        Ok(ReadSnapshot { txn })
    }

    /// 書き込みトランザクションを開始する（TABLE-3。バッチ台帳を使わない素の複数行
    /// コミット用）。
    ///
    /// `redb::Database::begin_write` の排他ロックにより他の書き込みトランザクションと
    /// 直列化される（同時に開けるのは 1 本のみ。先行トランザクションが commit/abort
    /// するまで本呼び出しはブロックする）。戻り値の [`WriteTxn`] は明示的に
    /// [`WriteTxn::commit`] するまで変更を確定しない（[`WriteTxn`] のドキュメント
    /// コメント参照）。
    ///
    /// [`ROWS_TABLE`] + [`crate::storage::BATCH_LOG_TABLE`] を同一トランザクションで
    /// 扱う 2 テーブル横断コミット（TASK-90、対象ビヘイビア: TABLE-10）が必要な場合は
    /// [`Storage::begin_batch_write`] を使うこと。[`WriteTxn`] にはバッチ台帳を操作する
    /// API を持たせていない（PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf 対応。
    /// [`BatchWriteTxn`] のドキュメントコメントの「型分離の理由」参照）。
    ///
    /// 設計メモ: この排他ロックはタイムアウトを持たず、呼び出し元が [`WriteTxn`] を
    /// 保持し続ける限り他の書き込みを無期限にブロックする。本モジュール自体には
    /// untrusted 入力からの到達経路はないが、後続の SQL surface（TASK-85〜）から
    /// 呼び出す際は、無制限ブロック（DoS）を防ぐ上位側のタイムアウト・キャンセル
    /// 制御を検討すること（[`Storage::begin_batch_write`] も同じ排他ロック契約）。
    pub fn begin_write(&self) -> crate::storage::Result<WriteTxn> {
        let txn = self.db().begin_write()?;
        Ok(WriteTxn { txn })
    }

    /// バッチ台帳付き書き込みトランザクションを開始する（TASK-90、対象ビヘイビア:
    /// TABLE-10。[`ROWS_TABLE`] + [`crate::storage::BATCH_LOG_TABLE`] を同一
    /// トランザクションで扱う 2 テーブル横断コミット用）。
    ///
    /// 排他ロックの契約は [`Storage::begin_write`] と同じ（同一 `redb::Database` の
    /// 書き込みトランザクションを共有するため、[`WriteTxn`] と [`BatchWriteTxn`] は
    /// 互いに同時オープンできない）。戻り値の [`BatchWriteTxn`] が保証する不変条件は
    /// [`BatchWriteTxn`] のドキュメントコメント参照。
    pub fn begin_batch_write(&self) -> crate::storage::Result<BatchWriteTxn> {
        let txn = self.db().begin_write()?;
        Ok(BatchWriteTxn {
            txn,
            pending_row_count: 0,
        })
    }
}

/// [`Storage::begin_read`] が返すスナップショット読み取りハンドル（TABLE-3）。
///
/// 開始時点でコミット済みの状態のみを見る。保持している間は基盤の
/// `redb::ReadTransaction` を握り続けるが、`Storage` を直接借用はしない
/// （`redb::ReadTransaction` 自体が内部で必要な参照カウントを保持する契約のため）。
pub struct ReadSnapshot {
    txn: redb::ReadTransaction,
}

impl ReadSnapshot {
    /// 行 ID を指定して 1 行取得する（[`Storage::get`] と同じデコード契約）。
    ///
    /// このスナップショットの開始後にコミットされた変更（他ライタによる書き込みを
    /// 含む）は反映されない。
    pub fn get(&self, id: u64) -> crate::storage::Result<Row> {
        let table = match self.txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            // テーブル未作成（1 行も書き込んでいない）は「存在しない」として扱う
            // （storage.rs の Storage::get と同方針）。
            Err(redb::TableError::TableDoesNotExist(_)) => return Err(StorageError::NotFound(id)),
            Err(e) => return Err(e.into()),
        };
        let guard = table.get(id)?.ok_or(StorageError::NotFound(id))?;
        decode_row(id, guard.value())
    }
}

/// [`Storage::begin_write`] が返す書き込みトランザクションハンドル（TABLE-3。バッチ
/// 台帳を使わない素の複数行コミット専用）。
///
/// `redb::Database::begin_write` の排他ロックにより、生存している間は他の書き込み
/// トランザクションの `begin_write`／`begin_batch_write` をブロックする（直列化）。
/// [`WriteTxn::commit`] を呼ぶまで、書き込んだ行は他のトランザクションから見えない。
///
/// [`WriteTxn::commit`]・[`WriteTxn::abort`] のどちらも呼ばずに drop した場合は、
/// 内部の `redb::WriteTransaction` の `Drop` 実装により自動的に abort される
/// （redb 4.2.0 の契約。書き込みは確定せず、排他ロックは解放される）。
///
/// バッチ台帳（[`crate::storage::BATCH_LOG_TABLE`]）を操作する API は持たない。
/// TABLE-10 の 2 テーブル横断コミットが必要な場合は [`BatchWriteTxn`]（
/// [`Storage::begin_batch_write`] が返す）を使うこと（型分離の理由は
/// [`BatchWriteTxn`] のドキュメントコメント参照）。
pub struct WriteTxn {
    txn: redb::WriteTransaction,
}

impl WriteTxn {
    /// 単一行を書き込む（commit するまで確定しない。[`Storage::put`] と同じ
    /// upsert セマンティクス・エンコーディング契約）。
    pub fn put(&mut self, id: u64, row: &RowInput<'_>) -> crate::storage::Result<()> {
        let encoded = encode_row(row)?;
        let mut table = self.txn.open_table(ROWS_TABLE)?;
        table.insert(id, encoded.as_slice())?;
        Ok(())
    }

    /// トランザクションをコミットし、書き込みを確定する。
    pub fn commit(self) -> crate::storage::Result<()> {
        self.txn.commit()?;
        Ok(())
    }

    /// トランザクションを明示的に中断し、書き込みを破棄する。
    pub fn abort(self) -> crate::storage::Result<()> {
        self.txn.abort()?;
        Ok(())
    }
}

/// [`Storage::begin_batch_write`] が返す、バッチ台帳付き書き込みトランザクション
/// ハンドル（TASK-90、対象ビヘイビア: TABLE-10。[`ROWS_TABLE`] +
/// [`crate::storage::BATCH_LOG_TABLE`] を同一トランザクションで扱う）。
///
/// # 型分離の理由（PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf 対応）
///
/// 当初は `WriteTxn` 1 型に `log_batch` を持たせ、「`log_batch` を 1 回以上呼んだ
/// トランザクションのみ commit 時に未台帳行を検証する」というインスタンス単位の
/// フラグで TABLE-3 用途と TABLE-10 用途を両立させていた。しかしこの設計では、
/// 同一 DB に対して「台帳を使わない `put` だけの `WriteTxn` で commit」→「別
/// トランザクションで台帳ありの `put` + `log_batch` を commit」という**トランザクション
/// をまたいだ**操作の組み合わせにより、台帳を経由しない行を公開 API だけで作り込め、
/// 「台帳の row_count 合計 == 行総数」という不変条件を破れてしまう
/// （codex レビューが指摘した具体例）。そこで TABLE-3 用の `WriteTxn` からは
/// `log_batch` を完全に撤去し、TABLE-10 用の `BatchWriteTxn` を独立した型として
/// 分離した。
///
/// # 契約の適用範囲（重要）
///
/// 型分離をもってしても、[`Storage::put`]・[`Storage::put_batch`]・
/// [`WriteTxn::put`] は本モジュールとは独立に [`ROWS_TABLE`] へ直接書き込めるため、
/// 「台帳の row_count 合計 == 行総数」という不変条件を DB 全体に対して型システムで
/// 強制することはできない。この不変条件が保証されるのは、**あるテーブルへの書き込みを
/// `BatchWriteTxn` だけで行った場合に限る**（[`crate::storage::BATCH_LOG_TABLE`] の
/// ドキュメントコメント参照）。`BatchWriteTxn` と他の書き込み経路を同一 DB・同一
/// テーブルに対して混在させないことは呼び出し元の責務であり、本モジュールは意図的に
/// それを検出・拒否しない（`Storage::put` 等からは `BatchWriteTxn` の存在自体が
/// 見えないため、混在検出には DB 全体のスキャンを要し、書き込みの都度その代償を
/// 払うことは本 PR のスコープでは正当化されないと判断した）。
///
/// `BatchWriteTxn` 単体としては、以下をすべて満たしてはじめて `commit` が成功する:
/// - 新規挿入した行はすべて [`BatchWriteTxn::log_batch`] で台帳へ記録済みであること
///   （[`StorageError::UnloggedRows`]）
/// - `log_batch` は 1 件以上の新規挿入を伴って呼ばれること（[`StorageError::EmptyBatch`]）
/// - `batch_seq` は重複しないこと（[`StorageError::DuplicateBatchSeq`]）
/// - 同一 ID への 2 回目以降の `put`（upsert による上書き）は行数としてカウントしない
pub struct BatchWriteTxn {
    txn: redb::WriteTransaction,
    /// 直近の [`BatchWriteTxn::log_batch`] 呼び出し以降（または `BatchWriteTxn`
    /// 生成以降）に [`BatchWriteTxn::put`] で**新規挿入**した件数（既存 ID への
    /// 上書きは含まない）。呼び出し元から任意の行数を申告させず自身が実書き込みを
    /// 数えることで、「台帳の値 == そのバッチで `ROWS_TABLE` へ新規追加した行数」
    /// という契約を保証する（PR #129 codex レビュー PRRT_kwDOUAKASM6bbQ7l・
    /// PRRT_kwDOUAKASM6bbc_I 対応）。
    pending_row_count: u64,
}

impl BatchWriteTxn {
    /// 単一行を書き込む（commit するまで確定しない。[`Storage::put`] と同じ
    /// upsert セマンティクス・エンコーディング契約）。[`BatchWriteTxn::log_batch`] 向けの
    /// 内部カウンタ（`pending_row_count`）は、`redb::Table::insert` の戻り値
    /// （上書き前の旧値）を見て**新規挿入のときのみ**増やす。同一 ID への 2 回目以降の
    /// `put`（上書き）はカウントしない。カウントしてしまうと同一 ID への複数回 put
    /// だけで実在行数より多い値を台帳へ記録でき、「台帳の row_count 合計 == 行総数」
    /// という TABLE-10 の契約を公開 API だけで破れてしまうため
    /// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbc_I 対応）。
    pub fn put(&mut self, id: u64, row: &RowInput<'_>) -> crate::storage::Result<()> {
        let encoded = encode_row(row)?;
        let mut table = self.txn.open_table(ROWS_TABLE)?;
        let previous = table.insert(id, encoded.as_slice())?;
        if previous.is_none() {
            // 呼び出し元にはトランザクション内で扱える行数の上限（TASK-90 の想定用途
            // ではバッチ 1 本あたり高々数千件）を課しており u64 を溢れさせることは
            // できないが、coding-rust.md の checked 演算方針に合わせ、万一の溢れを
            // 黙って折り返さず fail-closed に検出する。
            self.pending_row_count = self
                .pending_row_count
                .checked_add(1)
                .ok_or(StorageError::PendingRowCountOverflow)?;
        }
        Ok(())
    }

    /// バッチ台帳（[`crate::storage::BATCH_LOG_TABLE`]）へ、直近の `log_batch` 以降に
    /// 実際に [`BatchWriteTxn::put`] で**新規挿入**した行数（既存 ID への上書きは
    /// 含まない）を 1 エントリとして書き込む（TASK-90、対象ビヘイビア: TABLE-10）。
    /// 同一 ID への複数回 `put` は `pending_row_count` を増やさないため、
    /// `log_batch(seq, 99)` のような過大申告だけでなく「同一 ID への put の
    /// 繰り返し」でも実在行数を超える値を記録できない
    /// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbc_I 対応）。[`BatchWriteTxn::put`] と
    /// 同一の `redb::WriteTransaction`（`self.txn`）内で操作するため、
    /// [`BatchWriteTxn::commit`]・[`BatchWriteTxn::abort`] のどちらを呼んでも
    /// [`ROWS_TABLE`] への行書き込みと本エントリは常に運命を共にする（2 テーブル
    /// 横断で原子的にコミット／破棄される）。
    ///
    /// `row_count` を呼び出し元から受け取らないのは、任意の値を渡せると
    /// `log_batch(seq, 0)` のみ・過大申告といった「実書き込み数と独立した値」を
    /// 公開 API だけで永続化できてしまい、台帳の契約を保証できないため
    /// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbQ7l 対応）。呼び出し成功後は
    /// カウンタを 0 にリセットし、次の `log_batch` は「その後の `put` 件数」のみを
    /// 記録する。
    ///
    /// 直近の `log_batch`（または `BatchWriteTxn` 生成）以降 1 件も新規挿入していない
    /// 状態（`pending_row_count == 0`）で呼ぶと [`StorageError::EmptyBatch`] で
    /// fail-closed に拒否する。ゼロ件エントリを許すと、クラッシュ検証ツールの検証
    /// オラクル（台帳の各エントリ値 == 実際にコミットされたバッチサイズ）と食い違う
    /// ため（PR #129 codex レビュー PRRT_kwDOUAKASM6bbnm7 対応）。
    ///
    /// 既存の `batch_seq` を渡すと [`StorageError::DuplicateBatchSeq`] で fail-closed に
    /// 拒否する（`redb` の `insert` は無条件上書きのため、検出しないと呼び出し元の
    /// 採番バグ・再試行ミスがバッチ台帳の不変条件（`batch_seq` ごとに 1 エントリ）を
    /// 静かに破壊する。security.md「不安全な設計」対応）。
    pub fn log_batch(&mut self, batch_seq: u64) -> crate::storage::Result<()> {
        if self.pending_row_count == 0 {
            return Err(StorageError::EmptyBatch);
        }
        let mut table = self.txn.open_table(BATCH_LOG_TABLE)?;
        if table.get(batch_seq)?.is_some() {
            return Err(StorageError::DuplicateBatchSeq(batch_seq));
        }
        table.insert(batch_seq, self.pending_row_count)?;
        self.pending_row_count = 0;
        Ok(())
    }

    /// トランザクションをコミットし、書き込みを確定する。
    ///
    /// 直近の `log_batch` 以降に新規挿入したのに台帳へ記録していない行
    /// （`pending_row_count != 0`）がないかを常に検証し、あれば
    /// [`StorageError::UnloggedRows`] で fail-closed に拒否する。`BatchWriteTxn` は
    /// TABLE-10 専用の型であり（[`WriteTxn`] と分離済み）、この検証を無条件に適用
    /// してよい（PR #129 codex レビュー PRRT_kwDOUAKASM6bbnm4・PRRT_kwDOUAKASM6bbyWf
    /// 対応）。
    pub fn commit(self) -> crate::storage::Result<()> {
        if self.pending_row_count != 0 {
            return Err(StorageError::UnloggedRows(self.pending_row_count));
        }
        self.txn.commit()?;
        Ok(())
    }

    /// トランザクションを明示的に中断し、書き込みを破棄する。
    pub fn abort(self) -> crate::storage::Result<()> {
        self.txn.abort()?;
        Ok(())
    }
}
