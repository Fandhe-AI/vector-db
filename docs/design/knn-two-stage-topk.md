# 距離計算と Top-k 選出の 2 段分離バッチ化

- ステータス: **Rejected**（実装・回帰テストを試作し等価性は確認したが、前後比較の
  実測で改善を測定ノイズと切り分けられなかったため production 変更を撤回。
  本コミットは実測結果と判断根拠のみを記録する）
- 対応: Issue #366（`perf(engine): 距離計算と Top-k 選出の 2 段分離バッチ化`）。
  親: #361「ベクトル検索・ストレージレイアウト最適化」Phase 5
- 前提: `docs/design/knn-stage-profile.md`（Issue #362。S5 の段別内訳実測）・
  TASK-124・TASK-126・CORE-3・CORE-4・CORE-13（`docs/spec/04-behavior/
  core-engine.md`。判定内容・数値基準は spec 側が SSOT）

## 背景

`docs/design/knn-stage-profile.md`（Issue #362）の実測では、S5（距離計算＋Top-k
選出）は 9.0〜9.6 ns/行で e2e の 1.5% 未満、S5_scalar − S5'（Top-k 選出ループの
追加コストのみ）は 0.8〜3.1 ns/行にとどまることが分かっていた。改善幅は小さい
可能性が高いことは Issue 本文の時点で織り込み済みで、「悪化・効果僅少なら判断
根拠を記録して close してよい」とされている。

現状の CPU 総当たり検索（`kernel.rs::CpuScalarProvider::search`・
`parallel_search.rs::search_range`）は、候補 1 行ごとに「範囲取得 → `dot` →
有限値検査 → `TopKSelector::push`（`BinaryHeap` 判定）」を交互に行う単一ループ
だった。Lance の flat search のように「全候補の距離をスクラッチへ一括計算する
段」と「Top-k を選出する段」を分離し、内積カーネル呼び出しの連続実行による
パイプライン効率・将来のベクトル化余地を上げることを狙った。

## 試作した設計（撤回済み）

`kernel.rs::search_range_two_stage` を新設し、`ids`/`vectors` を
`SCORE_BLOCK_ROWS`（既定 1024 行）単位のブロックに分割して走査する構成を試作
した。ブロックごとに「距離計算段（`dot` を連続呼び出しし `[f32;
SCORE_BLOCK_ROWS]` のスタック配列へ書く。取得失敗行・非有限スコアは `f32::NAN`
の番兵）」→「選出段（`is_finite` な行だけ `TopKSelector::push`）」の 2 段構成
とし、`CpuScalarProvider::search`・`parallel_search.rs::search_range` の両方が
これを共有する形にした。スクラッチはスタック上の固定長配列（4 KiB）に限定し、
候補行数に比例したヒープ確保は新設しない設計（`arena.rs` の `embedding_scratch`
再利用と同じ思想）。`SearchProvider` trait・`SearchInput` 等の公開 API・
`isa.rs`（`dot` 本体・加算順序）は変更しない方針で設計した。

### 等価性の検証（撤回前に確認済み）

ブロック分割前の単一ループ実装との `assert_eq!`（境界を跨ぐ n・k の組み合わせ・
破損行〔truncate・NaN〕がブロック境界にまたがるケース）、`CpuScalarProvider`／
`ParallelSearchProvider` 間の bit 単位一致（n=1023・1024・1025・2049・5000）、
整数ベクトルでの独立参照実装（`benches/harness/scalar_reference.rs::
top_k_ids_scalar`）との一致（n=1025・2100）を回帰テストとして追加し、いずれも
green（`cargo test --workspace --all-features` 含め全体テスト・`cargo clippy
--workspace --all-targets --all-features -- -D warnings`・`make core-api-check`・
`make sort-determinism-check` も pass）だった。**正しさそのものは検証済み**で、
撤回の理由は次節の性能実測にある。

## 前後比較の実測と撤回理由

計測環境: 本開発環境（`nproc`=12）。他の並列実装エージェントがビルド・テストを
同時実行する共有マシンのため、負荷変動によるノイズが大きい（実測時
`loadavg` は 1〜9 台まで変動した）。`make bench-knn-profile`
（`crates/engine/benches/knn_profile_bench.rs`。dim=128・25,000 行コーパス）で
S5 系フェーズを比較した。

### 生データ

before（origin/main。5 回）・after（本実装。5 回）を単純逐次で計測したところ
before 側の 2 回（loadavg 急増と重なった）が S5_search_scalar=71.9・46.7 ns/行
（他は 9〜11 ns/行台）という桁違いの外れ値になった。これは環境ノイズの影響が
測定対象の効果（見込み最大でも数 ns/行）を大きく上回ることを示しており、
逐次計測では before/after を正しく比較できないと判断し、before/after を交互に
実行する形（4 ペア）で追加計測した。

interleave 計測を含めた全 9 回の **min-of-N**（環境ノイズは常に加算方向にしか
効かないため、最小値が実効コストに最も近い保守的な推定量）:

| フェーズ | before min (ns/行) | after min (ns/行) | 差分 |
| -------- | ------------------- | ------------------ | ---- |
| S5_search_scalar | 8.8 | 8.3 | -5.7%（5% ゲート境界上、僅かに通過） |
| S5_search_parallel | 8.8 | 9.5 | **+8.0%**（非劣化条件 +3% を超過） |
| S5prime_distance_only（**本変更が触れないコードパス**） | 8.0 | 7.0 | -12.5% |

S5prime_distance_only は `search_range_two_stage` を一切経由しない生の
`dot_wrapper` 逐次ループであり、before/after で production コードは無変更の
はずの区間である。にもかかわらず min-of-N でも -12.5% の差が出ており、これは
本環境における測定ノイズの下限が少なくとも約 12% あることを意味する。
S5_search_scalar の「改善」（-5.7%）・S5_search_parallel の「悪化」（+8.0%）は
いずれもこのノイズ下限（12.5%）以下の絶対値であり、**変更の実効果として
統計的に有意に切り分けられない**。

`feature_bench`（`vector_knn` フェーズ。interleave 3 回・min-of-N）も同様に
p50 before=8696us → after=8340us（-4.1%）・p95 before=9369us → after=9042us
（-3.5%）と、5% ゲートに届かずノイズ相当の範囲にとどまった。

### 事前固定した判断ルールの適用

Step 5 で事前固定した採否ルール（「S5_search_scalar の median が 3 回すべてで
before 比 5% 以上短縮、かつ S5_search_parallel・`feature_bench` の
`vector_knn` p50/p95 が before 比 +3% 以内」）に照らすと、S5_search_parallel が
+8.0%（許容 +3% を超過）となり不採用側に確定する。ノイズ下限（12.5%）の実測が
このゲート自体を本環境では信頼度をもって評価できないことも示しており、
「改善が測定ノイズ内」という Issue 本文が想定していたケースそのものに該当する。

## 判断

**不採用（撤回）**。`crates/engine/src/kernel.rs`・
`crates/engine/src/parallel_search.rs`・対応する
`crates/engine/tests/parallel_search.rs`・
`crates/engine/tests/simd_bench_reference.rs` への変更はすべて取り消し、
production コード・テストは Issue #366 着手前の状態（`origin/main`）へ復元した。
本コミットは実測結果と判断根拠を記録する ADR・`CLAUDE.md` の追記のみを含む。

設計・等価性の検証自体は妥当だったため、測定ノイズの小さい専有環境が確保できた
場合は本 ADR の設計節を出発点に再評価できる（「事前に固定長ブロック方式で
`search_range_two_stage` を共有実装化する」という設計判断自体は変更不要）。

## スコープ外

- `crates/engine/src/isa.rs`（`dot` カーネル本体の複数アキュムレータ化。
  Issue #365 の管轄）
- `rls.rs::PrefilterSnapshot::search_with`（`EngineCore::search` の
  `PrefilterCache` 経路。同型ループだが本 Issue の対象は kernel/parallel_search）
- `batch_search.rs`／`gpu_batch.rs`（複数クエリ同時走査で構造が異なる）
- 専有環境（他プロセスと負荷を共有しない計測環境）での再評価
  ——測定ノイズ下限が本開発環境（共有マシン）の実測（約 12.5%）より十分小さい
  環境が確保できれば、本 ADR の設計を再度実装して評価し直せる

## 再現手順

```sh
# before/after を interleave（交互）で複数回実行し、min-of-N で比較する。
# 逐次実行（before を N 回連続 → after を N 回連続）は環境ノイズと変更の効果が
# 時間方向に交絡するため避けること（本 ADR「生データ」節参照）。
make bench-knn-profile   # S5 系フェーズの実測（時間依存・手動実行専用）
cargo run --release -p engine --example feature_bench  # vector_knn フェーズ
```
