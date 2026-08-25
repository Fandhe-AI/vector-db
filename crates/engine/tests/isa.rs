//! `engine::isa` の結合テスト（TASK-156・対象ビヘイビア: CORE-14）。
//!
//! `dispatch.rs::detect_current_isa()` はもはやコンパイル時 `cfg(target_arch)` だけの
//! 保守的下限検出ではなく、`is_x86_feature_detected!` 等による実行時検出
//! （`isa::current()`）へ委譲する。本ファイルはその配線・決定性・数値整合、および
//! `unsafe`／sealed トークンの構造的な制約（外部上書き機構の不存在・公開コンストラクタ
//! の不存在・`unsafe` の局所化）をソース走査で検査する。
//!
//! `tests/dispatch.rs` の「crate 外から到達できる公開 API だけで検証する」という
//! 位置付けを踏襲し、`SimdKernel` の variant を直接構築することはしない
//! （トークン型・variant はいずれも `pub(crate)`／private フィールドのため、
//! crate 外からは構築できない）。

use engine::isa;

/// `isa::current().isa()` が、`std::arch` の feature 検出マクロ／`cfg(target_arch)`
/// からテスト側で独立に算出した期待値と一致すること。AVX-512 非搭載機では
/// `Avx2Fma` または `Scalar` を返すことを skip ではなく肯定的に検証する。
#[test]
fn detection_matches_std_feature_macros() {
    let expected = expected_isa_from_std_macros();
    assert_eq!(isa::current().isa(), expected);
}

#[cfg(target_arch = "x86_64")]
fn expected_isa_from_std_macros() -> isa::DetectedIsa {
    if std::arch::is_x86_feature_detected!("avx512f") {
        isa::DetectedIsa::Avx512
    } else if std::arch::is_x86_feature_detected!("avx2")
        && std::arch::is_x86_feature_detected!("fma")
    {
        isa::DetectedIsa::Avx2Fma
    } else {
        isa::DetectedIsa::Scalar
    }
}

#[cfg(target_arch = "aarch64")]
fn expected_isa_from_std_macros() -> isa::DetectedIsa {
    if std::arch::is_aarch64_feature_detected!("neon") {
        isa::DetectedIsa::Neon
    } else {
        isa::DetectedIsa::Scalar
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn expected_isa_from_std_macros() -> isa::DetectedIsa {
    isa::DetectedIsa::Scalar
}

/// `current()` の繰り返し呼び出し・複数スレッドからの呼び出しで同一値を返すこと、
/// `detect()`（毎回照会）とも一致すること（プロセス内単調性。`dispatch.rs::
/// select_execution_path` の参照透過性の前提）。
#[test]
fn detection_is_stable_within_process() {
    let baseline = isa::current().isa();

    for _ in 0..8 {
        assert_eq!(isa::current().isa(), baseline);
    }
    assert_eq!(isa::detect().isa(), baseline);

    let handles: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| isa::current().isa()))
        .collect();
    for handle in handles {
        assert_eq!(handle.join().expect("thread join"), baseline);
    }
}

/// `dispatch.rs::detect_current_isa()` が `isa::current().isa()` の写像であり、
/// `select_execution_path` が返す `SimdWidth` もそこから決まること（配線の回帰）。
#[test]
fn dispatch_detect_current_isa_reflects_runtime_detection() {
    use engine::dispatch::{
        detect_current_isa, select_execution_path, DetectedIsa, DispatchInput, ExecutionPath,
        SimdWidth,
    };

    assert_eq!(detect_current_isa(), isa::current().isa());

    let expected_width = match detect_current_isa() {
        DetectedIsa::Scalar => SimdWidth::Scalar,
        DetectedIsa::Neon => SimdWidth::W128,
        DetectedIsa::Avx2Fma => SimdWidth::W256,
        DetectedIsa::Avx512 => SimdWidth::W512,
    };

    let input = DispatchInput::for_single_query(8, false).expect("valid input");
    assert_eq!(
        select_execution_path(input),
        Ok(ExecutionPath::CpuSimd {
            width: expected_width
        })
    );
}

// ---------- 決定的擬似乱数（xorshift64*。`tests/hybrid_recall.rs` 等と同一実装。外部クレート不使用） ----------
struct XorShift64Star(u64);

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        XorShift64Star(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn next_f32(&mut self) -> f32 {
        // [-1.0, 1.0) の範囲へ写像する（内積の値域を過度に偏らせないため）。
        let bits = (self.next_u64() >> 40) as u32; // 24 bit
        (bits as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    }
}

fn random_vec(rng: &mut XorShift64Star, dim: usize) -> Vec<f32> {
    (0..dim).map(|_| rng.next_f32()).collect()
}

/// `isa::current().dot` がスカラー参照実装 [`isa::dot_scalar`] と許容差内で一致する
/// こと。決定的シード RNG で複数次元（0 を含む）を走査する。
#[test]
fn dispatched_dot_matches_scalar_reference_within_tolerance() {
    let dims = [0usize, 1, 3, 4, 7, 8, 15, 16, 17, 33, 768, 1000];
    let mut rng = XorShift64Star::new(0x1234_5678_9abc_def1);

    for &dim in &dims {
        let a = random_vec(&mut rng, dim);
        let b = random_vec(&mut rng, dim);

        let expected = isa::dot_scalar(&a, &b);
        let actual = isa::current().dot(&a, &b);

        let magnitude: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x * y).abs()).sum();
        let tolerance = 1e-5 * magnitude + 1e-6;
        assert!(
            (actual - expected).abs() <= tolerance,
            "dim={dim} actual={actual} expected={expected} tolerance={tolerance}"
        );
    }

    // 整数値ベクトルでは丸め誤差が生じないため完全一致することを確認する
    // （FMA・レーン分割があっても整数演算は正確に表現できる範囲内で一致するはず）。
    let a: Vec<f32> = (0..64).map(|i| (i % 7) as f32).collect();
    let b: Vec<f32> = (0..64).map(|i| (i % 5) as f32).collect();
    assert_eq!(isa::current().dot(&a, &b), isa::dot_scalar(&a, &b));
}

/// 長さ不一致・空スライスで [`isa::dot_scalar`] と同一の意味論（短い方への切り詰め）に
/// なること。
#[test]
fn dispatched_dot_length_mismatch_matches_scalar_semantics() {
    let a = vec![1.0f32, 2.0, 3.0, 4.0];
    let b = vec![5.0f32, 6.0];

    assert_eq!(isa::current().dot(&a, &b), isa::dot_scalar(&a, &b));
    assert_eq!(isa::current().dot(&[], &a), isa::dot_scalar(&[], &a));
    assert_eq!(isa::current().dot(&[] as &[f32], &[] as &[f32]), 0.0f32);
}

/// CORE-14: 検出結果への外部入力上書き機構（環境変数・設定ファイル読み取り等）が
/// ソース上に存在しないことを確認する（`tests/dispatch.rs::
/// dispatch_source_has_no_external_override_entry_points` と同じ禁止トークン集合）。
#[test]
fn isa_source_has_no_external_override_entry_points() {
    let source = include_str!("../src/isa.rs");

    let forbidden_tokens = [
        "std::env",
        "env::var",
        "env::var_os",
        "std::fs",
        "read_to_string",
        "File::open",
        "option_env!",
    ];

    for token in forbidden_tokens {
        assert!(
            !source.contains(token),
            "isa.rs must not contain external override entry point token: {token}"
        );
    }
}

/// `unsafe` が `isa.rs` 以外の `crates/engine/src/**/*.rs` に存在しないこと、
/// `isa.rs` 内の各 `unsafe {` 直前数行に `SAFETY:` があることをソース走査で確認する
/// （AGENTS.md P1「`unsafe` の立証」。sealed トークン所持を根拠とする 2 箇所
/// （AVX2+FMA・AVX-512）以外に `unsafe` を増やさないための回帰）。
#[test]
fn unsafe_is_confined_to_isa_module_with_safety_comments() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut rs_files = Vec::new();
    collect_rs_files(&src_dir, &mut rs_files);
    assert!(!rs_files.is_empty(), "no .rs files found under src/");

    for path in &rs_files {
        let content = std::fs::read_to_string(path).expect("read source file");
        let is_isa_module = path.file_name().and_then(|n| n.to_str()) == Some("isa.rs");

        if !is_isa_module {
            assert!(
                !content.contains("unsafe "),
                "unsafe must be confined to isa.rs, found a token in {}",
                path.display()
            );
            assert!(
                !content.contains("unsafe{"),
                "unsafe must be confined to isa.rs, found a token in {}",
                path.display()
            );
        }
    }

    let isa_path = src_dir.join("isa.rs");
    let isa_source = std::fs::read_to_string(&isa_path).expect("read isa.rs");
    let lines: Vec<&str> = isa_source.lines().collect();
    let mut unsafe_block_count = 0usize;

    for (idx, line) in lines.iter().enumerate() {
        if line.contains("unsafe {") {
            unsafe_block_count += 1;
            // 直前 10 行以内に `SAFETY:` があることを確認する。
            let start = idx.saturating_sub(10);
            let has_safety_comment = lines[start..idx].iter().any(|l| l.contains("SAFETY:"));
            assert!(
                has_safety_comment,
                "unsafe block at isa.rs line {} has no preceding SAFETY: comment",
                idx + 1
            );
        }
    }

    // ソーステキスト上には NEON・AVX2+FMA・AVX-512 の 3 箇所の `unsafe {` が
    // 現れる（実際のビルドで有効になるのは対象 arch の分岐のみだが、`cfg` 行は
    // ソース上に残ったまま走査されるため、arch に依存せず常に 3 を期待できる）。
    assert_eq!(
        unsafe_block_count, 3,
        "expected exactly 3 `unsafe {{` blocks in isa.rs (Neon, Avx2Fma, Avx512 dot dispatch)"
    );
}

fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// トークン型（`NeonToken`／`Avx2FmaToken`／`Avx512Token`）に `pub fn new`／
/// `pub fn try_new` が存在せず、`(())` 形式（unit struct・単一 private フィールド）で
/// あることをソース走査で確認する（sealed 方針の回帰。crate 外から任意のトークンを
/// 構築できないことの構造的な担保）。
#[test]
fn token_types_have_no_public_constructor() {
    let source = include_str!("../src/isa.rs");

    assert!(
        !source.contains("pub fn new"),
        "isa.rs must not expose a public constructor for token types"
    );
    assert!(
        !source.contains("pub fn try_new"),
        "isa.rs must not expose a public try_new constructor for token types"
    );

    for token in ["NeonToken", "Avx2FmaToken", "Avx512Token"] {
        assert!(
            source.contains(&format!("struct {token}(())")),
            "{token} must be defined as a unit-field tuple struct `{token}(())`"
        );
    }
}
