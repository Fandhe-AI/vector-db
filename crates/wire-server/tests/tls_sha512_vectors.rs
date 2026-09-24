//! SHA-512（`tls::sha512`。TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #960・
//! 親 #941）の公開 API だけを使う結合テスト。単体テストの内部関数照合とは
//! 別に、外部から見える契約（`Sha512::new`／`update`／`finalize`・`digest`）が
//! FIPS 180-4／RFC 6234 の公開テストベクタと一致し、ストリーミング分割が
//! 一括呼び出しと同じ結果になることを固定する。

use wire_server::tls::sha512::{digest, Sha512, BLOCK_LEN, DIGEST_LEN};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// RFC 6234 §8.5 TEST1。
#[test]
fn digest_matches_rfc6234_test1_abc() {
    let d = digest(b"abc");
    assert_eq!(
        hex(&d),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    assert_eq!(d.len(), DIGEST_LEN);
}

// RFC 6234 §8.5 TEST2_2（112 バイト・2 ブロックに収まる境界の入力）。
#[test]
fn digest_matches_rfc6234_test2_2() {
    let input = concat!(
        "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn",
        "hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
    );
    assert_eq!(input.len(), 112);
    let d = digest(input.as_bytes());
    assert_eq!(
        hex(&d),
        "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
    );
}

// RFC 6234 §8.5 TEST3（"a" を 1,000,000 回）。
#[test]
fn digest_matches_rfc6234_test3_million_a() {
    let input = vec![b'a'; 1_000_000];
    let d = digest(&input);
    assert_eq!(
        hex(&d),
        "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
    );
}

// FIPS 180-4／NIST 公開計算例: 空入力。
#[test]
fn digest_matches_known_digest_for_empty_input() {
    let d = digest(b"");
    assert_eq!(
        hex(&d),
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
    );
}

// パディング境界長（111/112/127/128。1 ブロックに収まるか 2 ブロックに
// なるかの分かれ目）で、あらゆる分割点の `update` が一括 `digest` と一致
// することを固定する。
#[test]
fn streaming_split_update_matches_one_shot_digest_at_boundary_lengths() {
    for len in [111usize, 112, 127, 128] {
        let input: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
        let expected = digest(&input);
        for split in 0..=len {
            let mut hasher = Sha512::new();
            hasher.update(&input[..split]);
            hasher.update(&input[split..]);
            assert_eq!(
                hasher.finalize(),
                expected,
                "mismatch at len={len}, split={split}"
            );
        }
    }
}

#[test]
fn streaming_split_update_matches_one_shot_for_various_chunk_sizes() {
    let input: Vec<u8> = (0..300).map(|i| (i % 256) as u8).collect();
    let expected = digest(&input);
    for chunk_size in [1usize, 3, 7, 16, 64, 111, 112, 127, 128, 129, 200] {
        let mut hasher = Sha512::new();
        for chunk in input.chunks(chunk_size) {
            hasher.update(chunk);
        }
        assert_eq!(
            hasher.finalize(),
            expected,
            "mismatch at chunk_size={chunk_size}"
        );
    }
}

#[test]
fn block_len_constant_matches_fips_180_4() {
    assert_eq!(BLOCK_LEN, 128);
}

#[test]
fn debug_output_does_not_leak_internal_state() {
    let mut hasher = Sha512::new();
    hasher.update(b"secret-input");
    let debug_str = format!("{hasher:?}");
    assert!(debug_str.contains("redacted"));
    assert!(!debug_str.contains("secret-input"));
}
