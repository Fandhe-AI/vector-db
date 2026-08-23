//! 決定的シードの疑似乱数生成器（xorshift64*）。
//!
//! ベンチ・回帰テストが使う入力データ（ベクトル・クエリ）を、同一シードから常に
//! 同一系列で再生成できるようにするための専用モジュール（TASK-158。ポインタ:
//! `docs/spec/05-tasks.md` TASK-158）。
//! アルゴリズムは public 参考実装
//! [rust-ai-library](https://github.com/Fandhe-AI/rust-ai-library)
//! `crates/bench-harness/src/rng.rs` と同系。
//!
//! # 暗号用途禁止（OWASP A02）
//!
//! xorshift64* は統計的品質のみを目的とした非暗号 PRNG である。鍵・トークン・
//! セッション識別子等のセキュリティ用途に使ってはならない。

/// 決定的シードの xorshift64* PRNG。
///
/// `protocol::run` の入力生成・`ab::run_ab` のワークロード合成から呼ばれる想定
/// （両モジュールとも `Deterministic` トレイト等は経由せず本型を直接保持する）。
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// 指定シードから生成器を作る。
    ///
    /// xorshift 系は状態 0 が不動点（常に 0 を返し続ける）になるため、シード 0 は
    /// 固定オフセットで補正する。呼び出し側はシード値の有効性を気にせず渡してよい。
    pub fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    /// 次の 64bit 疑似乱数値を返す。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0.0, 1.0)` の単位区間に収まる f32 疑似乱数値を返す。
    ///
    /// 上位 24bit のみを使い、f32 の仮数部精度内で一様性を保つ。
    pub fn next_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32; // 24bit
        (bits as f32) / (1u32 << 24) as f32
    }

    /// 指定次元の f32 ベクトルを `[-1.0, 1.0)` の一様分布で生成する。
    ///
    /// ベンチ入力（合成ベクトル・クエリベクトル）生成の共通ヘルパ。
    pub fn next_vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_f32() * 2.0 - 1.0).collect()
    }
}
