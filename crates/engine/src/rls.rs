//! 事前フィルタ方式によるテナント境界の再利用可能インデックス
//! （TASK-133・対象ビヘイビア: RLS-1, RLS-2, RLS-3, RLS-4）。
//!
//! `core.rs`（TASK-124）の `EngineCore::search` はクエリ 1 件ごとにアリーナを再構築するが、
//! 本モジュールは可視行の部分集合インデックスを構築して使い回し、後続クエリはその
//! 部分集合だけを総当たりスキャンする「事前フィルタ方式」を提供する。可視率・ポリシーが
//! 変わらない連続クエリ列（将来の SQL 実行経路・TASK-134 のフォールバック切り替え元を
//! 想定）でアリーナ再構築コストを避けるための構造体である。方式選定はポインタ:
//! `docs/spec/03-poc/rls-search-integration/`。
//!
//! テナント境界の判定は [`crate::policy::PolicyContext::is_visible`] の単一照合パスに
//! 限定し、本モジュール独自のテナント比較を新設しない（security.md P0）。
//! [`PrefilterIndex::build`] は構築時に渡された `PolicyContext` の可視性述語で
//! [`crate::arena::VectorArena::build_filtered`] を呼び、可視行だけを保持する縮約ビューを
//! 作る。以後の検索はこの構築時スナップショットのベクトル・id 集合に対してのみ行われる
//! （書き込みは検索対象の追加/除外としては反映されない）。ただし
//! [`PrefilterIndex::search`] は **provider を呼ぶ前** にアリーナが保持する全 id の現在の
//! 行状態（`tenant_id`・`visibility`）をストレージへ引き直して構築時の値と厳密に照合する
//! ため、構築後に update/delete で失効した行はスナップショットに残っていても provider へ
//! ベクトルが渡ることすらなく検索全体が拒否される（下記「失効行の全件事前検証」・
//! codex-review P0 指摘・PR #151 対応）。`PrefilterIndex` は構築時に束縛した
//! `PolicyContext` の複製（テナント ID・許可可視性集合）を保持し、
//! [`PrefilterIndex::search`] は呼び出し時に渡された `PolicyContext` とこの複製の完全一致
//! （`PartialEq`）を fail-closed に照合する。別テナント・可視性が狭化/拡大された ctx で
//! 同一インデックスを転用しようとした場合は [`RlsError::ContextMismatch`] で拒否する
//! （テナント境界 P0。codex-review P0 指摘・PR #151 対応: 以前は `search` が ctx を
//! 受け取らず、構築時 ctx との一致を検証していなかった）。
//!
//! **失効行の全件事前検証（codex-review P0 指摘・PR #151 対応）**: 構築時 ctx との一致
//! だけでは「インデックス構築後にストレージ側で該当行の tenant/visibility が変更・削除
//! された」ケースを検出できない（ctx 自体は変わらないため）。当初はヒット確定後にヒット
//! id だけを再検証していたが、これには 2 つの P0 があった: (1) provider は検証より前に
//! アリーナの全ベクトルを観測済みのため、事後拒否では provider（untrusted）が既に失効行の
//! ベクトルを見た事実を取り消せない、(2) ヘッダ取得とその後の可視性評価・結果返却の間に
//! 別の書き込みが挟まっても検出できない（TOCTOU）。[`PrefilterIndex::search`] は
//! provider を呼ぶ**前**に、[`crate::storage::Storage::get_row_headers_from_table`]
//! （1 回の呼び出しで単一の read トランザクション上に閉じる）でアリーナの**全 id**の
//! 現在の `tenant_id`・`visibility` を読み直し、構築時にアリーナへ格納した値と厳密に
//! 一致するか照合する。1 件でも不一致・不存在であれば provider を一切呼ばずに
//! [`RlsError::IndexStale`] で fail-closed に拒否する（呼び出し元へ
//! [`PrefilterIndex::build`] の再実行を要求する。テナント境界 P0）。設計判断の詳細・
//! 一貫性契約（返却結果がどの時点のスナップショットに対して一貫するか）は
//! [`PrefilterIndex::search`] のドキュメント参照。
//! なお、tenant/visibility は変わらず embedding 本体だけが更新された場合のスコアは
//! 依然として構築時点の値のまま返る（この再検証はテナント境界の失効検出が目的で、
//! embedding 鮮度の保証はスコープ外。完全な鮮度が必要な呼び出し元は `build` を
//! 呼び直すこと）。
//!
//! **再検証対象ストレージの束縛（codex-review P0 指摘・PR #151 対応）**: 上記の再検証は
//! 「[`PrefilterIndex::search`] へ渡された `Storage` が [`PrefilterIndex::build`] に
//! 使ったものと同一である」ことに依存する。以前は `search` が呼び出し時に任意の
//! `&Storage` を受け取っており、同名テーブル・同じヒット id・同じ `tenant_id`/
//! `visibility` を持つ**別の** `Storage`（別 DB ファイル）を渡すと、再検証をすり抜けて
//! その別ストレージ上のベクトル由来の id・スコアを返せてしまった（テナント境界 P0）。
//! `PrefilterIndex<'s>` は構築時に渡された `&'s Storage` をフィールドとして保持し、
//! `search` は引数から `storage` を取り除いて `self` が保持する参照のみを使う。
//! ストレージの同一性はランタイム比較ではなく借用チェッカーにより型レベルで保証され、
//! `search` に別インスタンスを渡すこと自体が不可能になる（`build` 後に別 `Storage` を
//! 渡す経路は構造的に存在しない）。テーブルの削除・再作成については、本クレートに
//! テーブル削除 API が現時点で存在しないため到達不能である（`catalog.rs` の
//! `user_rows_table_name` ドキュメント中の申し送り参照。将来 `drop_table` 相当を
//! 追加する実装者は、テーブル再作成後に古い `PrefilterIndex` を無効化する仕組みを
//! 同時に検討すること）。
//!
//! `core.rs::EngineCore`（`VectorCore::search`）への prefilter インデックスのキャッシュ
//! 統合・API 変更は本タスクのスコープ外（`VectorCore` trait のシグネチャは変更しない）。

use std::collections::HashSet;

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::CatalogError;
use crate::core::{provider_result_is_valid, validate_search_k, MAX_SEARCH_K};
use crate::kernel::{KernelError, SearchHit, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::storage::Storage;

/// [`PrefilterIndex`] のエラー型。`core.rs::CoreError` とおおむね対称の設計だが、
/// `Policy` は持たず（`PolicyContext` の構築時検証は呼び出し元の責務で本モジュールには
/// 到達しない）、[`RlsError::ContextMismatch`] は `core.rs` 側に対応がない
/// （`EngineCore::search` はクエリ毎にアリーナを再構築するため構築時 ctx と検索時 ctx の
/// 食い違いという状態自体が存在しない。本モジュール特有のインデックス再利用に伴う
/// エラー種別）。
#[derive(Debug)]
pub enum RlsError {
    Arena(ArenaError),
    Kernel(KernelError),
    /// `k == 0` または [`MAX_SEARCH_K`] 超過。
    InvalidK {
        k: usize,
    },
    /// 指定テーブルが存在しない（`core.rs::CoreError::NotFound` と同一契約: 不可視と
    /// 不存在を区別しない。[`PrefilterIndex::build`] が `ArenaError::Catalog`
    /// （`CatalogError::TableNotFound`）を捕捉してこの variant へ丸め込み、`Display` へ
    /// テーブル名を含めない。他テナントの存在情報を漏らさないため
    /// （security.md P0「エラー経由で存在情報を漏らさない」）。
    NotFound,
    /// [`PrefilterIndex::search`] に渡された `PolicyContext` が構築時に束縛した
    /// `PolicyContext` と一致しない（テナント ID・許可可視性集合のいずれかが異なる）。
    /// 別テナントへの転用・可視性の狭化/拡大のいずれも区別せず本 variant で fail-closed
    /// に拒否する（`Display` はテナント ID・可視性集合を含まない。テナント境界 P0・
    /// codex-review P0 指摘・PR #151 対応）。
    ContextMismatch,
    /// [`PrefilterIndex::search`] が provider を呼ぶ**前**にアリーナの全 id の現在の行状態を
    /// 再検証した結果、構築時点のスナップショットに残っている行のうち 1 件以上が検索時点
    /// では構築時と異なる状態（tenant/visibility の変更・行の削除）になっていた（テーブル
    /// 側の update/delete によるポリシー失効）。この検証は provider 呼び出しより前に完了
    /// するため、失効行のベクトルが provider（untrusted）へ渡ることはない。`Display` は
    /// id・テナント ID を含めない（他テナントの存在情報を漏らさないため。security.md P0）。
    /// 呼び出し元は [`PrefilterIndex::build`] を呼び直して再構築すること（codex-review P0
    /// 指摘・PR #151 対応）。
    IndexStale,
    /// `SearchProvider` が返却した `Vec<`[`SearchHit`]`>` が Top-k の契約に違反した
    /// （`core.rs::CoreError::ProviderResultRejected` と同一契約。判定は共有ヘルパ
    /// `provider_result_is_valid` で行う。fail-closed: 違反があれば結果を一切返さない）。
    ProviderResultRejected,
}

impl std::fmt::Display for RlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RlsError::Arena(e) => write!(f, "rls prefilter arena error: {e}"),
            RlsError::Kernel(e) => write!(f, "rls prefilter kernel error: {e}"),
            RlsError::InvalidK { k } => {
                write!(f, "invalid k: {k} (must be 1..={MAX_SEARCH_K})")
            }
            RlsError::NotFound => write!(f, "not found"),
            RlsError::ContextMismatch => write!(
                f,
                "policy context does not match the context the index was built with"
            ),
            RlsError::IndexStale => write!(
                f,
                "prefilter index is stale: rebuild required before searching again"
            ),
            RlsError::ProviderResultRejected => write!(
                f,
                "search provider returned a hit outside the policy-visible id set"
            ),
        }
    }
}

impl std::error::Error for RlsError {}

impl From<ArenaError> for RlsError {
    fn from(e: ArenaError) -> Self {
        RlsError::Arena(e)
    }
}

impl From<KernelError> for RlsError {
    fn from(e: KernelError) -> Self {
        RlsError::Kernel(e)
    }
}

/// 事前フィルタ方式の再利用可能インデックス（RLS-1〜4）。
///
/// [`Self::build`] 構築時に束縛した [`PolicyContext`] の可視性述語で
/// [`VectorArena::build_filtered`] を呼び、可視行のみのカラムナ表現を保持する。
/// 検索対象の候補集合（構築時点でどの行が挙がるか）は構築後の書き込みを反映しない
/// スナップショットのままだが、[`Self::search`] は返却直前にヒット行の現在の
/// tenant/visibility をストレージへ再照合するため、構築後に失効した行は結果に混入しない
/// （モジュールドキュメント「失効行の再検証」参照）。可視率・ポリシーが大きく変わる場合や
/// embedding 本体の鮮度が必要な場合は [`Self::build`] を呼び直して再構築する必要がある。
///
/// アクセサの `ctx` 必須化方針（codex-review P0 指摘・PR #151 対応）: [`Self::len`]・
/// [`Self::is_empty`] は構築元テナントの可視行数・行の有無という**存在情報**を返すため、
/// `ctx` を必須化し [`Self::built_ctx`] との完全一致を [`Self::search`] と同一の
/// fail-closed ゲートで照合する（一致しなければ [`RlsError::ContextMismatch`]）。
/// キャッシュ取り違え等で別テナント用の `PrefilterIndex` を受け取った呼び出し元が、
/// `search` を拒否されてもこれらのアクセサ経由で存在情報を得られてしまう経路を塞ぐ。
/// 一方 [`Self::dim`]・[`Self::table_name`] は `ctx` を要求しない: `dim` はテーブル定義
/// （`CREATE TABLE` 時に宣言される次元数）であり全テナント共通でテナント間の違いを
/// 持たない値、`table_name` は呼び出し元が [`Self::build`] へ自ら渡した引数の単純な
/// 反映であり、いずれも本インデックスから新たに得られる情報を持たない
/// （非機微と判断し対象外とした）。
pub struct PrefilterIndex<'s> {
    arena: VectorArena,
    /// [`Self::build`] に渡された `&Storage` をそのまま保持する。[`Self::search`] は
    /// この参照だけを失効行の再検証に使い、引数として別途 `Storage` を受け取らない
    /// （codex-review P0 指摘・PR #151 対応。モジュール doc「再検証対象ストレージの束縛」
    /// 参照。同一性を借用チェッカーで型レベルに保証し、別ストレージのすり替えを構造的に
    /// 排除する）。
    storage: &'s Storage,
    /// `arena.ids()` と同一集合の `HashSet` キャッシュ（[`Self::build`] 時に一度だけ構築）。
    /// [`Self::search`] は provider 結果の可視性再検証（`provider_result_is_valid`）で
    /// このキャッシュを使い回し、クエリ毎の再構築コストを避ける（本モジュールが解決対象と
    /// する「クエリ毎の前段コスト」をここで再生産しないため。モジュール doc 参照）。
    visible_id_set: HashSet<u64>,
    /// [`Self::build`] に渡された `PolicyContext` の複製（テナント ID・許可可視性集合）。
    /// [`Self::search`] はこの複製と呼び出し時の `PolicyContext` の完全一致を照合する
    /// ゲートとして使う（`is_visible` の追加呼び出しには使わない。可視性判定そのものは
    /// [`Self::build`] 時点の [`VectorArena::build_filtered`] で完結している）。
    /// codex-review P0 指摘・PR #151 対応: 転用・ポリシー失効の検出に必須（モジュール
    /// doc 参照）。
    built_ctx: PolicyContext,
}

impl<'s> PrefilterIndex<'s> {
    /// `table` に対し `ctx` の可視性述語で可視行のみのインデックスを構築する
    /// （事前フィルタ・RLS-1: 不可視行はこの構築時点でアリーナへ確保されない）。
    ///
    /// `ctx` は可視行の絞り込み述語として使うと同時に、[`Self::search`] での転用検出用に
    /// 複製して保持する（モジュールドキュメント参照）。`storage` への参照も
    /// [`PrefilterIndex`] のライフタイム `'s` として保持し、[`Self::search`] が別の
    /// `Storage` を受け取れない構造にする（モジュール doc「再検証対象ストレージの束縛」
    /// 参照）。テーブル不存在は `core.rs::EngineCore::search`/`get_row` と対称に
    /// [`RlsError::NotFound`] へ丸め込む（存在情報を漏らさない。security.md P0）。
    /// 容量超過・次元不整合はそのまま [`RlsError::Arena`] へ伝播する
    /// （`VectorArena::build_filtered` の契約をそのまま継承）。
    pub fn build(storage: &'s Storage, table: &str, ctx: &PolicyContext) -> Result<Self, RlsError> {
        let arena = match VectorArena::build_filtered(storage, table, |tenant, visibility| {
            ctx.is_visible(tenant, visibility)
        }) {
            Ok(arena) => arena,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(RlsError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };
        let visible_id_set: HashSet<u64> = arena.ids().iter().copied().collect();
        Ok(Self {
            arena,
            storage,
            visible_id_set,
            built_ctx: ctx.clone(),
        })
    }

    /// 保持済みインデックスに対して Top-k 検索を行う（over-fetch なし・RLS-3:
    /// 要求 `k` のまま provider を 1 回だけ呼び、追加フェッチを行わない）。
    ///
    /// `ctx` は [`Self::build`] 時に束縛した `PolicyContext` と完全一致
    /// （テナント ID・許可可視性集合の両方）していなければならない。一致しない場合は
    /// 別テナントへの転用・可視性の狭化/拡大のいずれであっても区別せず
    /// [`RlsError::ContextMismatch`] で fail-closed に拒否する（テナント境界 P0・
    /// codex-review P0 指摘・PR #151 対応。`batch_search.rs` が `PolicyContext` を
    /// クエリ引数として型レベルで要求する既存パターンと整合させ、`search` 単体で
    /// テナント境界の照合が完結する構造にする）。
    ///
    /// 一致後は `core.rs::EngineCore::search` と同一の前段検証（`k` の範囲・`query` の
    /// 次元/有限性）を行う。
    ///
    /// **失効行の全件事前検証（codex-review P0 指摘・PR #151 対応）**: 以前はヒット確定後に
    /// ヒット id だけを再検証していたが、これには 2 つの P0 があった。(1) provider は
    /// 検証より前にアリーナの**全ベクトル**を観測済みのため、事後に `IndexStale` で
    /// 拒否しても provider（untrusted）が既に失効行のベクトルを見た事実は取り消せない。
    /// (2) ヘッダ取得の `read_txn` は取得完了時に閉じ、その後の `ctx.is_visible` 評価から
    /// `Ok(hits)` 返却までの間に別の書き込みが挟まっても検出できない（TOCTOU）。
    /// 本実装はこれらを 1 つの機構で解消する: provider を呼ぶ**前**に、アリーナが保持する
    /// **全 id**（ヒットだけでなく全件。`self.storage.get_row_headers_from_table` は
    /// 呼び出しごとに単一の `read_txn` を張るため、この全件チェックは 1 つの一貫した
    /// スナップショット上で行われる）について、現在の `tenant_id`・`visibility` を
    /// [`crate::arena::VectorArena::tenant_id`]・[`crate::arena::VectorArena::visibility`]
    /// が保持する**構築時の値**と正確に一致するか検証する（`ctx.is_visible` の再評価では
    /// なく厳密な等値比較。可視性を保ったまま Public→Private のように値が変わっただけの
    /// 行も「構築時と状態が変わった」ものとして検出するため）。1 件でも不一致・不存在で
    /// あれば、provider を一切呼ばずに [`RlsError::IndexStale`] で fail-closed に拒否する
    /// （(1) の解消: 失効行のベクトルが provider のアドレス空間へ渡ること自体を防ぐ）。
    /// 全件一致した場合のみ、その直後に provider を呼ぶ。
    ///
    /// **一貫性契約**: 全件検証の完了から provider 呼び出しまでの間・provider 呼び出しから
    /// 返却までの間に別の書き込みトランザクションがコミットされても、本メソッドはそれを
    /// 検出するための追加読み取りを行わない（後述の理由により、全件検証以降は
    /// ストレージへ一切アクセスしない）。したがって本メソッドの返却値は
    /// **「全件検証を行った時点のストレージスナップショットに対して一貫」**であり、
    /// 「呼び出しが返った瞬間の最新状態」との一致は保証しない。次回の `search` 呼び出しは
    /// 改めて全件検証をやり直すため、失効は次回呼び出し以降に確実に検出される
    /// （永続的な見逃しにはならない）。
    ///
    /// **全件検証後にヒット限定の再検証を行わない理由**（(2) の解消）: `hits` は直後の
    /// `provider_result_is_valid` により必ずアリーナの `visible_id_set`（＝全件検証の対象
    /// だった id 集合）の部分集合であることが保証される。`get_row_headers_from_table` は
    /// 呼び出しのたびに新しい `read_txn`（redb のスナップショット分離）を張るため、
    /// 全件検証と同じ `read_txn` を使い回さない限り、ヒット限定の再検証は「同じ id 集合の
    /// 部分集合に対して**別の**スナップショットで再読取りする」操作になり、全件検証より
    /// 弱い保証しか生まない（別スナップショットである以上、全件検証時点では見えていな
    /// かった別の書き込みを新たに拾ってしまう可能性があり、それは「全件検証時点の
    /// スナップショットに対して一貫」という上記契約とは別の話になる）。一方、
    /// `get_row_headers_from_table` の呼び出しを `rls.rs` から橋渡しして同一 `read_txn`
    /// を使い回す設計（呼び出し元へ `redb::ReadTransaction` を公開する、または
    /// `search` 全体を `catalog.rs` 側のメソッドとして実装し直す）は、`ReadTransaction` と
    /// そこから開いた `redb::ReadOnlyTable` を同一構造体に保持する自己参照構造が必要になり
    /// （安全に実現するには `unsafe` か新規依存が要る。いずれも
    /// `.claude/rules/coding-rust.md`・`.claude/rules/dependency-policy.md` で原則禁止）、
    /// かつ untrusted かつ実行時間が不定な provider 呼び出しの間 `read_txn` を握り続ける
    /// ことになり、それ自体がリソース保持の観点で望ましくない。以上の理由から、
    /// 「provider 呼び出し前の全件検証」を単一の情報源とし、ヒット限定の事後再検証は行わない
    /// （全件検証の結果を超える追加情報を生まないため冗長）。
    ///
    /// **全件検証のコスト**（DoS 耐性）: 検証対象はアリーナの id 数（`self.arena.len()`）で、
    /// [`Self::build`] が使う [`crate::arena::VectorArena::build_filtered`] の構築時容量
    /// 上限（`arena.rs::MAX_ARENA_ROWS` = 1,000,000 行）により既に上限が課されている。
    /// つまり本検証は既存の DoS 対策の範囲内であり、新たに無制限な確保・走査を持ち込まない
    /// （行ごとに読むのはヘッダ（`tenant_id`・`visibility`）のみで embedding は読まない。
    /// [`crate::storage::Storage::get_row_headers_from_table`] のドキュメント参照）。
    /// 一方、従来のヒット限定検証（`O(k)`、`k <= MAX_SEARCH_K = 10,000`）と比べるとコストは
    /// 増加する（最大 100 倍・`String` アロケーションを伴う）。この設計変更のトレードオフ
    /// （本モジュールが元々解決対象としていた「クエリ毎の前段コスト」の一部を再び持ち込む
    /// 形になる点）はテナント境界 P0 の是正を優先した結果であり、呼び出し元への性能影響は
    /// 別途フォローアップの検討対象とする。
    ///
    /// 全件検証を通過した場合のみ provider を 1 回呼び、戻り値を共有ヘルパ
    /// `provider_result_is_valid`（`core.rs`）で再検証する。provider は untrusted
    /// 実装でありうるため、1 件でも契約違反があれば結果を一切返さず
    /// [`RlsError::ProviderResultRejected`] で拒否する（fail-closed。`core.rs`
    /// モジュールドキュメントの二重防御と同じ設計）。全件検証で使う id は
    /// `self.arena.ids()`（[`Self::build`] 時に確定した、呼び出し元・provider の入力に
    /// 一切依存しない値）のみであり、provider が捏造した id がストレージ読み取りへ
    /// 渡ることはない（provider をストレージ探索オラクルにできない。この検証順序に
    /// なったことで、ヒット確定後にストレージを読んでいた以前の版よりもオラクル耐性は
    /// 強化されている）。
    pub fn search(
        &self,
        ctx: &PolicyContext,
        provider: &dyn SearchProvider,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        validate_search_k(k).map_err(|k| RlsError::InvalidK { k })?;

        if query.len() != self.arena.dim() as usize {
            return Err(RlsError::Kernel(KernelError::DimMismatch {
                expected: self.arena.dim(),
                found: query.len(),
            }));
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(RlsError::Kernel(KernelError::NonFiniteQuery));
        }

        // 失効行の全件事前検証（上記ドキュメント参照）。`self.arena.ids()` は構築時に
        // 確定済みの信頼できる id 一覧であり、provider・呼び出し元の入力を経由しない。
        // `CatalogError`（redb I/O エラー・行ヘッダのデコード不正のいずれも含む）は
        // 種別を区別せず一律 `IndexStale` に丸め込む。`build` は同種のエラーを
        // `RlsError::Arena` へ透過するのに対し非対称だが、ここでは「現在の状態を
        // 確認できない」こと自体を fail-closed に「再構築が必要」として扱うのが目的であり、
        // エラー種別の詳細を呼び出し元へ伝える必要がない（他テナントの存在情報も
        // 含めない。security.md P0）。
        let arena_ids = self.arena.ids();
        let headers = self
            .storage
            .get_row_headers_from_table(self.arena.table_name(), arena_ids)
            .map_err(|_| RlsError::IndexStale)?;
        if headers.len() != arena_ids.len() {
            return Err(RlsError::IndexStale);
        }
        for (index, header) in headers.iter().enumerate() {
            let Some((current_tenant, current_visibility)) = header else {
                // 構築時には存在した行が検索時点では存在しない（削除相当）。
                return Err(RlsError::IndexStale);
            };
            // 構築時アリーナが保持する tenant_id・visibility との厳密な等値比較。
            // `ctx.is_visible` の再評価ではない点に注意（上記ドキュメント参照）:
            // 可視性を保ったまま値が変化した行（例: Public→Private だが ctx が両方
            // 許可）も「構築時と状態が変わった」ものとして検出するため。
            let built_tenant = self.arena.tenant_id(index);
            let built_visibility = self.arena.visibility(index);
            if built_tenant != Some(current_tenant.as_str())
                || built_visibility != Some(*current_visibility)
            {
                return Err(RlsError::IndexStale);
            }
        }

        // 保持済みアリーナは構築時点で可視行だけへ絞り込み済みであり、かつ上記の全件検証を
        // 通過したため、`ids`/`vectors` をそのまま provider へ渡せる（不可視・失効データは
        // provider のアドレス空間へ渡らない。`core.rs::EngineCore::search` と同じ境界＋
        // 本モジュール独自の失効検出）。
        let input = SearchInput {
            ids: self.arena.ids(),
            vectors: self.arena.vectors(),
            dim: self.arena.dim(),
            query,
            k,
        };
        let hits = provider.search(input)?;

        if !provider_result_is_valid(&hits, k, &self.visible_id_set) {
            return Err(RlsError::ProviderResultRejected);
        }

        Ok(hits)
    }

    /// インデックスが保持する可視行数を返す（テナント境界 P0・codex-review P0 指摘・
    /// PR #151 対応）。
    ///
    /// `ctx` は [`Self::build`] 時に束縛した `PolicyContext` と完全一致していなければ
    /// ならない。[`Self::search`] と同一のゲートで、一致しない場合は可視行数（存在情報）を
    /// 一切返さず [`RlsError::ContextMismatch`] で fail-closed に拒否する（キャッシュ
    /// 取り違え等で別テナント用インデックスを受け取った呼び出し元が、検索を拒否されても
    /// 本メソッド経由で行数を取得できてしまう経路を塞ぐため。struct doc 参照）。
    pub fn len(&self, ctx: &PolicyContext) -> Result<usize, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.len())
    }

    /// 可視行が 0 件かを返す（テナント境界 P0・codex-review P0 指摘・PR #151 対応）。
    /// `ctx` の照合方針は [`Self::len`] と同一（構築時 ctx との完全一致必須・
    /// 不一致は [`RlsError::ContextMismatch`]）。
    pub fn is_empty(&self, ctx: &PolicyContext) -> Result<bool, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.is_empty())
    }

    /// 検索対象ベクトルの次元。テーブル定義（`CREATE TABLE` 宣言次元）であり全テナント
    /// 共通の値のため `ctx` を要求しない（struct doc「アクセサの ctx 必須化方針」参照）。
    pub fn dim(&self) -> u32 {
        self.arena.dim()
    }

    /// 構築元のテーブル名。呼び出し元が [`Self::build`] へ自ら渡した引数の単純な反映で、
    /// 本インデックスから新たに得られる情報がないため `ctx` を要求しない（struct doc
    /// 「アクセサの ctx 必須化方針」参照）。
    pub fn table_name(&self) -> &str {
        self.arena.table_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::kernel::CpuScalarProvider;
    use crate::storage::{RowInput, Visibility};

    fn schema_for(table_name: &str, dim: u32) -> TableSchema {
        TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    // 簡易テンポラリディレクトリ（外部クレート非依存。dependency-policy 準拠。
    // `core.rs::tests::TempDir` と同型の複製）。
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    // プロセス内グローバル通番。`SystemTime::now()` の実測分解能はプラットフォームにより
    // ナノ秒より粗いため、同一 tick で並行実行された複数スレッドが `duration_since` の値だけで
    // 一時ディレクトリ名を組み立てると衝突しうる（`storage.rs::tests::unique_db_path`・
    // `arena.rs` の同種ヘルパーと同じ `SEQ.fetch_add` 対策。並列テスト実行時の
    // `DatabaseAlreadyOpen` フレーク回避）。
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tempdir() -> TempDir {
        let mut dir = std::env::temp_dir();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unique = format!(
            "engine-rls-test-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn open_storage(dir: &std::path::Path) -> Storage {
        Storage::open(dir.join("db.redb")).expect("open storage")
    }

    fn insert(storage: &Storage, table: &str, id: u64, tenant: &str, vis: Visibility, v: &[f32]) {
        storage
            .insert_row_into_table(
                table,
                id,
                &RowInput {
                    tenant_id: tenant,
                    visibility: vis,
                    embedding: v,
                    metadata: &[],
                },
            )
            .expect("insert row");
    }

    // 対象ビヘイビア: RLS-1。他テナント・不可視行は構築時点でインデックスへ含まれず、
    // 検索結果にも混入しない。
    #[test]
    fn build_excludes_invisible_rows_and_search_never_returns_them() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");
        assert_eq!(index.len(&ctx).expect("len ok"), 1);

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1。可視行が 0 件でも空結果を返す（拒否ではない）。
    #[test]
    fn empty_visible_set_returns_empty_result() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");
        assert!(index.is_empty(&ctx).expect("is_empty ok"));

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 5)
            .expect("search ok");
        assert!(hits.is_empty());
    }

    // `core.rs::EngineCore::search`/`get_row` と対称: テーブル不存在は他テナントの
    // 存在情報を漏らさず `RlsError::NotFound` へ丸め込まれ、`Display` にテーブル名を
    // 含まない（本 Issue のレビュー指摘対応）。
    #[test]
    fn build_returns_not_found_without_leaking_table_name_for_missing_table() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let result = PrefilterIndex::build(&storage, "no_such_table", &ctx);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("missing table must be rejected"),
        };
        assert!(matches!(err, RlsError::NotFound));
        assert_eq!(err.to_string(), "not found");
        assert!(!err.to_string().contains("no_such_table"));
    }

    #[test]
    fn search_rejects_k_zero_and_over_limit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 0),
            Err(RlsError::InvalidK { k: 0 })
        ));
        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], MAX_SEARCH_K + 1),
            Err(RlsError::InvalidK { .. })
        ));
    }

    #[test]
    fn search_rejects_dim_mismatch_and_non_finite_query() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0, 0.0], 1),
            Err(RlsError::Kernel(KernelError::DimMismatch { .. }))
        ));
        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[f32::NAN, 0.0], 1),
            Err(RlsError::Kernel(KernelError::NonFiniteQuery))
        ));
    }

    // ctx 束縛の検証: 同一テーブルでも構築時 ctx のテナントに紐づく行しか返らない
    // （一致 ctx での正常系）。
    #[test]
    fn index_is_bound_to_the_tenant_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");
        let hits = index_a
            .search(&ctx_a, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert!(hits.iter().all(|h| h.id == 1));

        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let index_b = PrefilterIndex::build(&storage, "docs", &ctx_b).expect("build index");
        let hits = index_b
            .search(&ctx_b, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert!(hits.iter().all(|h| h.id == 2));
    }

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 構築時とは別テナントの ctx
    // でインデックスを転用しようとした場合、可視行が存在していても
    // `RlsError::ContextMismatch` で fail-closed に拒否される（構築時可視行の内容に
    // 関わらず拒否されることを示すため、あえて `tenant-b` にも可視行を用意する）。
    #[test]
    fn search_rejects_a_different_tenant_context_than_the_one_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");

        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let result = index_a.search(&ctx_b, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::ContextMismatch)));
    }

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 構築時とは許可可視性集合が
    // 狭い ctx（Private 許可の取り消し。構築後のポリシー失効を模す）は転用とみなし拒否する。
    #[test]
    fn search_rejects_a_context_narrowed_from_build_time_visibility() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx_private =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx_private).expect("build index");

        let ctx_narrowed = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(matches!(
            index.search(&ctx_narrowed, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::ContextMismatch)
        ));

        // 構築時と完全一致する ctx（別インスタンスだが値は等しい）は受理される。
        let ctx_same =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert_eq!(ctx_same, ctx_private);
        let hits = index
            .search(&ctx_same, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("identical context must be accepted");
        assert_eq!(hits.len(), 1);
    }

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 構築時よりも許可可視性集合が
    // 広い ctx（構築時に持たなかった Private 許可が事後付与された ctx）も転用とみなし
    // 拒否する。構築時インデックスは Public 行しか保持していないため
    // `VectorArena::build_filtered` の再絞り込みは行われず、拡大された許可がそのまま
    // 反映されてしまわないことを確認する。
    #[test]
    fn search_rejects_a_context_widened_from_build_time_visibility() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_public_only = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx_public_only).expect("build index");

        let ctx_widened =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert!(matches!(
            index.search(&ctx_widened, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::ContextMismatch)
        ));
    }

    // 不正 provider（可視集合外の id を捏造して返す）は fail-closed に拒否される
    // （`core.rs::CoreError::ProviderResultRejected` と同一契約の再現）。
    struct RogueProvider;
    impl SearchProvider for RogueProvider {
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
            Ok(vec![SearchHit {
                id: 9_999_999,
                score: 1.0,
            }])
        }
    }

    #[test]
    fn rogue_provider_result_outside_visible_set_is_rejected() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let result = index.search(&ctx, &RogueProvider, &[1.0, 0.0], 1);
        assert!(matches!(result, Err(RlsError::ProviderResultRejected)));
    }

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: インデックス構築後に対象行の
    // tenant_id がストレージ側で書き換わり（構築時に可視だった行が他テナントへ移った）、
    // 同一 ctx で検索してもスナップショットに残った旧行を返さず `RlsError::IndexStale` で
    // fail-closed に拒否する。これが本 P0 指摘そのもの（build 後の失効行の漏えい）。
    #[test]
    fn search_rejects_when_a_hit_row_tenant_changed_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後に行 1 を他テナントへ書き換える（同一 id への upsert）。
        insert(
            &storage,
            "docs",
            1,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: tenant_id は変わらず
    // visibility だけが構築時 ctx の許可範囲外へ書き換わった場合も同様に
    // `RlsError::IndexStale` で拒否する。
    #[test]
    fn search_rejects_when_a_hit_row_visibility_changed_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後に行 1 の visibility を ctx が許可しない Private へ書き換える。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // 未変更のストレージに対しては引き続き正常にヒットを返すことを確認する
    // （再検証の追加で過剰拒否（over-rejection）を起こしていないことのガード）。
    #[test]
    fn search_still_returns_hits_when_storage_is_unchanged_since_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 「検索時に構築時とは別の
    // `Storage` を渡し、同名テーブル・同じ id・同じ tenant/visibility を持つ別 DB から
    // ヒットを返させる」という取り違えシナリオは、`search` の引数から `storage` を
    // 削除しフィールド `PrefilterIndex::storage`（構築時に束縛したライフタイム `'s` の
    // 参照）だけを使う構造にしたことで、コンパイル時に構文として表現不能になった
    // （以前この位置にあった単体テストは `search(&ctx, &storage_b, ...)` という
    // 呼び出し自体が今はコンパイルエラーになるため削除した。モジュール doc
    // 「再検証対象ストレージの束縛」参照）。行削除相当の「ヒット id がストレージ上に
    // 存在しない」再検証パス（`get_row_headers_from_table` の `None` 分岐）は
    // `catalog.rs::tests::get_row_headers_from_table_returns_none_for_a_deleted_or_missing_id`
    // で直接カバーする（本クレートに行削除 API 自体が存在しないため、削除ではなく
    // 未挿入 id で `None` 分岐を再現する）。

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: `len`/`is_empty` は構築元
    // テナントの可視行数・行の有無という存在情報を返すため、`search` と同一の ctx 照合
    // ゲートを持つ。別テナント（tenant-b。自身の可視行を持つ）の ctx を tenant-a の
    // インデックスへ渡した場合、`search` は拒否されるだけでなく `len`/`is_empty` からも
    // 存在情報を得られないことを確認する。
    #[test]
    fn len_and_is_empty_reject_a_context_different_from_the_one_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");

        // 一致する ctx では引き続き正常に値を返す（過剰拒否のガード）。
        assert_eq!(index_a.len(&ctx_a).expect("len ok"), 1);
        assert!(!index_a.is_empty(&ctx_a).expect("is_empty ok"));

        // 別テナントの ctx（tenant-b。tenant-a のインデックスへ渡す）はどちらも拒否する。
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        assert!(matches!(
            index_a.len(&ctx_b),
            Err(RlsError::ContextMismatch)
        ));
        assert!(matches!(
            index_a.is_empty(&ctx_b),
            Err(RlsError::ContextMismatch)
        ));
    }

    /// 呼び出し回数を記録してから [`CpuScalarProvider`] へ委譲する計装 provider
    /// （`tests/rls_prefilter.rs::CountingProvider` と同型の複製。crate 内 unit test
    /// モジュールから統合テストのヘルパーへは到達できないため複製する）。
    /// 失効行の全件事前検証（codex-review P0 指摘・PR #151 対応）が provider 呼び出し**前**に
    /// 完結していることを、この呼び出し回数で直接観測する。
    struct RecordingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl RecordingProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl SearchProvider for RecordingProvider {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            CpuScalarProvider.search(input)
        }
    }

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0・指摘 1）: 失効した行が
    // Top-k の**外**（クエリに対するスコアが最下位）であっても、provider は一切呼ばれない
    // ことを呼び出し回数で直接検証する。従来のヒット限定の事後検証では、この行は
    // 元々 Top-1 に入らないため再検証対象にすらならず、provider は失効行のベクトルを
    // 含むアリーナ全体を既に観測済みだった（漏えいが検出前に完了していた）。全件事前検証
    // により、provider はそもそも呼ばれない。
    #[test]
    fn search_rejects_before_calling_provider_even_when_the_stale_row_is_outside_top_k() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // クエリ [1.0, 0.0] に対する内積スコア: id=1 が最高位（1.0）、id=2 が中位（0.5）、
        // id=3 が最下位（0.1）。k=1 なら id=3 は本来 Top-k に入らない。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Public,
            &[0.1, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後、Top-1 には入らない id=3 だけを他テナントへ書き換える。
        insert(
            &storage,
            "docs",
            3,
            "tenant-b",
            Visibility::Public,
            &[0.1, 0.0],
        );

        let provider = RecordingProvider::new();
        let result = index.search(&ctx, &provider, &[1.0, 0.0], 1);
        assert!(matches!(result, Err(RlsError::IndexStale)));
        assert_eq!(
            provider.call_count(),
            0,
            "provider must not be called once any arena row fails the pre-check"
        );
    }

    // codex-review P0 指摘・PR #151 対応: ストレージが構築後に変化していない場合は、
    // 全件事前検証を通過して provider がちょうど 1 回呼ばれ、結果が返ることを確認する
    // （全件検証の追加が過剰拒否・provider 呼び出し省略を引き起こしていないことのガード）。
    #[test]
    fn search_calls_provider_exactly_once_when_no_row_is_stale() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let provider = RecordingProvider::new();
        let hits = index
            .search(&ctx, &provider, &[1.0, 0.0], 1)
            .expect("search ok");
        assert_eq!(provider.call_count(), 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }
}
