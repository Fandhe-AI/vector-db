//! SQL 表層の評価順序規則・`HINT ORDER(...)`（TASK-76・SQL-7 参照。
//! `docs/spec/05-tasks.md` TASK-76・`docs/spec/04-behavior/sql-surface.md` SQL-7・
//! `docs/spec/04-behavior/rls.md` RLS-5・RLS-7・RLS-8）。
//!
//! 責務境界: [`allowlist`](crate::sql::allowlist) が構造検証で受理した `HINT ORDER(...)`
//! （RLS・SCALAR・DISTANCE の順列）を、実行不能な状態を型で作れない
//! [`EvaluationOrder`] へ変換する。[`exec`](crate::sql::exec) はこの値から
//! [`ExecutionPlan`] を導出して実行順序を決める。
//!
//! **P0（テナント境界）**: `HINT ORDER` は RLS を後段に置くことを許すが、
//! 候補集合構築時の暗黙 RLS 事前フィルタ（`exec.rs` の
//! `VectorArena::build_filtered_with_rows_in_txn` の `predicate`）は
//! `HINT ORDER` の内容にかかわらず常に適用され続ける（本モジュールはこの事前
//! フィルタの適用有無には関与しない）。`HINT ORDER` は RLS 段の実行位置の見かけ上の
//! 記述に過ぎず、RLS の適用そのものを外す・弱める手段にはならない
//! （security.md「fail-open にする変更は P0」）。
//!
//! 加えて、`exec.rs` は最終結果に対して [`crate::rls::RlsSafetyNet`]（TASK-136・
//! RLS-5）を無条件に適用する。現状はこの事前フィルタと同じ `arena`
//! （`ctx.is_visible` を通過済みの候補集合）由来のテナント・可視性ラベルで
//! 再判定するため、今この経路単体では不可視行を追加で落とすことはない
//! （事前フィルタが唯一の実効的な防御線）。それでも安全網を `Option` や条件分岐で
//! 無効化できない構造にしておくのは、候補集合の構築元が将来広がった場合
//! （例: 事前フィルタを経由しない別経路の追加）に、その時点の `exec.rs` 側の
//! 変更漏れだけで RLS 違反が起こらないようにする構造的な歯止め
//! （defense-in-depth）としてであり、「今の実行経路で 2 つの独立した検査が
//! 効いている」という主張ではない。安全網の実装・witness 型（通過済み hits
//! だけを持ち出せる型）は `rls.rs` 側の管轄（TASK-136 で `sql::plan` から
//! 再配置）で、本モジュールはそれを分岐させる真偽値を [`ExecutionPlan`] に
//! 持たせない設計だけを担う。
//!
//! [`apply_rls_safety_net`] は TASK-136 で `crate::rls::RlsSafetyNet` へ実装を
//! 再配置した後も、main へマージ済みの公開 `pub fn` との互換性のために
//! `#[deprecated]` 付きで残す（PR #192 codex-review P1 対応）。呼び出し元
//! （`sql::exec`）はすでに `RlsSafetyNet` を直接使っており、本関数はワーク
//! スペース内では未参照。

/// `HINT ORDER(...)` が受理する評価段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Rls,
    Scalar,
    Distance,
}

/// untrusted な段名トークン（大文字小文字を区別しない）を [`Stage`] へ写像する。
/// 未知の名前は `None`（呼び出し元の allowlist 側で `42601` に写像する）。
pub fn parse_stage_name(name: &str) -> Option<Stage> {
    match name.to_ascii_uppercase().as_str() {
        "RLS" => Some(Stage::Rls),
        "SCALAR" => Some(Stage::Scalar),
        "DISTANCE" => Some(Stage::Distance),
        _ => None,
    }
}

/// `RLS`・`SCALAR`・`DISTANCE` を重複なく漏れなく並べた検証済み順列。
///
/// [`try_from_stages`](Self::try_from_stages) を通過した値だけが存在できるため、
/// 実行側（[`ExecutionPlan::from_evaluation_order`]）は「段の省略・重複・未知の段」を
/// 考慮せずに済む（許可リスト層の再検証を実行層で重複させない設計）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationOrder([Stage; 3]);

impl EvaluationOrder {
    /// TASK-75 時点の固定順序（`HINT ORDER` 未指定時の既定値）。
    pub const DEFAULT: EvaluationOrder =
        EvaluationOrder([Stage::Rls, Stage::Scalar, Stage::Distance]);

    /// `stages` が `RLS`・`SCALAR`・`DISTANCE` の順列（3 要素・重複なし）であることを
    /// 検証してから構築する。それ以外（個数不正・重複・欠落）は `Err`。
    pub fn try_from_stages(stages: &[Stage]) -> Result<Self, PlanError> {
        let [a, b, c] = <[Stage; 3]>::try_from(stages)
            .map_err(|_| PlanError::WrongStageCount { got: stages.len() })?;
        let has_rls = matches!(a, Stage::Rls) || matches!(b, Stage::Rls) || matches!(c, Stage::Rls);
        let has_scalar =
            matches!(a, Stage::Scalar) || matches!(b, Stage::Scalar) || matches!(c, Stage::Scalar);
        let has_distance = matches!(a, Stage::Distance)
            || matches!(b, Stage::Distance)
            || matches!(c, Stage::Distance);
        if !(has_rls && has_scalar && has_distance) {
            return Err(PlanError::NotAPermutation);
        }
        Ok(EvaluationOrder([a, b, c]))
    }

    pub fn stages(&self) -> [Stage; 3] {
        self.0
    }

    /// SCALAR 段が DISTANCE 段より先に評価されるか。`exec.rs` が事前適用
    /// （`on_visible_row` でのメタデータフィルタ突合。TASK-147・EXT-3）と事後適用
    /// （DISTANCE 段後のフィルタ）の
    /// どちらを使うかを決める唯一の分岐点。
    pub fn scalar_before_distance(&self) -> bool {
        let scalar_pos = self.0.iter().position(|s| matches!(s, Stage::Scalar));
        let distance_pos = self.0.iter().position(|s| matches!(s, Stage::Distance));
        matches!((scalar_pos, distance_pos), (Some(s), Some(d)) if s < d)
    }
}

impl Default for EvaluationOrder {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// [`EvaluationOrder::try_from_stages`] の失敗理由。呼び出し元
/// （`sql::allowlist::Parser::parse_hint_order`）が `SqlSurfaceError::unsupported`
/// （`42601`）へ写像する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// 3 個以外の個数（省略・重複起因の水増し・不足）。
    WrongStageCount { got: usize },
    /// 個数は 3 だが `RLS`・`SCALAR`・`DISTANCE` の順列になっていない（重複段）。
    NotAPermutation,
}

/// [`crate::sql::parser::BoundStatement`] から導出する、`exec.rs` が直接参照する
/// 実行方針。RLS 安全網（[`crate::rls::RlsSafetyNet`]）は `HINT ORDER` の内容に
/// 関わらず `exec.rs` が無条件に呼び出す契約であり、その適用有無を分岐させる
/// 真偽値はこの構造体に持たせない（boolean フィールドを経由させないことで
/// 「安全網を無効化できる `ExecutionPlan` を作れない」ことを型で保証する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionPlan {
    /// SCALAR 条件を候補構築時（`on_visible_row`）に事前適用するか。
    /// `false` の場合、DISTANCE 段の後に事後フィルタとして適用する。
    pub scalar_prefilter: bool,
}

impl ExecutionPlan {
    /// `sql::parser::BoundStatement::evaluation_order`（[`EvaluationOrder`]）から
    /// 導出する。呼び出し元（`exec.rs`）は `bound.evaluation_order` を渡す。
    pub fn from_evaluation_order(order: EvaluationOrder) -> Self {
        ExecutionPlan {
            scalar_prefilter: order.scalar_before_distance(),
        }
    }
}

/// RLS 実行時安全網（RLS-5）の旧 API（TASK-76）。TASK-136 で実装を
/// [`crate::rls::RlsSafetyNet`] へ再配置した後も、main へマージ済みの公開 API
/// との互換性のために残す（PR #192 codex-review P1 対応）。
///
/// `hits`（DISTANCE 段が返した `(id, score)` の順序付き列）から、
/// `is_visible(tenant_id, visibility)` が `false` を返す行、および
/// `tenant_id`/`visibility` を引けない行（データ不整合。fail-closed に除去）を除く。
/// `hits` の相対順序は保つ（`filter` ベースで安定。要素の並べ替えは行わない）。
///
/// `RlsSafetyNet::apply` へ内部委譲しない: `RlsSafetyNet` は判定ロジックを
/// `PolicyContext::is_visible` に固定することで「呼び出し元が判定を差し替えて
/// 検査を弱められない」契約を保証しており（`rls.rs` モジュールドキュメント参照）、
/// 本関数の `is_visible: F` のような任意の述語注入を許す口を新設すると、その
/// 契約自体を破ることになる（security.md P0「テナント境界の検査を外す/緩める
/// 経路を作らない」）。互換性のためだけに `RlsSafetyNet` 側の設計を緩めるのは
/// 本末転倒のため、本関数は旧実装をそのまま保持し、
/// `sql::plan::tests::deprecated_wrapper_matches_rls_safety_net` で
/// `RlsSafetyNet::apply` と同一入力に対し同一結果になることをテストで固定する。
#[deprecated(note = "use crate::rls::RlsSafetyNet instead")]
pub fn apply_rls_safety_net<F>(
    hits: Vec<(u64, f64)>,
    tenant_and_visibility: impl Fn(u64) -> Option<(String, crate::storage::Visibility)>,
    is_visible: F,
) -> Vec<(u64, f64)>
where
    F: Fn(&str, crate::storage::Visibility) -> bool,
{
    hits.into_iter()
        .filter(|(id, _)| match tenant_and_visibility(*id) {
            Some((tenant, visibility)) => is_visible(&tenant, visibility),
            None => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Stage / parse_stage_name ------------------------------------------------

    #[test]
    fn parse_stage_name_is_case_insensitive() {
        assert_eq!(parse_stage_name("rls"), Some(Stage::Rls));
        assert_eq!(parse_stage_name("Rls"), Some(Stage::Rls));
        assert_eq!(parse_stage_name("SCALAR"), Some(Stage::Scalar));
        assert_eq!(parse_stage_name("distance"), Some(Stage::Distance));
    }

    #[test]
    fn parse_stage_name_rejects_unknown_name() {
        assert_eq!(parse_stage_name("HINT"), None);
        assert_eq!(parse_stage_name(""), None);
        assert_eq!(parse_stage_name("RL"), None);
    }

    // --- EvaluationOrder::try_from_stages（6 順列すべて受理） ---------------------

    #[test]
    fn accepts_all_six_permutations() {
        use Stage::*;
        let perms: [[Stage; 3]; 6] = [
            [Rls, Scalar, Distance],
            [Rls, Distance, Scalar],
            [Scalar, Rls, Distance],
            [Scalar, Distance, Rls],
            [Distance, Rls, Scalar],
            [Distance, Scalar, Rls],
        ];
        for perm in perms {
            let order = EvaluationOrder::try_from_stages(&perm)
                .expect("valid permutation must be accepted");
            assert_eq!(order.stages(), perm);
        }
    }

    #[test]
    fn rejects_wrong_stage_count() {
        assert!(matches!(
            EvaluationOrder::try_from_stages(&[Stage::Rls, Stage::Scalar]),
            Err(PlanError::WrongStageCount { got: 2 })
        ));
        assert!(matches!(
            EvaluationOrder::try_from_stages(&[
                Stage::Rls,
                Stage::Scalar,
                Stage::Distance,
                Stage::Rls
            ]),
            Err(PlanError::WrongStageCount { got: 4 })
        ));
        assert!(matches!(
            EvaluationOrder::try_from_stages(&[]),
            Err(PlanError::WrongStageCount { got: 0 })
        ));
    }

    #[test]
    fn rejects_duplicate_stage() {
        assert_eq!(
            EvaluationOrder::try_from_stages(&[Stage::Rls, Stage::Rls, Stage::Scalar]),
            Err(PlanError::NotAPermutation)
        );
        assert_eq!(
            EvaluationOrder::try_from_stages(&[Stage::Distance, Stage::Distance, Stage::Distance]),
            Err(PlanError::NotAPermutation)
        );
    }

    #[test]
    fn default_order_matches_task_75_fixed_order() {
        assert_eq!(
            EvaluationOrder::DEFAULT.stages(),
            [Stage::Rls, Stage::Scalar, Stage::Distance]
        );
        assert_eq!(EvaluationOrder::default(), EvaluationOrder::DEFAULT);
    }

    // --- scalar_before_distance ----------------------------------------------------

    #[test]
    fn scalar_before_distance_is_true_for_default_and_scalar_leading_orders() {
        use Stage::*;
        for perm in [
            [Rls, Scalar, Distance],
            [Scalar, Rls, Distance],
            [Scalar, Distance, Rls],
        ] {
            let order = EvaluationOrder::try_from_stages(&perm).unwrap();
            assert!(order.scalar_before_distance(), "perm={perm:?}");
        }
    }

    #[test]
    fn scalar_before_distance_is_false_for_distance_leading_orders() {
        use Stage::*;
        for perm in [
            [Distance, Rls, Scalar],
            [Distance, Scalar, Rls],
            [Rls, Distance, Scalar],
        ] {
            let order = EvaluationOrder::try_from_stages(&perm).unwrap();
            assert!(!order.scalar_before_distance(), "perm={perm:?}");
        }
    }

    // --- ExecutionPlan ---------------------------------------------------------------

    #[test]
    fn execution_plan_scalar_prefilter_follows_scalar_before_distance() {
        let order = EvaluationOrder::try_from_stages(&[Stage::Scalar, Stage::Rls, Stage::Distance])
            .unwrap();
        assert!(ExecutionPlan::from_evaluation_order(order).scalar_prefilter);

        let order = EvaluationOrder::try_from_stages(&[Stage::Distance, Stage::Scalar, Stage::Rls])
            .unwrap();
        assert!(!ExecutionPlan::from_evaluation_order(order).scalar_prefilter);
    }

    // --- 旧 API 互換（apply_rls_safety_net）---------------------------------------

    /// PR #192 codex-review P1 対応: `#[deprecated]` で復元した旧 API
    /// `apply_rls_safety_net` が、現行実装 `crate::rls::RlsSafetyNet::apply` と
    /// 同一入力に対し同一結果（生き残る id・順序・除去件数）を返すことを固定する。
    /// テナント不一致による除去（fail-closed の可視性判定）と、ラベルを引けない
    /// id の除去（`None` 分岐。データ不整合時の fail-closed）の両方を含める
    /// （どちらも通す自明なケースだけでは互換性の証明にならないため）。
    #[test]
    #[allow(deprecated)]
    fn deprecated_wrapper_matches_rls_safety_net() {
        use crate::policy::PolicyContext;
        use crate::rls::RlsSafetyNet;
        use crate::storage::Visibility;
        use std::collections::HashMap;

        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");

        // id 1: 同一テナント Public（可視）。
        // id 2: 他テナント Private（不可視・テナント不一致）。
        // id 3: 同一テナント Private（可視）。
        // id 4: ラベルなし（データ不整合。fail-closed に除去）。
        let mut labels: HashMap<u64, (String, Visibility)> = HashMap::new();
        labels.insert(1, ("tenant-a".to_string(), Visibility::Public));
        labels.insert(2, ("tenant-b".to_string(), Visibility::Private));
        labels.insert(3, ("tenant-a".to_string(), Visibility::Private));

        let hits: Vec<(u64, f64)> = vec![(1, 0.1), (2, 0.2), (3, 0.3), (4, 0.4)];

        let old_result = apply_rls_safety_net(
            hits.clone(),
            |id| labels.get(&id).cloned(),
            |tenant, visibility| ctx.is_visible(tenant, visibility),
        );

        let net = RlsSafetyNet::new(&ctx);
        let verified = net.apply(hits, |id| {
            labels
                .get(&id)
                .map(|(tenant, visibility)| (tenant.as_str(), *visibility))
        });

        assert_eq!(
            old_result,
            verified.hits().to_vec(),
            "deprecated apply_rls_safety_net must match RlsSafetyNet::apply"
        );
        assert_eq!(old_result, vec![(1, 0.1), (3, 0.3)]);
        assert_eq!(
            verified.dropped(),
            2,
            "id 2 (tenant mismatch) and id 4 (no label) must be dropped"
        );
    }
}
