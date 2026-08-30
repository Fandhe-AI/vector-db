# 境界同点グループ再取得ループのレイテンシ影響計測

- ステータス: Accepted（本コミットで計測ベンチ・ADR を追加。`bench-tier` 実測は
  常駐 Ollama を持つ環境で運用者が別途実施——下記「`bench-tier`（PLAN-4/6/7）:
  運用者実測手順」参照）
- 対応: Issue #324（`perf(engine): 境界同点グループ再取得ループのレイテンシ影響を
  計測する（CORE-7・ティア別基準）`）。親: #301。発端: PR #320（Issue #310）
- 前提: `docs/spec/04-behavior/core-engine.md` CORE-7・`docs/spec/04-behavior/
  query-planning.md` PLAN-4, PLAN-6, PLAN-7（判定内容・数値基準は spec 側が SSOT）

## 背景

PR #320 は `crates/engine/src/hybrid.rs::hybrid_search_boosted` に、密チャネルの
`pool_depth` 境界の同点グループが未確定（`TieBoundary::Undetermined`）の場合に
`fetch_k` を倍増して密 provider を再呼び出しするループを追加した（上限
`MAX_FETCH_K`・可視集合サイズで有界化）。大規模段の Recall 回帰フィクスチャ
（20,000 件・100 クエリ）では、40 クエリで再取得が可視集合全体まで到達する
（`crates/engine/tests/hybrid_recall.rs::
hybrid_recall_large_scale_dense_refetch_is_bounded_by_visible_set_size` が固定値
40 で回帰トラッキング）。再取得は密 provider（`ParallelSearchProvider` 経由の
総当たり）の再実行を伴うため、ハイブリッド検索（C4 経路。`sql/exec.rs` →
`hybrid::hybrid_search`）の単発レイテンシを押し上げうるが、その劣化幅はこれまで
計測されていなかった。本 Issue はこの隙間を埋める。

## 既存受け入れ基準ベンチとの関係（測定経路の確認）

| ベンチ | 測定経路 | `hybrid_search` を通るか |
| ------ | -------- | ------------------------ |
| CORE-7（`batch_bench.rs::run_core7_gate`） | `BatchEngine::batch_search`（f16 常駐・CPU-SIMD の動的窓集約） | **通らない**（`docs/design/core7-dynamic-window-gate.md`） |
| ティア別（`tier_latency_bench.rs`。PLAN-4/6/7） | `EngineCore::execute_sql` の `USING PLAN(...)` 経由 | 通る（常駐 Ollama 前提） |

CORE-7 は `hybrid_search` を一切呼ばないため、PR #320 の変更が CORE-7 の実測値へ
影響することは構造的にありえない。ティア別ベンチは `hybrid_search` を通るが常駐
Ollama が前提で、再取得ループの寄与だけを Ollama なしに分離して計測する入口が
存在しなかった。

## 計測設計: in-build 比較

2 コミット間の worktree A/B（PR #320 前後を別 worktree でそれぞれビルドして比較）
ではなく、**単一ビルド内**で「再取得がほぼ発生しない通常コーパス」と「再取得が
複数回発生する同点誘発コーパス」を比較する方式を採用した（`crates/engine/benches/
hybrid_latency_bench.rs`・`crates/engine/benches/harness/hybrid_latency.rs`）。

理由:

- 2 worktree の cold build（`wgpu`・`usearch` の C++ ビルドを含む）を 2 回行うコストに対し、
  再取得ループの有無という唯一の変数を単一ビルド内で切り替えるほうが、非専有環境
  （後述）でも再現性の高い比較になる
- in-build 比較は `make bench-hybrid` を実行するだけで誰でも再現できる（worktree
  操作・過去コミットの一時チェックアウトを要しない）

### 同点誘発（「プロトタイプクラスタ」モード）

`harness::hybrid_latency::generate_corpus(seed, num_docs, vocab_size, dim,
quantize_levels)` は `quantize_levels: Some(n)` で `n` 個のプロトタイプベクトルを
生成し、各文書へそのまま（ビット単位で同一のまま）割り当てる。同一プロトタイプの
文書群は任意のクエリに対して内積スコアが厳密に一致するため、`pool_depth` 境界へ
巨大な同点グループを確実に発生させる（`quantize_levels: None` は通常の連続値
ベクトルで、再取得がほぼ発生しない対照）。

### 測定対象

`hybrid::hybrid_search(provider, SearchInput{k: 20}, &sparse_index, query_text, 20,
&RrfConfig::default())` の単発呼び出しのみ（SQL パース・テーブル走査を含まない。
`sql/exec.rs` の C4 経路から再取得ループの寄与だけを分離する）。`RefetchTrackingProvider`
（`ParallelSearchProvider` を包む診断ラッパ）で provider 呼び出し回数・観測された
最大 `k`（＝密側 `fetch_k`）・可視集合到達有無をクエリ単位で集計する
（`tests/hybrid_recall.rs::MaxKTrackingProvider` と同型）。

### フィクスチャ規模

`tests/hybrid_recall.rs` の段構成に合わせ小規模・大規模の 2 段で測る。小規模段は
`num_docs = 1,000`（`RrfConfig::default()` の初回 `fetch_k`（`pool_depth * 2` =
400）ちょうどに合わせると通常コーパスでも初回呼び出しで可視集合全体を取り切って
しまい比較にならないため、それを上回る規模にしてある）。大規模段は
`num_docs = 20,000`（`tests/hybrid_recall.rs` の大規模フィクスチャと同一件数）。

## 実測結果（参考値）

**実行環境**: 非専有の共有開発環境（他 worktree のエージェントが並行実行中）。
`env: os=linux arch=x86_64 logical_cpus=12 isa=Avx2Fma`（`harness::env_report::
EnvReport` 出力）。専有環境ではないため、下記の値は**参考値**として扱う（専有
環境での再測定要否は運用者判断へ申し送る。「申し送り」節参照）。

**測定コミット**: `perf/324-hybrid-refetch-latency`（base: `1be4575`。#320 の
再取得ループを含む）。`make bench-hybrid` を 3 回実行し、各指標は 3 回の中央値を
採用した（`docs/design/core7-dynamic-window-gate.md` と同じスパイク対策）。

| 段 | corpus | provider 呼び出し回数（最大） | 観測 `fetch_k` 最大値 | p95（3 回の中央値） | median（3 回の中央値） |
| --- | --- | --- | --- | --- | --- |
| small（1,000 件） | no_refetch（連続値） | 1 | 400 | 199 µs | 189 µs |
| small（1,000 件） | tie_refetch（5 クラスタ） | 2 | 800 | 349 µs | 191 µs |
| large（20,000 件） | no_refetch（連続値） | 1 | 400 | 3,676 µs | 3,101 µs |
| large（20,000 件） | tie_refetch（5 クラスタ） | 5 | 6,400 | 9,978 µs | 9,486 µs |

`reached_visible_set`（再取得が可視集合サイズまで到達したクエリ数）はいずれの
段・モードでも 0/50 だった。今回の同点誘発コーパス（5 クラスタ）は
`TieBoundary::Undetermined` を複数回引き起こして `fetch_k` を大規模段で
400 → 6,400 まで伸ばすには十分だったが、可視集合全体（20,000）まで到達する
最悪ケース（Recall フィクスチャの 100 クエリ中 40 クエリで実際に起きている挙動。
背景節参照）を単純なプロトタイプクラスタ数の調整だけでは再現できなかった。より
強い同点誘発フィクスチャの設計は本 Issue のスコープ外とする（「申し送り」節）。
本 PR のレビュー対応として、stage 名を `max_refetch` から `tie_refetch` へ改称し
（可視集合到達を含意しない表記へ）、`hybrid_latency_bench.rs` モジュールドキュメント
へ「可視集合到達ケース未測定」「コーパス分布差（連続値 vs. プロトタイプクラスタ）が
再取得ループ以外の変数として残る近似比較である」旨を明記した（コード側の詳細は
同ファイル参照。数値・解釈は本節が SSOT のまま変更していない）。

### 解釈

- 大規模段では、再取得ループが 1 回 → 5 回の provider 呼び出しへ伸びたことで
  p95 が約 2.7 倍（3,676 µs → 9,978 µs）、median が約 3.1 倍（3,101 µs →
  9,486 µs）に増加した。再取得ループの寄与は測定ノイズの範囲内ではなく、
  provider 呼び出し回数の増加に比例する形で明確に観測できる
- 小規模段でも provider 呼び出しが 1 回 → 2 回に伸びた影響で p95 が増加した
  （199 µs → 349 µs）が、median はほぼ変化していない（189 µs → 191 µs）。
  絶対値が小さい段ではプロセス起動時の測定ノイズの寄与が相対的に大きい
- CORE-7（下記）が「不変である」ことの確認である一方、本計測は「再取得ループが
  発生した場合、単発クエリの C4 経路レイテンシに測定可能な影響を与える」ことを
  示す。ハイブリッド検索経路自体には spec の直接的な数値基準がないため、
  本計測は PLAN-4/6/7（ティア別 p95）の端末間レイテンシへの寄与要因の 1 つとして
  位置づける。閾値との照合は `bench-tier` の運用者実測（下記）を待つ

## CORE-7: 構造的に不変（1 回実測で確認）

`batch_bench.rs::run_core7_gate` は `BatchEngine::batch_search` 経由でのみ測定し
`hybrid_search` を一切通らない（上表参照）ため、PR #320 前後での比較は情報量を
持たない（測定経路が変更の影響を受けえないことが構造的に確定しているため）。
前後 2 回の A/B は行わず、本ブランチ（#320 の変更を含む）で 1 回実測し
「変更に関わらず不変」であることの記録のみを残す。

```text
$ BENCH_VERBOSE=1 BENCH_BATCH_MAX_DEGRADATION_PCT=<任意の判定用値> make bench-batch
dynamic_window_degradation: rows=20000 dim=256 k=10 trials=9 pass=true
verbose(dynamic_window_degradation): trial_degradation_pct=[...] median_pct=-0.0167
```

median_pct（劣化率の中央値）は -0.0167%（劣化なし・むしろ改善方向のノイズ）で
`pass=true`。`hybrid_search` を通らない測定経路として想定どおりの挙動であり、
再取得ループの追加が CORE-7 の実測値に影響しないことを裏づける。

閾値（`BENCH_BATCH_MAX_DEGRADATION_PCT`）そのものは spec 由来の非公開値のため
本 ADR には転記しない（`.claude/rules/spec-confidentiality.md`）。

## `bench-tier`（PLAN-4/6/7）: 運用者実測手順

本実行環境には常駐 Ollama がないため（`127.0.0.1:11434` 応答なし）、
`make bench-tier` は自動運転では実測できない。**未実測**（値を捏造しない
fail-closed の方針）。

常駐 Ollama を持つ環境を確保できる運用者は、以下の手順で PR #320 前後の比較を
行える。

1. `git worktree add <scratch>/pre320 3b4ec54`（#320 squash 直前）と
   `git worktree add <scratch>/post320 1be4575`（#320 squash。または本ブランチの
   base）の 2 worktree を用意する
2. 両 worktree で README「ティア別レイテンシ受け入れ基準の実測手順」の手順に
   従い `make bench-tier` を実行する（接続・閾値 env は共通の値を使う）
3. `p95_latency(tier_dialogue_e2e)`・`p95_latency(tier_precision_e2e)`
   （いずれも `USING PLAN(...)` 経由で `hybrid_search`・再取得ループを含む
   エンドツーエンド計測）の p95 を比較する
4. 実測値は `docs/design/tier-latency-acceptance.md`「実測状態」節の運用に従い
   pass/fail・数値を記録する（本 ADR には転記しない。同ファイル参照）

## 対象外（別 Issue 候補）

- 再取得ループ自体のコスト削減策（同点判定の早期打ち切り・provider 側の同点
  グループ取得 API 等）。本 Issue は計測専任であり `hybrid.rs` は変更しない
- 可視集合全体まで到達する最悪ケース（Recall フィクスチャで実際に起きている
  挙動）を安定して再現するフィクスチャ設計の強化
- `bench-tier` の運用者実測（上記手順の実施）
- 専有環境での再測定（本計測は非専有環境の参考値）
- 本ベンチの `.github/workflows/bench.yml` への配線（spec 閾値を持たない
  情報提供専用のため意図的に配線しない。将来的に閾値化する場合は別途検討）

## 申し送り

- 上記「対象外」の各項目は、ユーザー承認を得たうえで別 Issue として追跡する
  （`.claude/rules/out-of-scope-tracking.md`）
- 専有環境での再測定要否・より強い同点誘発フィクスチャの要否は、本 ADR の
  実測結果（大規模段で約 2.7〜3.1 倍の明確な劣化が観測できている）を踏まえて
  運用者が判断する
