//! RFC 9106 準拠の Argon2id 自作実装（`unsafe` なし）。依存追加なしでパスワードハッシュを
//! 実装するための構成要素（`.claude/rules/dependency-policy.md`）。内部の BLAKE2b 呼び出しは
//! [`super::blake2b`] に委譲する。
//!
//! `auth.rs` のユーザーストア照合（WIRE-2・WIRE-3）から呼ばれる。PHC 文字列
//! （`$argon2id$v=19$m=..,t=..,p=..$<salt>$<hash>`）の生成・照合を公開 API とし、
//! 生の Argon2 計算過程（レーン・スライス・ブロック生成）は非公開に留める。
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`）。

use super::blake2b;

/// 1 ブロック = 1024 バイト = 128 個の 64bit ワード（RFC 9106 §3.2）。
type Block = [u64; 128];

const BLOCK_QWORDS: usize = 128;
const ARGON2ID_TYPE: u64 = 2;
const ARGON2_VERSION: u32 = 0x13;

/// Argon2id の実行パラメータ。`m_cost_kib` はメモリ量（KiB＝ブロック数）、`t_cost` は
/// パス数、`p_cost` はレーン数（並列度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

/// OWASP 推奨の低メモリ構成（m=19456 KiB, t=2, p=1）。`auth.rs` のユーザーストアが
/// 新規ハッシュ生成時（`wire-server hash-password` サブコマンド）に用いる既定値。
pub const RECOMMENDED_PARAMS: Params = Params {
    m_cost_kib: 19_456,
    t_cost: 2,
    p_cost: 1,
};

/// `m_cost_kib` の上限（2 GiB）。ユーザーストアの PHC 行は構文的に正しければ任意の
/// 巨大値を持ちうる untrusted 入力であり、上限なしに `hash_raw` へ通すと
/// `m_prime` ブロック数に比例した `Vec` 確保で OOM を招く（1 テナントの認証失敗では
/// なくプロセス全体のクラッシュに波及するため、ロード時に fail-closed で拒否する）。
pub const MAX_M_COST_KIB: u32 = 2 * 1024 * 1024;
/// `t_cost`（パス数）の上限。CPU 時間の異常な引き伸ばしを防ぐ。
pub const MAX_T_COST: u32 = 64;
/// `p_cost`（レーン数）の上限。レーンごとの作業領域確保・スレッド分の CPU 消費を防ぐ。
pub const MAX_P_COST: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Argon2Error {
    /// メモリ量がレーン数に対して小さすぎる（RFC 9106: m は 8*p 以上必須）。
    MemoryTooSmall,
    /// `m_cost_kib` / `t_cost` / `p_cost` が [`MAX_M_COST_KIB`] 等の運用上限を超える
    /// （構文的には正しい PHC でも、計算資源の異常確保を防ぐため拒否する）。
    ParamOutOfRange(&'static str),
    /// レーン数・パス数・出力長が 0（RFC 9106 の定義域外）。
    InvalidParam(&'static str),
    /// PHC 文字列がこの実装が生成する形式（`$argon2id$v=19$m=..,t=..,p=..$salt$hash`）と
    /// 一致しない（fail-closed。詳細な原因は攻撃者へ推測材料を与えないため区別しない）。
    MalformedPhc,
}

impl std::fmt::Display for Argon2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Argon2Error::MemoryTooSmall => write!(f, "argon2id: m_cost too small for p_cost"),
            Argon2Error::ParamOutOfRange(name) => {
                write!(f, "argon2id: parameter exceeds resource ceiling: {name}")
            }
            Argon2Error::InvalidParam(name) => write!(f, "argon2id: invalid parameter: {name}"),
            Argon2Error::MalformedPhc => write!(f, "argon2id: malformed PHC string"),
        }
    }
}

/// `hash_raw` が実際に計算資源を確保する前に課すパラメータ検証をロード時にも再利用
/// できるよう切り出したもの（`auth.rs::UserStore::load_from_file` から呼ばれる）。
/// 構文検証（[`parse_phc`]）とは独立に、値の範囲（下限は RFC 9106 の `m >= 8*p`、
/// 上限は [`MAX_M_COST_KIB`] 等の運用上限）を検証する。
pub fn validate_params(params: &Params) -> Result<(), Argon2Error> {
    let p = params.p_cost;
    let m = params.m_cost_kib;
    let t = params.t_cost;
    if p == 0 || t == 0 {
        return Err(Argon2Error::InvalidParam("p_cost/t_cost must be >= 1"));
    }
    if m < p.saturating_mul(8) {
        return Err(Argon2Error::MemoryTooSmall);
    }
    if m > MAX_M_COST_KIB {
        return Err(Argon2Error::ParamOutOfRange("m_cost_kib"));
    }
    if t > MAX_T_COST {
        return Err(Argon2Error::ParamOutOfRange("t_cost"));
    }
    if p > MAX_P_COST {
        return Err(Argon2Error::ParamOutOfRange("p_cost"));
    }
    Ok(())
}

impl std::error::Error for Argon2Error {}

// ---------------------------------------------------------------------------
// BLAKE2b ベースの補助関数（H_0・可変長ハッシュ H'・圧縮関数 G）
// ---------------------------------------------------------------------------

/// RFC 9106 §3.3 の可変長ハッシュ関数 H'。`out_len<=64` は単発の BLAKE2b、それ以外は
/// 32 バイトずつ切り出しながら繰り返しハッシュする（ブロック生成 H'^1024・最終タグ H'^T
/// の両方がこの関数を経由する）。
fn h_prime(out_len: usize, input: &[u8]) -> Vec<u8> {
    let mut seed = Vec::with_capacity(4 + input.len());
    seed.extend_from_slice(&(out_len as u32).to_le_bytes());
    seed.extend_from_slice(input);

    if out_len <= 64 {
        return blake2b::hash(out_len, &seed);
    }

    let mut out = Vec::with_capacity(out_len);
    let mut v = blake2b::hash(64, &seed);
    out.extend_from_slice(&v[..32]);

    // r = ceil(out_len/32) - 2 個の 64 バイト値 V_2..V_r を生成し、それぞれの先頭
    // 32 バイトを連結する。最後の V_{r+1} だけ残りバイト数ぶん切り詰めて出力する。
    let r = out_len.div_ceil(32) - 2;
    for _ in 1..r {
        v = blake2b::hash(64, &v);
        out.extend_from_slice(&v[..32]);
    }
    let remaining = out_len - out.len();
    let v_last = blake2b::hash(remaining, &v);
    out.extend_from_slice(&v_last);
    out
}

/// RFC 9106 Figure 1 の H_0 生成（p/T/m/t/v/y と P・S・K・X を長さ接頭辞つきで連結して
/// BLAKE2b-512 する）。K（secret）・X（associated data）が空でも長さフィールド 0 は残す。
#[allow(clippy::too_many_arguments)]
fn compute_h0(
    password: &[u8],
    salt: &[u8],
    secret: &[u8],
    ad: &[u8],
    p_cost: u32,
    out_len: u32,
    m_cost_kib: u32,
    t_cost: u32,
) -> [u8; 64] {
    let mut buf = Vec::with_capacity(40 + password.len() + salt.len() + secret.len() + ad.len());
    buf.extend_from_slice(&p_cost.to_le_bytes());
    buf.extend_from_slice(&out_len.to_le_bytes());
    buf.extend_from_slice(&m_cost_kib.to_le_bytes());
    buf.extend_from_slice(&t_cost.to_le_bytes());
    buf.extend_from_slice(&ARGON2_VERSION.to_le_bytes());
    buf.extend_from_slice(&(ARGON2ID_TYPE as u32).to_le_bytes());
    buf.extend_from_slice(&(password.len() as u32).to_le_bytes());
    buf.extend_from_slice(password);
    buf.extend_from_slice(&(salt.len() as u32).to_le_bytes());
    buf.extend_from_slice(salt);
    buf.extend_from_slice(&(secret.len() as u32).to_le_bytes());
    buf.extend_from_slice(secret);
    buf.extend_from_slice(&(ad.len() as u32).to_le_bytes());
    buf.extend_from_slice(ad);

    let digest = blake2b::hash(64, &buf);
    let mut out = [0u8; 64];
    out.copy_from_slice(&digest);
    out
}

fn bytes_to_block(bytes: &[u8]) -> Block {
    let mut block = [0u64; BLOCK_QWORDS];
    for (i, word) in block.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
        *word = u64::from_le_bytes(b);
    }
    block
}

fn block_to_bytes(block: &Block) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    for word in block.iter() {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

/// RFC 9106 §3.6 の GB 関数（BLAKE2b の回転混合に乗算 `fBlaMka` を組み込んだ版）。
#[inline]
fn gb(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize) {
    #[inline]
    fn f_bla_mka(x: u64, y: u64) -> u64 {
        let m = 0xFFFF_FFFFu64;
        let xy = (x & m).wrapping_mul(y & m);
        x.wrapping_add(y).wrapping_add(xy.wrapping_mul(2))
    }
    v[a] = f_bla_mka(v[a], v[b]);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = f_bla_mka(v[c], v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = f_bla_mka(v[a], v[b]);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = f_bla_mka(v[c], v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// RFC 9106 §3.6 の置換 P（16 個の 64bit ワードに対する 1 ラウンド）。
fn p_round(v: &mut [u64; 16]) {
    gb(v, 0, 4, 8, 12);
    gb(v, 1, 5, 9, 13);
    gb(v, 2, 6, 10, 14);
    gb(v, 3, 7, 11, 15);
    gb(v, 0, 5, 10, 15);
    gb(v, 1, 6, 11, 12);
    gb(v, 2, 7, 8, 13);
    gb(v, 3, 4, 9, 14);
}

/// RFC 9106 §3.5 の圧縮関数 G(X,Y)。R=X^Y に対し行方向・列方向へ P を適用して Z を得て、
/// Z XOR R を返す（前段パスとの XOR 合成は呼び出し側 [`fill_segment`] の責務）。
fn g(x: &Block, y: &Block) -> Block {
    let mut r = [0u64; BLOCK_QWORDS];
    for i in 0..BLOCK_QWORDS {
        r[i] = x[i] ^ y[i];
    }
    let r_before = r;

    // 8x8 の 16 バイトレジスタ行列とみなし、まず行方向（連続する 16 ワード = 1 行）へ。
    for i in 0..8 {
        let mut v: [u64; 16] = r[16 * i..16 * i + 16].try_into().expect("16 words");
        p_round(&mut v);
        r[16 * i..16 * i + 16].copy_from_slice(&v);
    }
    // 続いて列方向（16 レジスタおきに 1 ワードずつ取り出した 8 列）へ。
    for i in 0..8 {
        let idx = [
            2 * i,
            2 * i + 1,
            2 * i + 16,
            2 * i + 17,
            2 * i + 32,
            2 * i + 33,
            2 * i + 48,
            2 * i + 49,
            2 * i + 64,
            2 * i + 65,
            2 * i + 80,
            2 * i + 81,
            2 * i + 96,
            2 * i + 97,
            2 * i + 112,
            2 * i + 113,
        ];
        let mut v = [0u64; 16];
        for (k, &ix) in idx.iter().enumerate() {
            v[k] = r[ix];
        }
        p_round(&mut v);
        for (k, &ix) in idx.iter().enumerate() {
            r[ix] = v[k];
        }
    }

    let mut out = [0u64; BLOCK_QWORDS];
    for i in 0..BLOCK_QWORDS {
        out[i] = r_before[i] ^ r[i];
    }
    out
}

// ---------------------------------------------------------------------------
// メモリ充填（レーン・スライスごとの segment 処理。RFC 9106 §3.4）
// ---------------------------------------------------------------------------

/// RFC 9106 §3.4.2 の index_alpha 相当（参照先ブロックの非一様分布サンプリング）。
/// `i` はセグメント内の 0 起算位置、`pr_low32` は擬似乱数値の下位 32 ビット。
fn index_alpha(
    pass: u32,
    slice: u32,
    i: u32,
    seg_len: u32,
    q: u32,
    same_lane: bool,
    pr_low32: u32,
) -> u32 {
    let reference_area_size: u64 = if pass == 0 {
        if slice == 0 {
            (i - 1) as u64
        } else if same_lane {
            (slice * seg_len + i - 1) as u64
        } else {
            let base = slice * seg_len;
            (if i == 0 { base - 1 } else { base }) as u64
        }
    } else if same_lane {
        (q - seg_len + i - 1) as u64
    } else {
        let base = q - seg_len;
        (if i == 0 { base - 1 } else { base }) as u64
    };

    let pr = pr_low32 as u64;
    let rel = (pr * pr) >> 32;
    let relative_position = reference_area_size - 1 - ((reference_area_size * rel) >> 32);

    let start_position: u64 = if pass != 0 {
        if slice == 3 {
            0
        } else {
            ((slice + 1) * seg_len) as u64
        }
    } else {
        0
    };

    ((start_position + relative_position) % (q as u64)) as u32
}

/// 1 レーン・1 スライスぶんの segment を埋める（RFC 9106 §3.4 のスライス同期処理の
/// 単位）。Argon2id のハイブリッドアドレッシング（pass 0 のスライス 0・1 のみ
/// data-independent、それ以外は data-dependent）をここで切り替える。
#[allow(clippy::too_many_arguments)]
fn fill_segment(
    blocks: &mut [Block],
    pass: u32,
    lane: u32,
    slice: u32,
    p_cost: u32,
    q: u32,
    seg_len: u32,
    t_cost: u32,
    m_prime: u32,
) {
    let data_independent = pass == 0 && slice < 2;
    let starting_index = if pass == 0 && slice == 0 { 2 } else { 0 };

    let zero_block = [0u64; BLOCK_QWORDS];
    let mut input_block = [0u64; BLOCK_QWORDS];
    let mut address_block = [0u64; BLOCK_QWORDS];
    if data_independent {
        input_block[0] = pass as u64;
        input_block[1] = lane as u64;
        input_block[2] = slice as u64;
        input_block[3] = m_prime as u64;
        input_block[4] = t_cost as u64;
        input_block[5] = ARGON2ID_TYPE;
        if starting_index == 2 {
            input_block[6] += 1;
            address_block = g(&zero_block, &input_block);
            address_block = g(&zero_block, &address_block);
        }
    }

    for i in starting_index..seg_len {
        let curr_col = slice * seg_len + i;
        let prev_col = if curr_col == 0 { q - 1 } else { curr_col - 1 };

        let pseudo_rand: u64 = if data_independent {
            if i % (BLOCK_QWORDS as u32) == 0 {
                input_block[6] += 1;
                address_block = g(&zero_block, &input_block);
                address_block = g(&zero_block, &address_block);
            }
            address_block[(i % (BLOCK_QWORDS as u32)) as usize]
        } else {
            blocks[(lane * q + prev_col) as usize][0]
        };

        let mut ref_lane = ((pseudo_rand >> 32) as u32) % p_cost;
        if pass == 0 && slice == 0 {
            ref_lane = lane;
        }
        let same_lane = ref_lane == lane;
        let ref_col = index_alpha(
            pass,
            slice,
            i,
            seg_len,
            q,
            same_lane,
            (pseudo_rand & 0xFFFF_FFFF) as u32,
        );

        let prev_block = blocks[(lane * q + prev_col) as usize];
        let ref_block = blocks[(ref_lane * q + ref_col) as usize];
        let new_val = g(&prev_block, &ref_block);

        let dst = &mut blocks[(lane * q + curr_col) as usize];
        if pass == 0 {
            *dst = new_val;
        } else {
            for k in 0..BLOCK_QWORDS {
                dst[k] ^= new_val[k];
            }
        }
    }
}

/// Argon2id の生タグ（バイト列）を計算する。`secret`（K）・`ad`（X）は RFC 9106 の
/// KAT（公開テストベクタ）検証のためだけに一般化した引数であり、`auth.rs` からの
/// パスワード照合呼び出しでは両方とも空スライスを渡す。
pub fn hash_raw(
    password: &[u8],
    salt: &[u8],
    secret: &[u8],
    ad: &[u8],
    params: &Params,
    out_len: usize,
) -> Result<Vec<u8>, Argon2Error> {
    validate_params(params)?;
    let p = params.p_cost;
    let m = params.m_cost_kib;
    let t = params.t_cost;
    if out_len < 4 {
        return Err(Argon2Error::InvalidParam("out_len must be >= 4"));
    }

    let m_prime = 4 * p * (m / (4 * p));
    let q = m_prime / p;
    let seg_len = q / 4;

    let h0 = compute_h0(password, salt, secret, ad, p, out_len as u32, m, t);

    let mut blocks: Vec<Block> = vec![[0u64; BLOCK_QWORDS]; m_prime as usize];
    for lane in 0..p {
        let mut seed0 = Vec::with_capacity(72);
        seed0.extend_from_slice(&h0);
        seed0.extend_from_slice(&0u32.to_le_bytes());
        seed0.extend_from_slice(&lane.to_le_bytes());
        blocks[(lane * q) as usize] = bytes_to_block(&h_prime(1024, &seed0));

        let mut seed1 = Vec::with_capacity(72);
        seed1.extend_from_slice(&h0);
        seed1.extend_from_slice(&1u32.to_le_bytes());
        seed1.extend_from_slice(&lane.to_le_bytes());
        blocks[(lane * q + 1) as usize] = bytes_to_block(&h_prime(1024, &seed1));
    }

    for pass in 0..t {
        for slice in 0..4 {
            for lane in 0..p {
                fill_segment(&mut blocks, pass, lane, slice, p, q, seg_len, t, m_prime);
            }
        }
    }

    let mut c = [0u64; BLOCK_QWORDS];
    for lane in 0..p {
        let blk = &blocks[(lane * q + q - 1) as usize];
        for i in 0..BLOCK_QWORDS {
            c[i] ^= blk[i];
        }
    }

    Ok(h_prime(out_len, &block_to_bytes(&c)))
}

// ---------------------------------------------------------------------------
// PHC 文字列（`$argon2id$v=19$m=..,t=..,p=..$salt$hash`）のエンコード・デコード
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// パディングなし標準 base64 エンコード（PHC 文字列の salt・hash フィールド用。
/// 依存追加なしで完結させるため自作する）。
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (b0 as u32) << 16 | (b1 as u32) << 8 | (b2 as u32);
        out.push(B64_ALPHABET[(n >> 18 & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[(n >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[(n >> 6 & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// [`b64_encode`] の逆変換。untrusted な PHC 文字列（ユーザーストアファイル由来）を
/// 扱うため、アルファベット外の文字・不正長は `None`（fail-closed）で拒否する。
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        B64_ALPHABET.iter().position(|&x| x == c).map(|p| p as u32)
    }
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3 + 3);
    for chunk in bytes.chunks(4) {
        let len = chunk.len();
        if len == 1 {
            return None;
        }
        let c0 = val(chunk[0])?;
        let c1 = val(chunk[1])?;
        let n = c0 << 18 | c1 << 12;
        out.push((n >> 16) as u8);
        if len >= 3 {
            let c2 = val(chunk[2])?;
            let n = n | c2 << 6;
            out.push((n >> 8) as u8);
            if len == 4 {
                let c3 = val(chunk[3])?;
                out.push((n | c3) as u8);
            }
        }
    }
    Some(out)
}

/// 新規レコード生成（`wire-server hash-password` サブコマンド）向け: 指定 salt・
/// パラメータで PHC 文字列を組み立てる。
pub fn encode_phc(password: &[u8], salt: &[u8], params: &Params) -> Result<String, Argon2Error> {
    let hash = hash_raw(password, salt, &[], &[], params, 32)?;
    Ok(format!(
        "$argon2id$v=19$m={},t={},p={}${}${}",
        params.m_cost_kib,
        params.t_cost,
        params.p_cost,
        b64_encode(salt),
        b64_encode(&hash)
    ))
}

/// PHC 文字列をパラメータ・salt・hash に分解する（ユーザーストアのロード時検証・
/// 照合の両方から呼ばれる）。形式不一致はすべて [`Argon2Error::MalformedPhc`] に
/// 畳み込み、詳細な失敗理由を外部へ出さない（fail-closed）。
pub fn parse_phc(phc: &str) -> Result<(Params, Vec<u8>, Vec<u8>), Argon2Error> {
    let mut parts = phc.split('$');
    // split('$') の先頭要素は先頭 '$' の前の空文字列。
    if parts.next() != Some("") {
        return Err(Argon2Error::MalformedPhc);
    }
    if parts.next() != Some("argon2id") {
        return Err(Argon2Error::MalformedPhc);
    }
    if parts.next() != Some("v=19") {
        return Err(Argon2Error::MalformedPhc);
    }
    let param_field = parts.next().ok_or(Argon2Error::MalformedPhc)?;
    let salt_field = parts.next().ok_or(Argon2Error::MalformedPhc)?;
    let hash_field = parts.next().ok_or(Argon2Error::MalformedPhc)?;
    if parts.next().is_some() {
        return Err(Argon2Error::MalformedPhc);
    }

    let mut m_cost_kib = None;
    let mut t_cost = None;
    let mut p_cost = None;
    for kv in param_field.split(',') {
        let (k, v) = kv.split_once('=').ok_or(Argon2Error::MalformedPhc)?;
        let v: u32 = v.parse().map_err(|_| Argon2Error::MalformedPhc)?;
        match k {
            "m" => m_cost_kib = Some(v),
            "t" => t_cost = Some(v),
            "p" => p_cost = Some(v),
            _ => return Err(Argon2Error::MalformedPhc),
        }
    }
    let params = Params {
        m_cost_kib: m_cost_kib.ok_or(Argon2Error::MalformedPhc)?,
        t_cost: t_cost.ok_or(Argon2Error::MalformedPhc)?,
        p_cost: p_cost.ok_or(Argon2Error::MalformedPhc)?,
    };

    let salt = b64_decode(salt_field).ok_or(Argon2Error::MalformedPhc)?;
    let hash = b64_decode(hash_field).ok_or(Argon2Error::MalformedPhc)?;
    if salt.is_empty() || hash.is_empty() {
        return Err(Argon2Error::MalformedPhc);
    }
    Ok((params, salt, hash))
}

/// 定数時間比較（タイミングサイドチャネルでハッシュ一致長を漏らさない。WIRE-3 の
/// 認証照合・`auth.rs` のダミー照合の両方から呼ばれる）。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// PHC 文字列とパスワードを照合する。`auth.rs::verify` の中核（ユーザー本体・
/// ダミー照合の双方から同一関数を通ることで、経路差によるタイミング差を避ける）。
pub fn verify_phc(phc: &str, password: &[u8]) -> Result<bool, Argon2Error> {
    let (params, salt, expected_hash) = parse_phc(phc)?;
    let computed = hash_raw(password, &salt, &[], &[], &params, expected_hash.len())?;
    Ok(constant_time_eq(&computed, &expected_hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    /// RFC 9106 §5.3 の Argon2id 公開テストベクタ（p=4 レーン・secret・associated data
    /// を含む一般形。KAT はこの構成でのみ与えられているため、実装は p=1 専用に単純化
    /// せず一般の p に対応させてある）。spec 本文ではなく RFC の公開テキストが根拠。
    #[test]
    fn argon2id_matches_rfc9106_kat() {
        let password = [0x01u8; 32];
        let salt = [0x02u8; 16];
        let secret = [0x03u8; 8];
        let ad = [0x04u8; 12];
        let params = Params {
            m_cost_kib: 32,
            t_cost: 3,
            p_cost: 4,
        };
        let tag = hash_raw(&password, &salt, &secret, &ad, &params, 32).expect("valid params");
        assert_eq!(
            hex(&tag),
            "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659"
        );
    }

    /// RFC 9106 の H_0 プレハッシュ中間値（§5.3 記載の "Pre-hashing digest"）が
    /// 独立に一致すること（KAT 不一致時の切り分けを容易にする回帰確認）。
    #[test]
    fn h0_matches_rfc9106_prehash_digest() {
        let password = [0x01u8; 32];
        let salt = [0x02u8; 16];
        let secret = [0x03u8; 8];
        let ad = [0x04u8; 12];
        let h0 = compute_h0(&password, &salt, &secret, &ad, 4, 32, 32, 3);
        let expected = hex_decode(
            "2889de487eb42ae500c0007ed9252f1069eadec40d5765b485de6dc2437a67\
             b8546a2f0acc1a0882db8fcf74714b472e94df421a5da1112ffa11434370a1\
             e997",
        );
        assert_eq!(h0.to_vec(), expected);
    }

    #[test]
    fn phc_round_trip_encode_verify() {
        let params = Params {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        };
        let salt = b"0123456789abcdef";
        let phc = encode_phc(b"correct horse battery staple", salt, &params).expect("valid");
        assert!(verify_phc(&phc, b"correct horse battery staple").expect("valid phc"));
        assert!(!verify_phc(&phc, b"wrong password").expect("valid phc"));
    }

    #[test]
    fn parse_phc_rejects_malformed_strings() {
        assert_eq!(
            parse_phc("not-a-phc-string"),
            Err(Argon2Error::MalformedPhc)
        );
        assert_eq!(
            parse_phc("$argon2id$v=19$m=64,t=1,p=1$c2FsdA$"),
            Err(Argon2Error::MalformedPhc)
        );
        assert_eq!(
            parse_phc("$argon2i$v=19$m=64,t=1,p=1$c2FsdA$aGFzaA"),
            Err(Argon2Error::MalformedPhc)
        );
    }

    #[test]
    fn hash_raw_rejects_memory_too_small_for_lanes() {
        let params = Params {
            m_cost_kib: 4,
            t_cost: 1,
            p_cost: 4,
        };
        assert_eq!(
            hash_raw(b"pw", b"salt", &[], &[], &params, 32),
            Err(Argon2Error::MemoryTooSmall)
        );
    }

    #[test]
    fn base64_round_trip() {
        for data in [
            b"".to_vec(),
            b"f".to_vec(),
            b"fo".to_vec(),
            b"foo".to_vec(),
            b"foob".to_vec(),
            b"fooba".to_vec(),
            b"foobar".to_vec(),
        ] {
            let encoded = b64_encode(&data);
            assert_eq!(b64_decode(&encoded).expect("valid base64"), data);
        }
    }
}
