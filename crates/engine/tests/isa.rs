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

/// [`isa::SimdKernel::dot_with_scalar_tail`]（現行のスカラー逐次和 tail）と
/// [`isa::SimdKernel::dot_with_padded_tail`]（零埋め固定長バッファによる分岐なし
/// tail、Issue #528）が dim 0..=129 の全長でビット同一であること。あわせて
/// [`isa::SimdKernel::dot`]（既定経路）が `dot_with_scalar_tail` と一致すること
/// （`DEFAULT_PADDED_TAIL == false` の配線回帰）も確認する。`isa::current().isa()`
/// を assert メッセージへ含め、どの ISA で検証されたかを判別可能にする
/// （実機の AVX2/AVX-512/NEON 対応有無はテスト実行環境依存のため）。
#[test]
fn branchless_tail_matches_scalar_tail_bit_exact_across_dims() {
    let current_isa = isa::current().isa();
    let mut rng = XorShift64Star::new(0x0fed_cba9_8765_4321);

    for dim in 0..=129usize {
        let a = random_vec(&mut rng, dim);
        let b = random_vec(&mut rng, dim);

        let scalar_tail = isa::current().dot_with_scalar_tail(&a, &b);
        let padded_tail = isa::current().dot_with_padded_tail(&a, &b);
        assert_eq!(
            scalar_tail.to_bits(),
            padded_tail.to_bits(),
            "isa={current_isa:?} dim={dim} scalar_tail={scalar_tail} padded_tail={padded_tail}"
        );

        let default_dot = isa::current().dot(&a, &b);
        assert_eq!(
            default_dot.to_bits(),
            scalar_tail.to_bits(),
            "isa={current_isa:?} dim={dim}: SimdKernel::dot must still use the scalar tail \
             (DEFAULT_PADDED_TAIL == false) as production behavior is unchanged by Issue #528"
        );
    }
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

/// `isa::SimdKernel::dot_block4`（Issue #510・TASK-156・CORE-14。行ブロック
/// カーネル）が 1 行版 [`isa::SimdKernel::dot`] とビット同一であることを、
/// 決定的シード RNG で dim 0..=129・768・1000・1536 を走査して検証する
/// （ポインタ: `docs/design/dot-kernel-row-block.md`）。符号付きゼロ・微小値
/// （`f32::MIN_POSITIVE` 近傍）を含む値集合もあわせて検証し、4 行それぞれで
/// 独立した丸め誤差が生じないこと（1 行版と完全に同じ縮約経路を通ること）を
/// 固定する。
#[test]
fn dot_block4_matches_single_row_dot_bit_exact_across_dims() {
    let current_isa = isa::current().isa();
    let mut rng = XorShift64Star::new(0x510a_bcde_f012_3456);

    let dims: Vec<usize> = (0..=129usize).chain([768, 1000, 1536]).collect();

    for &dim in &dims {
        let query = random_vec(&mut rng, dim);
        let r0 = random_vec(&mut rng, dim);
        let r1 = random_vec(&mut rng, dim);
        let r2 = random_vec(&mut rng, dim);
        let r3 = random_vec(&mut rng, dim);

        let expected = [
            isa::current().dot(&r0, &query),
            isa::current().dot(&r1, &query),
            isa::current().dot(&r2, &query),
            isa::current().dot(&r3, &query),
        ];
        let actual = isa::current().dot_block4([&r0, &r1, &r2, &r3], &query);

        for i in 0..4 {
            assert_eq!(
                actual[i].to_bits(),
                expected[i].to_bits(),
                "isa={current_isa:?} dim={dim} i={i} actual={} expected={}",
                actual[i],
                expected[i]
            );
        }
    }

    // 符号付きゼロ・微小値（subnormal 近傍）を含むエッジ値集合。
    let edge_values: Vec<f32> = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        f32::EPSILON,
        -f32::EPSILON,
        1e-30,
        -1e-30,
    ];
    let query = edge_values.clone();
    let r0 = edge_values.clone();
    let r1: Vec<f32> = edge_values.iter().rev().copied().collect();
    let r2 = edge_values.clone();
    let r3: Vec<f32> = edge_values.iter().map(|v| v * 2.0).collect();

    let expected = [
        isa::current().dot(&r0, &query),
        isa::current().dot(&r1, &query),
        isa::current().dot(&r2, &query),
        isa::current().dot(&r3, &query),
    ];
    let actual = isa::current().dot_block4([&r0, &r1, &r2, &r3], &query);
    for i in 0..4 {
        assert_eq!(
            actual[i].to_bits(),
            expected[i].to_bits(),
            "isa={current_isa:?} edge-values i={i} actual={} expected={}",
            actual[i],
            expected[i]
        );
    }
}

/// 4 行と `query` の長さが 1 つでも異なる場合、[`isa::SimdKernel::dot_block4`] が
/// 高速経路（intrinsics ブロックカーネル）へ入らず、1 行版 `dot` を 4 回呼ぶ
/// 縮退経路と一致すること（`isa.rs::SimdKernel::dot_block4_impl` の
/// `uniform_len` 判定の回帰。production では `parallel_search.rs::search_range`
/// が常に 4 行と `query` の長さを揃えて呼ぶため、この経路は主に安全側の
/// フォールバックとして機能する）。
#[test]
fn dot_block4_falls_back_to_single_row_dot_when_lengths_are_not_uniform() {
    let query = vec![1.0f32, 2.0, 3.0, 4.0];
    let r0 = vec![1.0f32, 0.0, 0.0, 0.0]; // query と同じ長さ
    let r1 = vec![1.0f32, 0.0, 0.0]; // 1 要素短い
    let r2 = vec![1.0f32, 0.0, 0.0, 0.0, 0.0]; // 1 要素長い
    let r3: Vec<f32> = Vec::new(); // 空

    let expected = [
        isa::current().dot(&r0, &query),
        isa::current().dot(&r1, &query),
        isa::current().dot(&r2, &query),
        isa::current().dot(&r3, &query),
    ];
    let actual = isa::current().dot_block4([&r0, &r1, &r2, &r3], &query);
    assert_eq!(actual, expected);
}

/// CORE-14: 検出結果への外部入力上書き機構（環境変数・設定ファイル読み取り等）が
/// ソース上に存在しないことを確認する（`tests/dispatch.rs::
/// dispatch_source_has_no_external_override_entry_points` と同じ禁止トークン集合）。
#[test]
fn isa_source_has_no_external_override_entry_points() {
    // Issue #510: `isa/x86_block4.rs`（cfg(x86_64) サブモジュール）も同じ禁止
    // トークン集合で走査する（`isa.rs` 本体からモジュール分割しても CORE-12
    // の「上書き機構の不存在」検査が抜け穴にならないようにするため）。
    let sources = [
        include_str!("../src/isa.rs"),
        include_str!("../src/isa/x86_block4.rs"),
    ];

    let forbidden_tokens = [
        "std::env",
        "env::var",
        "env::var_os",
        "std::fs",
        "read_to_string",
        "File::open",
        "option_env!",
    ];

    for source in sources {
        for token in forbidden_tokens {
            assert!(
                !source.contains(token),
                "isa module must not contain external override entry point token: {token}"
            );
        }
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
        // Issue #510: `isa/x86_block4.rs`（`isa.rs` の cfg(x86_64) サブモジュール）は
        // `unsafe` を持たない safe fn のみで構成する契約（ADR
        // `docs/design/simd-intrinsics-adoption.md` 決定 1）だが、それはこの検査を
        // 弱める理由にはならない。`unsafe` を許すのは sealed トークン所持を根拠に
        // 検証済みの `isa.rs` 本体のみとし、`isa/` 配下のサブモジュールへ `unsafe`
        // が紛れ込んだ場合はこの検査で検出できるよう除外範囲を `isa.rs` 単体に限定
        // する（codex-review 指摘対応。ディレクトリ一致による除外は
        // `isa/x86_block4.rs` への `unsafe` 追加を無検査で通してしまうため撤回）。
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

    // ソーステキスト上には NEON・AVX2+FMA・AVX-512 の `dot` ディスパッチ 3 箇所に加え、
    // Issue #510（TASK-156・CORE-14）で追加した `dot_block4` の AVX2+FMA・AVX-512
    // ディスパッチ 2 箇所の計 5 箇所の `unsafe {` が現れる（実際のビルドで有効に
    // なるのは対象 arch の分岐のみだが、`cfg` 行はソース上に残ったまま走査される
    // ため、arch に依存せず常に 5 を期待できる）。
    assert_eq!(
        unsafe_block_count, 5,
        "expected exactly 5 `unsafe {{` blocks in isa.rs (Neon/Avx2Fma/Avx512 dot dispatch \
         + Avx2Fma/Avx512 dot_block4 dispatch)"
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
