//! バッチ経路（`batch_search.rs`・`batch_fallback.rs`）の中間バッファを、サイズクラス別の
//! フリーリスト・総量上限・グローバル LRU 自動破棄で管理するバッファプール（TASK-157
//! ポインタ: `docs/spec/05-tasks.md` TASK-157、対象ビヘイビア:
//! `docs/spec/04-behavior/core-engine.md` CORE-15）。
//!
//! `batch_search.rs::run_batch_search` は呼び出しのたびに 1 行分の f32 デコード先
//! （`row_buf`）を確保・破棄していた。バッチ検索・一括インデクシングは同一プロセス内で
//! 繰り返し呼ばれるため、本モジュールはそのスクラッチバッファをプールへ返却・再取得する
//! 経路を提供し、呼び出し回数に比例してアロケータ呼び出しが増え続けるのを避ける
//! （縮退経路 `batch_fallback.rs::FallbackBatchEngine` は `run_batch_search` を内部で
//! 呼ぶため、本プールの効果を構造的に共有する）。
//!
//! サイズクラスは 2 のべき乗刻み（[`size_class_for`]）。総量上限は [`BufferPool::new`] が
//! 受け取る `quota_bytes`（呼び出し元が構築時に決める設定値であり、環境変数・接続
//! パラメータ等の外部入力からは到達できない。CORE-12 と同じ「上書き機構自体を設けない」
//! 方針を踏襲する）。超過解放時はクラスを問わず解放時刻が最も古いバッファから破棄する
//! （グローバル LRU）。要求サイズの上限は呼び出し元が渡す `max_elems`（`batch_search.rs::
//! MAX_BATCH_DIM` 等、既存の検証済み上限）で、これを超える要求はアロケーション前に `Err`
//! で拒否する（本モジュール自身は独自の絶対上限を新設しない）。
//!
//! `indexing.md` INDEX-4（一括投入の処理量上限）は本 crate に未実装のため、本モジュールは
//! その具体的な上限値には依存せず、既に実装・検証済みの `batch_search.rs` の上限とのみ
//! 整合させる（INDEX-4 実装時の再整合は別タスクの宿題として残る）。GPU 転送ステージング用途は
//! 実際の GPU（`wgpu`）実行を追加していない現段階では対象を持たない（`batch_search.rs`
//! モジュール冒頭コメント参照）。

use std::collections::VecDeque;

/// 1 サイズクラスあたりの最小要素数（2 のべき乗）。これ未満の要求もこのクラスへ丸める。
const MIN_CLASS_ELEMS: usize = 64;

/// アイドルバッファ 1 個（要素数×4 バイトに加えベクタ自体のオーバーヘッドを無視した
/// 概算）が占めるバイト量を計算する（要素型は f32 固定）。
fn class_bytes(class_elems: usize) -> usize {
    class_elems.saturating_mul(std::mem::size_of::<f32>())
}

/// `len` 以上の最小の 2 のべき乗（[`MIN_CLASS_ELEMS`] 未満には丸め上げる）を返す。
fn size_class_for(len: usize) -> Option<usize> {
    let base = len.max(MIN_CLASS_ELEMS);
    if base.is_power_of_two() {
        Some(base)
    } else {
        base.checked_next_power_of_two()
    }
}

/// [`BufferPool`] のエラー（fail-closed）。メッセージは英語（プログラム出力文字列。
/// japanese-style.md 準拠）。
#[derive(Debug, Clone, PartialEq)]
pub enum BufferPoolError {
    /// 要求要素数が呼び出し元の指定した `max_elems`（`batch_search.rs::MAX_BATCH_DIM` 等、
    /// 既存の検証済み上限）を超過した。
    RequestTooLarge { len: usize, max: usize },
    /// サイズクラスの算出・容量計算が `usize` の範囲を超えた（理論上 `max_elems` が
    /// 極端に大きい呼び出し元でのみ起こりうる、アロケーション前の fail-closed 拒否）。
    RequestOverflow,
    /// `Vec::try_reserve_exact` がメモリ不足で失敗した（`arena.rs::try_reserve_exact` と
    /// 同方針。abort させず `Result` で伝播する）。
    AllocationFailed(String),
}

impl std::fmt::Display for BufferPoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferPoolError::RequestTooLarge { len, max } => {
                write!(f, "buffer_pool: request {len} exceeds max {max}")
            }
            BufferPoolError::RequestOverflow => {
                write!(f, "buffer_pool: size class computation overflowed")
            }
            BufferPoolError::AllocationFailed(msg) => {
                write!(f, "buffer_pool: allocation failed: {msg}")
            }
        }
    }
}

impl std::error::Error for BufferPoolError {}

/// アイドル状態で保持しているバッファ 1 個。`stamp` はグローバル LRU 順序を決める
/// 単調増加カウンタ値（[`BufferPool::release`] が付与する）。
struct IdleBuffer {
    buf: Vec<f32>,
    stamp: u64,
}

/// 1 サイズクラス分のアイドルバッファ列。新しい順に末尾へ積む（`Vec::pop` で
/// 直近解放されたバッファを優先的に再利用し、局所性を高める）。
struct SizeClass {
    elems: usize,
    idle: VecDeque<IdleBuffer>,
}

/// [`BufferPool::acquire`] が返す貸与ハンドル。バッファ本体と、[`BufferPool::release`] に
/// そのまま渡すべきサイズクラス（要素数）の組。
///
/// クラスをバッファの実 `capacity()` から逆算しない設計上の理由: `Vec::try_reserve_exact`
/// はアロケータが要求量ちょうどより多い容量を返すことを妨げない（標準ライブラリの契約は
/// 「少なくとも要求量」であって「ちょうど」ではない）。実 `capacity()` でクラスを判定すると、
/// アロケータの挙動次第でバッファが要求したクラスと異なるバケットへ紛れ込み、
/// 総量上限の会計精度やそのクラスでの再利用可能性が損なわれる。[`acquire`] 時に確定させた
/// 論理クラスをハンドルで持ち回ることで、実容量の値に依存しない。
#[derive(Debug)]
pub(crate) struct PooledBuffer {
    pub(crate) buf: Vec<f32>,
    class_elems: usize,
}

/// バッチ経路の中間バッファ（f32 スクラッチバッファ）用サイズクラス別プール
/// （TASK-157・CORE-15 ポインタ）。呼び出し元（`batch_search.rs::run_batch_search`）が
/// `acquire` → 使用 → `release` の順で明示的に貸し借りする（`Drop` によるハンドル自動返却は
/// 実装しない。coding-rust.md の `unsafe` 原則禁止方針の下で最小構成に留め、返却漏れは
/// 「プールへ戻らず通常どおり `Vec` として解放される」だけで安全側に縮退するため）。
///
/// # テナント越え残存データ防止（P0）
///
/// [`Self::acquire`] は返すバッファの `0..len` 範囲（呼び出し元が安全な `Vec` API で
/// 観測できる全範囲）を必ず `0.0` で埋めてから返す（直前にどのテナントのデータを
/// 保持していたバッファを再利用した場合でも、前テナントの値が読み取れない）。
/// [`Self::release`] 側でも保持前にバッファ全体をゼロクリアする（二重防御。
/// `batch_search.rs` の「選出前マスク＋選出後独立再検証」と同じ思想）。
///
/// # ピーク計測（`outstanding_bytes`）
///
/// アイドル保持中のバッファ（`retained_bytes`）だけでなく、`acquire` 済みで
/// まだ `release` されていない貸出中バッファも `outstanding_bytes` として計上し、
/// [`Self::peak_retained_bytes`]（テスト専用）は両者の合計の最大値を報告する。
/// `retained_bytes` だけを見ると、貸出中バッファの分だけ実際の総保持量を
/// 過小評価してしまう。
pub(crate) struct BufferPool {
    quota_bytes: usize,
    retained_bytes: usize,
    outstanding_bytes: usize,
    peak_retained_bytes: usize,
    next_stamp: u64,
    classes: Vec<SizeClass>,
}

impl BufferPool {
    /// 総量上限 `quota_bytes`（設定値。呼び出し元が構築時に決める。外部入力からは
    /// 到達不能）を持つ空のプールを構築する。
    pub(crate) fn new(quota_bytes: usize) -> Self {
        Self {
            quota_bytes,
            retained_bytes: 0,
            outstanding_bytes: 0,
            peak_retained_bytes: 0,
            next_stamp: 0,
            classes: Vec::new(),
        }
    }

    /// 現在アイドル保持している総バイト量。
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// 構築以降に観測した「アイドル保持＋貸出中」総バイト量の最大値（回帰テスト専用の
    /// 可観測性。TASK-157「ピークメモリが理論上限に対して有界であることを計測する
    /// 回帰テスト」の計測対象のため、本番経路からは呼ばない）。
    #[cfg(test)]
    pub(crate) fn peak_retained_bytes(&self) -> usize {
        self.peak_retained_bytes
    }

    fn note_peak(&mut self) {
        let total = self.retained_bytes.saturating_add(self.outstanding_bytes);
        self.peak_retained_bytes = self.peak_retained_bytes.max(total);
    }

    fn class_mut(&mut self, elems: usize) -> &mut SizeClass {
        if let Some(idx) = self.classes.iter().position(|c| c.elems == elems) {
            return match self.classes.get_mut(idx) {
                Some(class) => class,
                None => unreachable!("idx came from position() over the same Vec"),
            };
        }
        self.classes.push(SizeClass {
            elems,
            idle: VecDeque::new(),
        });
        // 直前に push したばかりのため `last_mut` は必ず `Some`。`unwrap`/`[]` を
        // 避け、`expect` の代わりに `if let` で明示的に処理する（コンテナ操作直後の
        // 自明な到達可能性であり untrusted 入力の分岐ではないが、house style に
        // 揃えて添字・unwrap を避ける）。
        match self.classes.last_mut() {
            Some(class) => class,
            None => unreachable!("just pushed a SizeClass"),
        }
    }

    /// `len` 要素以上を保持できるバッファを取得する。`max_elems` は呼び出し元が
    /// 検証済みの上限（`batch_search.rs::MAX_BATCH_DIM` 等）で、これを超える `len` は
    /// アロケーション前に `Err` で拒否する（fail-closed。呼び出し元ごとに上限が
    /// 異なりうるため、本モジュール自身は独自の絶対上限を持たない）。
    ///
    /// 返すバッファは `0..len` を必ず `0.0` へ初期化済み（残存データ防止。型ドキュメント
    /// 参照）。プール内に再利用可能なバッファがなければ `Vec::try_reserve_exact` で
    /// フォールブルに新規確保する（`arena.rs::try_reserve_exact` と同方針。abort させない）。
    /// 返す [`PooledBuffer`] のクラスをそのまま [`Self::release`] へ渡すことで、
    /// 実 `capacity()` に依存しない正確なクラス管理を保つ（型ドキュメント参照）。
    pub(crate) fn acquire(
        &mut self,
        len: usize,
        max_elems: usize,
    ) -> Result<PooledBuffer, BufferPoolError> {
        if len > max_elems {
            return Err(BufferPoolError::RequestTooLarge {
                len,
                max: max_elems,
            });
        }
        let class_elems = size_class_for(len).ok_or(BufferPoolError::RequestOverflow)?;

        let class = self.class_mut(class_elems);
        let mut buf = if let Some(idle) = class.idle.pop_back() {
            let freed = class_bytes(class_elems);
            self.retained_bytes = self.retained_bytes.saturating_sub(freed);
            idle.buf
        } else {
            let mut buf: Vec<f32> = Vec::new();
            buf.try_reserve_exact(class_elems).map_err(|e| {
                BufferPoolError::AllocationFailed(format!("failed to reserve buffer: {e}"))
            })?;
            buf
        };

        // 呼び出し元が安全な `Vec` API（`get`・イテレータ等）で観測できる範囲を
        // `len` に揃え、その範囲を `0.0` で埋める（プールから再利用したバッファに
        // 前テナントの値が残っていても、`0..len` を上書きし終えるまで公開しない）。
        buf.clear();
        buf.resize(len, 0.0);

        self.outstanding_bytes = self
            .outstanding_bytes
            .saturating_add(class_bytes(class_elems));
        self.note_peak();
        Ok(PooledBuffer { buf, class_elems })
    }

    /// バッファを返却する（アイドル保持へ戻す）。内容をゼロクリアしてから
    /// （残存データ防止の二重防御）貸与時に確定したサイズクラスへ格納し、
    /// ピーク計測を更新したうえで総量上限超過分をグローバル LRU で破棄する。
    ///
    /// クラスは [`PooledBuffer`] が保持する値をそのまま使う（`buf.capacity()` からの
    /// 逆算はしない。型ドキュメント参照）。クラスが [`MIN_CLASS_ELEMS`] 未満・2 のべき乗
    /// でない等、[`Self::acquire`] が生成しえない値であれば、内部不変条件違反として
    /// 格納せずに破棄する（fail-safe。本モジュール外から任意の `PooledBuffer` を
    /// 構築する経路は存在しないため、正常経路では到達しない防御的分岐）。
    pub(crate) fn release(&mut self, pooled: PooledBuffer) {
        let PooledBuffer {
            mut buf,
            class_elems,
        } = pooled;
        self.outstanding_bytes = self
            .outstanding_bytes
            .saturating_sub(class_bytes(class_elems));

        if class_elems < MIN_CLASS_ELEMS || !class_elems.is_power_of_two() {
            return;
        }
        for v in buf.iter_mut() {
            *v = 0.0;
        }
        buf.clear();

        let bytes = class_bytes(class_elems);
        // 1 個のバッファ単体で総量上限を超える場合は、格納しても即座に自分自身を
        // 破棄する羽目になるため、格納自体を行わない（無駄な会計更新を避ける）。
        if bytes > self.quota_bytes {
            self.note_peak();
            return;
        }

        let stamp = self.next_stamp;
        self.next_stamp = self.next_stamp.wrapping_add(1);
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        let class = self.class_mut(class_elems);
        class.idle.push_back(IdleBuffer { buf, stamp });

        // ピークは LRU 破棄の「前」に採る（破棄後に採ると、破棄ロジック自体が
        // 壊れていても総量が常に quota 以下に見えてしまい、テストが不変条件を
        // 検出できなくなる。破棄前の一時的な超過分＝quota + 直前に積んだ1個の
        // クラスぶんが理論上限になる）。
        self.note_peak();
        self.evict_until_within_quota();
    }

    /// 総量が `quota_bytes` を超えている間、クラスを問わずスタンプが最も古い
    /// （＝最後に使われてから最も時間が経っている）バッファを 1 個ずつ破棄する
    /// （グローバル LRU。CORE-15 が要求する「総量上限＋LRU で自動破棄」の破棄本体）。
    fn evict_until_within_quota(&mut self) {
        while self.retained_bytes() > self.quota_bytes {
            let mut oldest: Option<(usize, u64)> = None;
            for (class_idx, class) in self.classes.iter().enumerate() {
                if let Some(front) = class.idle.front() {
                    let is_older = match oldest {
                        Some((_, stamp)) => front.stamp < stamp,
                        None => true,
                    };
                    if is_older {
                        oldest = Some((class_idx, front.stamp));
                    }
                }
            }
            let Some((class_idx, _)) = oldest else {
                // アイドルバッファが 1 個も無いのに超過している状態は起こらない
                // （`release` は格納前に quota 超過を検査し、格納後にのみ本メソッドを
                // 呼ぶため）が、fail-closed に無限ループを避けて抜ける。
                break;
            };
            let Some(class) = self.classes.get_mut(class_idx) else {
                break;
            };
            // グローバル LRU の対象は各クラスの先頭（最も古い解放）に限らない
            // 可能性があるが、`release` は毎回 `push_back` するため各クラス内は
            // 解放順（＝スタンプ昇順）を維持しており、先頭が常にそのクラス内最古。
            let Some(evicted) = class.idle.pop_front() else {
                break;
            };
            let freed = class_bytes(class.elems);
            self.retained_bytes = self.retained_bytes.saturating_sub(freed);
            drop(evicted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CORE-15 ポインタ: サイズクラスの丸め。
    #[test]
    fn size_class_rounds_up_to_power_of_two_with_floor() {
        assert_eq!(size_class_for(1), Some(MIN_CLASS_ELEMS));
        assert_eq!(size_class_for(MIN_CLASS_ELEMS), Some(MIN_CLASS_ELEMS));
        assert_eq!(
            size_class_for(MIN_CLASS_ELEMS + 1),
            Some(MIN_CLASS_ELEMS * 2)
        );
        assert_eq!(size_class_for(200), Some(256));
        assert_eq!(size_class_for(256), Some(256));
    }

    #[test]
    fn size_class_overflow_returns_none() {
        assert_eq!(size_class_for(usize::MAX), None);
    }

    // CORE-15 ポインタ: fail-closed な上限拒否（アロケーション前）。
    #[test]
    fn acquire_rejects_request_over_caller_supplied_max_before_allocating() {
        let mut pool = BufferPool::new(1024 * 1024);
        let err = pool.acquire(9_000, 8_192).unwrap_err();
        assert_eq!(
            err,
            BufferPoolError::RequestTooLarge {
                len: 9_000,
                max: 8_192
            }
        );
        assert_eq!(pool.retained_bytes(), 0);
    }

    // 残存データ防止（P0 トラップ対応）: プールから再利用したバッファに前回の
    // 内容が残っていないことを確認する。
    #[test]
    fn acquire_never_exposes_residual_data_from_a_previous_release() {
        let mut pool = BufferPool::new(1024 * 1024);
        let mut pooled = pool.acquire(16, 64).expect("acquire");
        for (i, v) in pooled.buf.iter_mut().enumerate() {
            *v = (i as f32) + 1.0;
        }
        assert!(pooled.buf.iter().all(|&v| v != 0.0));
        pool.release(pooled);

        // 同じサイズクラスへ再取得し、内容が全てゼロであること（前回の値が
        // 一切読み取れないこと）を確認する。
        let reused = pool.acquire(16, 64).expect("acquire again");
        assert!(reused.buf.iter().all(|&v| v == 0.0));
    }

    // プールが実際にバッファを再利用していること（常に新規確保へフォールバックする
    // 無効なプールではないこと）を確認する。1 回解放したあとの 2 回目の取得で
    // アイドル保持量が 0 まで戻ることを見れば、フリーリストからの払い出しが
    // 起きたと判定できる。
    #[test]
    fn acquire_after_release_reuses_the_freed_buffer_instead_of_only_allocating_new() {
        let mut pool = BufferPool::new(1024 * 1024);
        let pooled = pool.acquire(16, 64).expect("acquire");
        pool.release(pooled);
        assert!(
            pool.retained_bytes() > 0,
            "release must retain the buffer in the free list"
        );

        let _reused = pool.acquire(16, 64).expect("acquire again");
        assert_eq!(
            pool.retained_bytes(),
            0,
            "reacquiring the same class must draw from the free list, not add a second buffer"
        );
    }

    // CORE-15 ポインタ: 総量上限＋グローバル LRU。異なるサイズクラスに跨って解放した
    // 場合でも、破棄対象がクラス内 LRU ではなく「解放時刻が最も古いもの」（グローバル
    // LRU）で選ばれることを、破棄後に残っているクラスを直接確認して検証する
    // （単に「保持総量が quota 以下」を見るだけでは、クラス内 LRU 実装や誤ったクラスを
    // 破棄する実装でも同じ結果になり得るため、破棄「対象」まで特定する）。
    #[test]
    fn release_evicts_the_globally_oldest_buffer_not_merely_within_its_own_class() {
        let class_a_bytes = class_bytes(MIN_CLASS_ELEMS); // 64要素クラス = 256 バイト
        let class_b_bytes = class_bytes(MIN_CLASS_ELEMS * 2); // 128要素クラス = 512 バイト
                                                              // 両方を同時に保持すると 1 バイトだけ超過し、ちょうど 1 個だけ破棄される
                                                              // ように quota を選ぶ。
        let quota = class_a_bytes + class_b_bytes - 1;
        let mut pool = BufferPool::new(quota);

        let buf_a = pool.acquire(10, 64).expect("acquire a"); // class=64要素、先に解放=最古
        let buf_b = pool.acquire(70, 128).expect("acquire b"); // class=128要素、後に解放
        pool.release(buf_a);
        pool.release(buf_b);

        // 最古（buf_a、クラス=64要素）だけが破棄され、クラス=128要素（buf_b）は
        // 生き残ることを保持総量の値から特定する。
        assert_eq!(
            pool.retained_bytes(),
            class_b_bytes,
            "the globally oldest buffer (class=64) must be evicted, leaving only class=128"
        );
    }

    // TASK-157: ピークメモリが理論上限に対して有界であることを計測する回帰テスト
    // （テスト専用の小さいローカル定数のみを使い、spec の受け入れ基準数値は
    // 転記しない）。多数の acquire/release サイクルを混在サイズクラスで回し、
    // ピーク（アイドル保持＋貸出中）が「quota + 直前に積んだ 1 個ぶんの理論上限」を
    // 一度も超えないことを確認する。ピークは破棄「前」（[`BufferPool::release`] の
    // `note_peak` 呼び出し順序）に採るため、破棄が機能していない実装であれば
    // サイクルを重ねるごとにピークが単調増加し、この上限をすぐに超える
    // （非破棄時の理論値 `naive_unbounded_peak` と比較して、実測ピークが
    // それより十分小さいことも合わせて確認し、アサーションが恒真になっていないことを
    // 検査する）。
    #[test]
    fn peak_retained_bytes_never_exceeds_quota_plus_one_pending_class_across_many_cycles() {
        // テスト専用の小さい上限。`max_elems=512` で登場しうる全サイズクラス
        // （64・128・256・512 要素）を同時にアイドル保持すると 3,840 バイトに
        // なるため、quota をそれより明確に小さくして、周回中に確実に LRU 破棄が
        // 発動する状況を作る（破棄が発動しない quota では本テストは常に緩く
        // 通ってしまい、破棄ロジックの回帰を検出できない）。
        let quota = 1024usize;
        let mut pool = BufferPool::new(quota);
        let max_elems = 512usize;
        let cycles = 500usize;
        // 破棄前に一時的に積みうる最大の 1 クラス分（理論上限の上乗せ項）。
        let largest_class = size_class_for(max_elems).expect("class for max_elems");
        let theoretical_bound = quota + class_bytes(largest_class);
        // 破棄が一切機能しなかった場合に到達しうる下限値（各サイクルで新しい
        // クラスへ解放し続けたと仮定した最悪ケースの近似）。実測ピークがこれを
        // 大きく下回ることを確認し、`theoretical_bound` の充足が「たまたま緩い
        // 上限に収まった」だけでなく破棄が実際に効いた結果であることを示す。
        let naive_unbounded_peak = cycles * class_bytes(largest_class);
        assert!(theoretical_bound < naive_unbounded_peak);

        for i in 0..cycles {
            let len = 1 + (i * 37) % max_elems;
            let pooled = pool.acquire(len, max_elems).expect("acquire");
            pool.release(pooled);
            assert!(
                pool.retained_bytes() <= quota,
                "retained bytes exceeded quota after eviction"
            );
        }

        assert!(
            pool.peak_retained_bytes() <= theoretical_bound,
            "peak retained {} must not exceed theoretical bound {}",
            pool.peak_retained_bytes(),
            theoretical_bound
        );
        assert!(
            pool.peak_retained_bytes() < naive_unbounded_peak,
            "peak retained {} must be far below the no-eviction worst case {}",
            pool.peak_retained_bytes(),
            naive_unbounded_peak
        );
    }

    // 単体バッファが quota を超える場合は格納自体を行わない（無限に破棄しては
    // 積む無駄な会計更新を避ける）ことを確認する。
    #[test]
    fn release_of_buffer_larger_than_quota_is_not_retained() {
        let mut pool = BufferPool::new(class_bytes(MIN_CLASS_ELEMS) - 1);
        let pooled = pool.acquire(10, 64).expect("acquire");
        pool.release(pooled);
        assert_eq!(pool.retained_bytes(), 0);
    }
}
