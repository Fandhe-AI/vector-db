//! 検索エンジンの選択・構築レイヤ（TASK-131・対象ビヘイビア: CORE-9）。
//!
//! `core.rs` の [`crate::core::EngineCore::open`] から呼ばれ、`Box<dyn SearchProvider>`
//! （`kernel.rs::SearchProvider`、CORE-13）を構築して返す。CORE-9 が指す「総当たり実装を
//! 差し替え可能なインターフェース越しに呼び出す」という差し替え点は、独立の trait 階層を
//! 新設せず CORE-13 の provider 注入機構（`SearchProvider` trait・TASK-124 実装済み）へ
//! 一本化する。将来の ANN 実装は本モジュールの [`SearchEngineKind`] に選択肢を追加し、
//! 新しい `SearchProvider` 実装を返す分岐を [`build`] に加えるだけで、`core.rs` 側の
//! コア API（`EngineCore`／`VectorCore`）を変更せずに追加できる。
//!
//! エンジンの選択はコード上の明示指定（[`SearchEngineKind`] の値）のみで決まる。
//! 環境変数・設定ファイルによる実行時の経路上書き機構は設けない（`kernel.rs` に明記の
//! 方針を踏襲。CORE-12）。CPU-SIMD／GPU の実行経路自体を決める決定表は
//! `dispatch.rs::select_execution_path`（TASK-155・CORE-11, 12）が担う。本モジュールが
//! 構築する provider（[`SearchEngineKind`]）はいずれも CPU 実装のため、
//! `core.rs::EngineCore::search` は provider を実行する前に `select_execution_path` を
//! 呼び、`ExecutionPath::CpuSimd` の場合だけ本モジュールの provider を実行する
//! （`ExecutionPath::Gpu` は fail-closed に拒否する。詳細は `core.rs::EngineCore::search`
//! の実装、`dispatch.rs` モジュールドキュメント参照）。SIMD 幅ごとに異なる provider を
//! 構築する配線（[`SearchEngineKind`] への variant 追加を伴う）は TASK-156・CORE-14 の
//! 管轄。
//!
//! # ANN（HNSW）の opt-in 結線（Issue #407・ADR `docs/design/ann-index-adoption.md` B 案）
//!
//! [`SearchEngineKind::Hnsw`] は #404〜#406 で実装した `hnsw.rs::HnswIndex` を選ぶ
//! variant で、`core.rs::EngineCore::open_with_engine`／`from_storage_with_engine`
//! （本 Issue で追加）を明示的に呼んだ場合のみ選択される opt-in 経路であり、
//! [`default_kind`]／[`default_engine`]（既定は不変。[`SearchEngineKind::ParallelBruteForce`]）
//! には影響しない。テーブル単位カタログ属性・wire-server CLI からの露出は対象外
//! （ADR「判断確定後のスコープ外」節、および本 Issue のスコープ外事項）。
//!
//! 本 Issue 時点では [`hnsw::provider::HnswSearchProvider`](crate::hnsw::provider::HnswSearchProvider)
//! は索引を構築・保持せず、`ParallelSearchProvider` へ全件フォールバックする
//! （世代整合キャッシュが無く索引済み集合と `SearchInput` の差分を判定できないため。
//! 索引の実利用は #408 の担当。詳細は `hnsw/provider.rs` モジュールドキュメント参照）。
//!
//! 不正な `HnswParams`（[`crate::hnsw::HnswParams::validate`] が拒否する値）は
//! `SearchEngineKind::Hnsw` 自体には現れない（`open_with_engine` 等の呼び出し時点で
//! [`SearchEngineError`] として fail-closed に拒否され、`EngineCore` は構築されない）。
//! この不変条件は本モジュールの [`build_validated`] だけでなく、`crate::hnsw` が
//! 公開モジュール（`lib.rs::pub mod hnsw`）であるために本モジュールを経由せず直接
//! 到達しうる [`crate::hnsw::provider::HnswSearchProvider::new`] 自身の検証でも
//! 二重に維持する（codex-review P1 指摘・Issue #407 追記。`hnsw/provider.rs`
//! モジュールドキュメント参照）。infallible な [`build`] 自体は `pub(crate)` に留め、
//! 外部呼び出し元は必ず検証を行う [`build_validated`] を経由する。
//!
//! ## 破壊的変更（`build` の公開性・codex-review P1 指摘・Issue #407 追記）
//!
//! `build` は本 Issue 以前（`main`）では `pub fn` だった。`SearchEngineKind::Hnsw`
//! 追加に伴い `pub(crate)` へ縮小したのは意図的な破壊的変更であり、互換ラッパーとして
//! 旧シグネチャの `pub fn build` を残す選択肢は採らない: `build` は infallible な
//! 契約（呼び出し元が事前検証済みの値を渡す前提）を保つ設計であり、`pub` のまま
//! 残すと外部呼び出し元が未検証の `HnswParams` を直接 `SearchEngineKind::Hnsw` へ
//! 詰めて渡せてしまい、`HnswSearchProvider::new` の検証を経ない構築経路（かつては
//! `.expect(...)` によるパニックへ帰結しうる経路だった）を公開 API として残すことに
//! なる。ライブラリ利用側の untrusted な設定値がプロセスパニックへ直結する互換ラッパーは
//! `.claude/rules/coding-rust.md`（`unwrap`/`expect` を受信データ経路で禁止する方針）の
//! 精神に反するため、`build` を非公開化し `Result` を返す [`build_validated`] のみを
//! crate 外 API として残す方を選んだ。この破壊的変更は本 Issue（#407）のスコープ内
//! （既存 spec ビヘイビア ID に対応する変更ではなく、本 Issue で新設する opt-in API の
//! 一部）であり、`docs/design/hnsw-search-engine-wiring.md`「変更履歴」節に対応する
//! 記録を残す。

use crate::hnsw::provider::HnswSearchProvider;
use crate::hnsw::HnswParams;
use crate::kernel::{CpuScalarProvider, SearchProvider};
use crate::parallel_search::ParallelSearchProvider;
use std::fmt;

/// 選択可能な検索エンジン。
///
/// 各 variant はいずれも `kernel.rs::SearchProvider` の実装を返す（CORE-13 の
/// 単一 trait 階層への一本化。本モジュールは新規 trait を定義しない）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngineKind {
    /// 単一スレッド・スカラー演算の参照実装（[`crate::kernel::CpuScalarProvider`]）。
    /// 他 provider の正解値検証用途が主で、既定エンジンではない。
    CpuScalarBruteForce,
    /// マルチスレッド並列の総当たり Top-k（[`crate::parallel_search::ParallelSearchProvider`]、
    /// TASK-126）。既定エンジン（[`default_engine`] 参照）。
    ParallelBruteForce,
    /// HNSW 近似最近傍探索（`hnsw.rs::HnswIndex`、Issue #403 ADR B 案）。opt-in
    /// （モジュールドキュメント「ANN（HNSW）の opt-in 結線」節参照）。保持する
    /// `HnswParams` は構築前に [`SearchEngineError`] として検証済み（このモジュールの
    /// 呼び出し元、[`build_validated`] を経由する限り不正値は到達しない。
    /// `crate::hnsw::provider::HnswSearchProvider::new` 自身も同じ検証を行う二重防御に
    /// ついてはモジュールドキュメント参照）。
    Hnsw(HnswParams),
}

/// `SearchEngineKind::Hnsw` の構築が失敗した理由。
///
/// TASK-152・ERR-2 の分類リストへの新規登録・`wire_code()` の公開は行わない
/// （codex-review P1 指摘・Issue #407 追記）: 本 variant を SQLSTATE 風コードで
/// 表すなら `22023` が字面上は近いが、その値は既存分類
/// `error_format::ErrorClass::OperationIdContentMismatch`（TASK-101・RECOVER-10）が
/// 既に占有しており、流用すると ERR-2 の「分類 ⇔ `wire_code` 一意対応」契約
/// （`error_format.rs::wire_codes_are_pairwise_distinct`）が保証する一意性の外側で
/// コードが衝突する。本 Issue は wire／SQL 表層への露出を持たない
/// （モジュールドキュメント「ANN（HNSW）の opt-in 結線」節）ため、正式な
/// `ErrorClass` 登録・`wire_code()` の追加は spec 側のビヘイビア ID 確定後の
/// 別タスクへ申し送る（`docs/design/hnsw-search-engine-wiring.md` 参照）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEngineError {
    /// [`HnswParams::validate`] が拒否した。
    InvalidHnswParams(crate::hnsw::HnswError),
}

impl fmt::Display for SearchEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchEngineError::InvalidHnswParams(err) => {
                write!(f, "invalid HNSW search engine params: {err}")
            }
        }
    }
}

impl std::error::Error for SearchEngineError {}

impl fmt::Display for SearchEngineKind {
    /// 診断・`EXPLAIN`（#411 の担当。本 Issue は表示専用の生成元のみ用意する）向けの
    /// 人間可読表現。`FromStr` の対は本 Issue 時点で呼び出し元が存在しないため設けない
    /// （untrusted 文字列パーサを使う先が無いまま追加しない）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchEngineKind::CpuScalarBruteForce => write!(f, "cpu_scalar_brute_force"),
            SearchEngineKind::ParallelBruteForce => write!(f, "parallel_brute_force"),
            SearchEngineKind::Hnsw(params) => write!(
                f,
                "hnsw(m={},ef_construction={},ef_search={})",
                params.m, params.ef_construction, params.ef_search
            ),
        }
    }
}

/// `kind` に対応する `SearchProvider` 実装を構築する（crate 内部専用）。
///
/// `Hnsw` の `HnswParams` は事前検証済みであることを呼び出し元が保証する契約
/// （本関数は infallible）。この契約を型で強制するため公開 API からは外し
/// `pub(crate)` に限定する（codex-review P1 指摘・Issue #407 追記: 本関数が
/// `pub` のままだと、`SearchEngineKind::Hnsw(HnswParams)` も `pub` である以上、
/// 外部 crate が未検証の `HnswParams` を直接 `build` へ渡して不正値を保持した
/// provider を構築できてしまい、モジュールドキュメントの「不正値は到達しない」
/// 契約と矛盾する）。untrusted な文字列・設定値から `SearchEngineKind::Hnsw` を
/// 組み立てる経路（`core.rs::EngineCore::open_with_engine` 等）を含め、crate 外から
/// provider を構築する唯一の経路は必ず検証を行う [`build_validated`] とする。
///
/// 呼び出し元（`core.rs::EngineCore::open` 等）はここで返る `Box<dyn SearchProvider>` を
/// そのまま `EngineCore::with_provider` へ渡す想定（object-safe な trait のため
/// ジェネリクスなしで受け渡しできる）。
fn build(kind: SearchEngineKind) -> Box<dyn SearchProvider> {
    match kind {
        SearchEngineKind::CpuScalarBruteForce => Box::new(CpuScalarProvider),
        SearchEngineKind::ParallelBruteForce => Box::new(ParallelSearchProvider),
        SearchEngineKind::Hnsw(params) => Box::new(HnswSearchProvider::new(params).expect(
            "build() の Hnsw 分岐は build_validated 経由でのみ到達し、その時点で params は検証済み",
        )),
    }
}

/// [`build`] の検証付き版。`kind` が `Hnsw(params)` の場合のみ
/// [`HnswParams::validate`] を通し、失敗を [`SearchEngineError`] として fail-closed に
/// 返す（`Hnsw` 以外の variant は現時点で検証すべきパラメータを持たないため常に成功）。
/// 非公開の [`build`] を crate 外から呼ぶ唯一の経路であり、`SearchEngineKind` から
/// `Box<dyn SearchProvider>` を得る公開 API はこの関数のみ（[`default_engine`] は
/// 常に検証を要さない [`default_kind`] を経由するため対象外）。
///
/// `core.rs::EngineCore::open_with_engine`／`from_storage_with_engine`
/// （Issue #407 で追加）が唯一の呼び出し元で、不正な `HnswParams` を持つ
/// `EngineCore` が構築される経路を構造的に無くす。
pub fn build_validated(
    kind: SearchEngineKind,
) -> Result<Box<dyn SearchProvider>, SearchEngineError> {
    if let SearchEngineKind::Hnsw(params) = kind {
        params
            .validate()
            .map_err(SearchEngineError::InvalidHnswParams)?;
    }
    Ok(build(kind))
}

/// 既定の検索エンジン種別（`ParallelBruteForce`）。[`EngineCore::open`]
/// （`core.rs`）・[`default_engine`] が参照する唯一の既定値の源泉であり、
/// 既定エンジンを固定するテストはこの関数の戻り値を検証する。
pub fn default_kind() -> SearchEngineKind {
    SearchEngineKind::ParallelBruteForce
}

/// 既定の検索エンジンを構築する（`EngineCore::open` から呼ばれる既定経路）。
///
/// 現時点の既定は [`default_kind`]（[`SearchEngineKind::ParallelBruteForce`]、
/// マルチスレッド並列総当たり）。挙動は `EngineCore::open` が従来
/// `ParallelSearchProvider` を直接生成していたときと同一で、性能・結果の回帰は
/// 発生しない。
pub fn default_engine() -> Box<dyn SearchProvider> {
    build(default_kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    // object-safety の固定（CORE-13 の一本化要件）: build/default_engine の戻り値型が
    // `Box<dyn SearchProvider>` であることをコンパイル時代入で固定する。
    #[test]
    fn build_and_default_return_boxed_search_provider() {
        let _cpu: Box<dyn SearchProvider> = build(SearchEngineKind::CpuScalarBruteForce);
        let _parallel: Box<dyn SearchProvider> = build(SearchEngineKind::ParallelBruteForce);
        let _hnsw: Box<dyn SearchProvider> = build(SearchEngineKind::Hnsw(HnswParams::default()));
        let _default: Box<dyn SearchProvider> = default_engine();
    }

    // 既定エンジン不変（受け入れ条件 (a) の型レベル部分）。
    #[test]
    fn default_kind_is_parallel_brute_force() {
        assert_eq!(default_kind(), SearchEngineKind::ParallelBruteForce);
    }

    #[test]
    fn hnsw_default_params_pass_validation() {
        assert!(HnswParams::default().validate().is_ok());
        assert!(build_validated(SearchEngineKind::Hnsw(HnswParams::default())).is_ok());
    }

    #[test]
    fn build_validated_rejects_invalid_hnsw_params() {
        let invalid = HnswParams {
            m: 1, // HnswParams::validate は m < 2 を拒否する
            ..HnswParams::default()
        };
        // `Box<dyn SearchProvider>` は `Debug` を実装しないため `expect_err` は使えず、
        // `match` で `Err` 側だけを取り出す。
        let err = match build_validated(SearchEngineKind::Hnsw(invalid)) {
            Ok(_) => panic!("m=1 must be rejected"),
            Err(e) => e,
        };
        assert!(matches!(err, SearchEngineError::InvalidHnswParams(_)));
    }

    // `build`（crate 内部専用）に直接 Hnsw(invalid_params) を渡すコードは crate 内に
    // 存在しない・存在させないという不変条件を、`build_validated` を経由した場合のみ
    // 検証が効くことの確認で固定する（codex-review P1 指摘・Issue #407 追記）。
    #[test]
    fn hnsw_provider_new_rejects_invalid_params_directly() {
        let invalid = HnswParams {
            m: 1,
            ..HnswParams::default()
        };
        assert!(HnswSearchProvider::new(invalid).is_err());
    }

    #[test]
    fn build_validated_never_validates_non_hnsw_kinds() {
        // CpuScalarBruteForce / ParallelBruteForce は検証すべきパラメータを持たず常に成功。
        assert!(build_validated(SearchEngineKind::CpuScalarBruteForce).is_ok());
        assert!(build_validated(SearchEngineKind::ParallelBruteForce).is_ok());
    }

    #[test]
    fn display_hnsw_includes_params() {
        let kind = SearchEngineKind::Hnsw(HnswParams {
            m: 32,
            ef_construction: 200,
            ef_search: 128,
        });
        assert_eq!(
            kind.to_string(),
            "hnsw(m=32,ef_construction=200,ef_search=128)"
        );
        assert_eq!(
            SearchEngineKind::ParallelBruteForce.to_string(),
            "parallel_brute_force"
        );
        assert_eq!(
            SearchEngineKind::CpuScalarBruteForce.to_string(),
            "cpu_scalar_brute_force"
        );
    }
}
