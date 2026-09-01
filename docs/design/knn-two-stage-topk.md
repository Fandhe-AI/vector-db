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

### 実測値（集計値のみ・per-run 生データは保持せず）

before（origin/main。5 回）・after（本実装。5 回）を単純逐次で計測したところ
before 側の 2 回（loadavg 急増と重なった）が S5_search_scalar=71.9・46.7 ns/行
（他は 9〜11 ns/行台）という桁違いの外れ値になった。これは環境ノイズの影響が
測定対象の効果（見込み最大でも数 ns/行）を大きく上回ることを示しており、
逐次計測では before/after を正しく比較できないと判断し、before/after を交互に
実行する形（4 ペア）で追加計測した。

interleave 計測を含めた全 9 回の **min-of-N**（環境ノイズは常に加算方向にしか
効かないため、最小値が実効コストに最も近い保守的な推定量として計測当時に
採用した集計方法。per-run の個別値は記録が残っておらず、下表の各フェーズの
min-of-N のみが事後に参照できる集計値である）:

| フェーズ | before min (ns/行) | after min (ns/行) | 差分 |
| -------- | ------------------- | ------------------ | ---- |
| S5_search_scalar | 8.8 | 8.3 | -5.7% |
| S5_search_parallel | 8.8 | 9.5 | +8.0% |
| S5prime_distance_only（**本変更が触れないコードパス**） | 8.0 | 7.0 | -12.5% |

S5prime_distance_only は `search_range_two_stage` を一切経由しない生の
`dot_wrapper` 逐次ループであり、before/after で production コードは無変更の
はずの区間である。にもかかわらず min-of-N でも -12.5% の差が観測された。この
差分は統計的な有意性検定の結果ではなく、単一組の min-of-N 差分という一点の
観測にすぎない——本環境（共有計測マシン・`loadavg` が 1〜9 台で変動）では、
変更を一切含まない区間ですら S5_search_scalar の「改善」（-5.7%）・
S5_search_parallel の「悪化」（+8.0%）と同程度かそれ以上の run-to-run 差分が
生じることを、この一事例が示している。この一点の観測から「本環境の測定
ノイズの下限は約 12% である」という母数を確立することはできない。

`feature_bench`（`vector_knn` フェーズ。interleave 3 回・min-of-N）も同様に
p50 before=8696us → after=8340us（-4.1%）・p95 before=9369us → after=9042us
（-3.5%）だった。

### 事前固定した判断ルールの適用

Step 5 で事前固定した採否ルール（「S5_search_scalar の median が 3 回すべてで
before 比 5% 以上短縮、かつ S5_search_parallel・`feature_bench` の
`vector_knn` p50/p95 が before 比 +3% 以内」）は 3 回ぶんの**中央値**を要求する。
しかし上表のとおり実測時に保持したのは各フェーズの min-of-N（9 回中の最小値）
のみで、個別 3 回の中央値を事後に算出できる per-run の生データは残っていない
（次回計測時は per-run 値を必ず記録すること）。そのため本ルールを文字どおり
適用して合否を判定することはできない。

その代わりに、次の事実のみを不採用の根拠とする: 本変更が一切経由しない
S5prime_distance_only の min-of-N 差分（-12.5%）が、S5_search_scalar の
「改善」（-5.7%）・S5_search_parallel の「悪化」（+8.0%）のいずれよりも
絶対値で大きい。すなわち変更を含まない区間の run-to-run 変動が、変更を含む
区間で観測された差分を上回っており、本環境・この標本数では変更の効果を
run-to-run 変動から切り分けて確認できない。これは「改善が測定ノイズ内なら
判断根拠を記録して close してよい」という Issue 本文が想定していたケースに
該当すると判断した。

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
  ——変更を含まない区間ですら run-to-run 差分が数%〜十数% 生じた本開発環境
  （共有マシン）より run-to-run 変動が十分小さい環境が確保できれば、本 ADR の
  設計を再度実装して評価し直せる

## 再現手順（将来の再評価向け。本 ADR の実測値そのものの厳密な再現は不可）

**重要**: 試作した `search_range_two_stage` の実装はコミットされておらず、本
ADR コミット自体は `crates/engine/src` 配下を変更しない。そのため上表の
after 側の測定を生成したバイナリはリポジトリ履歴から復元できず、上記の実測
値は再現不能な履歴記録として扱う。将来この設計を再評価する場合は、以下の
手順で「試作した設計」節の記述に基づき実装を作り直したうえで計測すること。

- before 側の基準コミット: 本 ADR コミットの親コミット `50189b1`
  （`origin/main`。このコミット・本 ADR コミット自身のいずれも
  `crates/engine/src` 配下を変更しないため、本 ADR コミットの HEAD を before
  側の計測に使ってもよい）
- after 側: 上記「試作した設計」節の記述（`kernel.rs::search_range_two_stage`・
  `SCORE_BLOCK_ROWS` 単位のブロック分割・距離計算段/選出段の分離）に基づき
  実装を作り直し、「等価性の検証」節に列挙した回帰テスト（境界を跨ぐ n・k・
  破損行のブロック境界一致・`CpuScalarProvider`／`ParallelSearchProvider` 間
  bit 単位一致・整数ベクトル独立参照実装との一致）を先に green にしてから
  計測すること
- ビルド: before/after とも `cargo build --release -p engine`
  （before/after で `Cargo.toml`・依存は変更しないため同一プロファイル）
- 計測対象: `make bench-knn-profile`（S5 系フェーズ）と
  `cargo run --release -p engine --example feature_bench`（`vector_knn`
  フェーズ）の両方を実行する
- 交互実行: before 1 回 → after 1 回を 1 ペアとし、最低 4 ペア（8 回）以上を
  ペア単位で交互に実行する。逐次実行（before を N 回連続 → after を N 回
  連続）は環境ノイズと変更の効果が時間方向に交絡するため避けること（本 ADR
  「実測値」節参照）
- 集計: フェーズごとに before/after 各回の値を per-run で記録し（本 ADR の
  実測ではこの per-run 生データを保持しなかったため上表は再現できない——
  次回は必ず記録すること）、事前固定した採否ルールが指定する統計量
  （median 等）をそのまま算出できる形で残す
- 環境: `nproc`・同時実行中の他プロセス・`loadavg` を計測ログに残し、
  可能であれば専有環境（他プロセスと負荷を共有しない環境）で実施する

```sh
make bench-knn-profile   # S5 系フェーズの実測（時間依存・手動実行専用）
cargo run --release -p engine --example feature_bench  # vector_knn フェーズ
```
