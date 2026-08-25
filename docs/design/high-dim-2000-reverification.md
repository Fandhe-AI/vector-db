# ADR: 2000 次元 CPU-SIMD/GPU 相対性能・複数次元テーブル共存の再検証

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-151（MS-6 / phase:6・並行フォローアップ）
- 対象ビヘイビア: EXT-2（`docs/spec/04-behavior/extensions.md`）
- 関連: TABLE-2・TASK-91（`docs/design/multi-dim-table-coexistence.md`）・
  TASK-160（PoC-14、`docs/spec/03-poc/f16-quantization-bandwidth/`）
- 前提: TASK-126（PR #141 マージ済み）

## 背景

`docs/design/multi-dim-table-coexistence.md`（TASK-91）は 384/768/1536 次元での
複数次元テーブル共存を検証し、「2000 次元級の再検証は別タスクの管轄」と明記して
スコープ外とした。TASK-160（PoC-14）は合成ベクトルで 2000 次元の CPU-SIMD/GPU
バンド幅を検証済みだが、MS-6 での EXT-2 最終確定に向けて (i) 2000 次元での
CPU-SIMD/GPU 相対性能の再検証と、(ii) 実埋め込みに近い分布・複数次元テーブル
共存という残宿題が残っている。本 ADR はそのうち本リポジトリの管轄範囲
（production 検索経路での複数次元共存・その実測）を扱う。

**位置づけ**: 実装をブロックしない並行フォローアップ。production コード
（`crates/engine/src/`）の変更は行わず、既存 API に対する検証テスト・実測
ハーネス・本レポートの追加に限定した。

## 検証設計

1. **正しさ（`crates/engine/tests/extensions.rs` の `ext2_2000_dim_*` 3 ケース）**:
   `EngineCore::search`（`core.rs`。wire-server が依存する実検索経路）を通し、
   768 次元テーブルと 2000 次元テーブルが同一 `Storage` に共存した状態で
   (a) 既定 provider（`search_engine::default_engine()`）と参照実装
   （`kernel.rs::CpuScalarProvider`）の Top-k が完全一致すること、(b) 次元不一致
   クエリがテーブルごとに fail-closed に拒否されること、(c) close→reopen 後も
   同一 Top-k が得られることを検証した。
2. **製品経路の次元スケーリング（`crates/engine/examples/high_dim_bench.rs` パート A）**:
   `kernel.rs::SearchProvider` を直接叩き、768 次元・2000 次元それぞれの単発
   クエリ p50/p95/max を既定 provider（スレッド並列。ベクトル化なし）と参照実装
   （単一スレッド・スカラー）の両方で計測し、次元比（2000/768）に対する所要時間
   比を求めた。
3. **2000 次元共存の end-to-end（同ファイル パート B）**: (a) 2000 次元テーブル
   単独 DB、(b) 768 + 2000 次元テーブル共存 DB（各テーブル同一行数）を作り、
   `EngineCore::search` での 2000 次元テーブル検索 p50/p95 を比較した。
4. **CPU-SIMD / GPU の相対性能再検証（PoC-14 ハーネス再実行）**: 本リポジトリの
   worktree では `docs/spec` submodule が未初期化（`git submodule status` で
   `-` 接頭辞、内容なし）であり、private ハーネス（`docs/spec/03-poc/f16-quantization-bandwidth/impl/`）
   に本セッションからアクセスできない。共有 checkout に対する
   `git submodule update --init`（ネットワーク・グローバル状態変更を伴う）を
   本セッション判断で実行することは避け、fail-closed に「未測定」として記録する
   （下記「制約・スコープ外」)。

## 実測環境

| 項目 | 値 |
| ---- | -- |
| CPU | QEMU Virtual CPU（x86_64、avx2/fma/f16c あり）、12 vCPU |
| OS | Linux 7.0.0-29-generic（Ubuntu ベース） |
| GPU | NVIDIA GeForce RTX 3060（driver 595.71.05。本 ADR の実測では未使用） |
| RAM | 32 GiB |
| ビルド | `cargo run --release`（`-p engine --example high_dim_bench`） |

**注意（測定条件の限界）**: 本ホストは仮想化 CPU（QEMU）であり、`multi-dim-table-coexistence.md`
の実測環境（Apple M4 Max ベアメタル）とは異なる。絶対値の比較はせず、本 ADR 内の
相対比（次元比・共存有無）にのみ意味を持たせる。項目 4（CPU-SIMD/GPU 相対性能）は
本ホストでの実測自体が今回できていない点に留意（下記参照）。

## 実測結果

### パート A: 単発経路の次元スケーリング（provider 直接、row_count=20,000、k=10）

| provider / dim | p50 | p95 | max |
| --------------- | --- | --- | --- |
| default（スレッド並列） dim=768 | 2.154ms | 4.080ms | 5.274ms |
| default（スレッド並列） dim=2000 | 7.557ms | 10.226ms | 11.781ms |
| CpuScalarProvider（単一スレッド） dim=768 | 6.646ms | 6.680ms | 6.715ms |
| CpuScalarProvider（単一スレッド） dim=2000 | 18.959ms | 20.554ms | 21.363ms |

- 次元比（2000/768、p50）: default（スレッド並列）= 3.51 倍、CpuScalarProvider = 2.85 倍。
  演算量ベースの理論比（2000/768 ≒ 2.60）に対し、CpuScalarProvider はおおむね近い
  一方、スレッド並列側はやや上振れした（後述「判断材料」参照）。
- 並列化による短縮幅（p50）: 768 次元で 6.646ms → 2.154ms（約 3.1 倍）、2000 次元で
  18.959ms → 7.557ms（約 2.5 倍）。次元が大きくなるほど並列化の短縮率がやや縮む。

### パート B: 共存状態の end-to-end（`EngineCore::search`、1 テーブルあたり row_count=10,000、k=10）

| config | p50 | p95 | DB ファイルサイズ |
| ------ | --- | --- | ------------------ |
| solo（2000 次元単独） | 3.659ms | 4.519ms | 134,746,112 bytes |
| coexist（768 + 2000 次元共存） | 3.800ms | 4.493ms | 134,746,112 bytes |

- 共存によるオーバーヘッド（p95、coexist vs solo）: -0.6%（有意な劣化なし）。
- DB ファイルサイズは両条件で同値だった。`redb` のページ確保単位（成長チャンク）が
  支配的で、768 次元テーブル分のデータ量が今回のファイルサイズの差として現れな
  かったとみられる（`multi-dim-table-coexistence.md` の傾向とは異なる。行数・
  データ量の規模の違いによるものと考えられ、本 ADR では追加調査は行わない）。

### CPU-SIMD / GPU 相対性能（PoC-14 再実行）

**未測定**。上記「検証設計」4. の通り、本セッションの worktree では
`docs/spec` submodule が未初期化であり、private ハーネスへアクセスできな
かった。GPU 側・SIMD 側いずれについても、CPU 参照実装や既存の M4 Max 実測値
（`docs/design/multi-dim-table-coexistence.md` 等）で代替しない（fail-closed。
`.claude/rules/coding-rust.md`「エラー契約は fail-closed」の趣旨を測定判断にも
適用した）。再実行手順は「制約・スコープ外」に記載する。

## 判断材料

- **製品経路（スレッド並列のみ、ベクトル化なし）は 2000 次元でも 768 次元共存
  テーブルとの間で正しさが崩れない**: `ext2_2000_dim_table_coexists_with_768_dim_table_and_search_is_exact`
  で、既定 provider と参照実装 `CpuScalarProvider` の Top-k がスコアまで含めて
  完全一致することを確認した（`parallel_search.rs` が内積計算を `kernel.rs::dot`
  で共有し加算順序を揃えているため、丸め誤差起因の不一致は観測されない設計）。
- **次元不一致は 2000 次元でも per-table に fail-closed で拒否される**:
  `ext2_2000_dim_search_query_dim_mismatch_is_rejected_per_table_fail_closed` で
  確認した（`core.rs::EngineCore::search` の早期次元検証がカタログ照会のみで
  完結し、`VectorArena::build` の全行デコードへ進む前に拒否する設計どおり）。
- **2000 次元テーブルの検索結果は 768 次元テーブルとの共存 DB でも close/reopen
  後に不変**: `ext2_2000_dim_search_results_survive_close_and_reopen` で確認した。
- **2000 次元検索の end-to-end 性能は 768 次元テーブルとの共存による明確な劣化を
  示さなかった**（パート B、p95 で -0.6%）。ただし本ホストは仮想化 CPU上の共有
  環境であり、`multi-dim-table-coexistence.md` と同じ限界（他プロセスの同時稼働
  可能性）を持つため、この数値は目安に留める。
- **次元スケーリングの比率はスレッド並列側でやや理論比を上回った**（3.51 倍 vs
  理論目安 2.60 倍）。CpuScalarProvider（2.85 倍）の方が理論比に近く、スレッド
  並列側の上振れはスレッド起動・結合コストなどの固定オーバーヘッドが 2000 次元
  では相対的に薄まりにくいことが一因と推測される（本 ADR では原因の切り分けは
  行わない。製品コードへの SIMD 導入判断は Issue #177 / #109 の管轄）。
- **GPU/CPU-SIMD の相対性能そのものは今回再検証できなかった**。EXT-2 の
  「2000 次元での GPU 優位性が768 次元と同様に維持されるか」という判断材料は
  本 ADR では提供できていない（下記「制約・スコープ外」参照）。

## 制約・スコープ外

1. **CPU-SIMD / GPU の相対性能再検証（PoC-14 ハーネス）は未実施**。本セッション
   の worktree で `docs/spec` submodule が未初期化のため、private ハーネス
   （`docs/spec/03-poc/f16-quantization-bandwidth/impl/`）を実行できなかった。
   submodule が初期化済みの環境（`git -C docs/spec status --short` が確認できる
   状態）で、`CARGO_TARGET_DIR` を submodule 外（例: scratchpad）に向けて
   `cargo run --release` を再実行し、768/2000 次元の CPU-SIMD p95・GPU バッチ
   p95・クエリあたり償却時間を回収の上、本 ADR の該当節を追記する必要がある。
2. **実埋め込み分布での再確認は未実施**。本 ADR・`tests/extensions.rs`・
   `examples/high_dim_bench.rs` はいずれも決定論的な合成ベクトル（xorshift32）
   を使う。2000 次元の実埋め込みモデル・データセットの選定はオーナー判断事項。
3. **製品コードでの CPU-SIMD/実 GPU 経路は本 ADR の対象外**。`crates/engine` に
   SIMD カーネルは未実装（`parallel_search.rs` はスレッド並列のみ。Issue #177 /
   #109 の管轄）、実 GPU 経路も未接続（`batch_search.rs::BatchEngine` は f16
   パックの CPU 参照実装。wgpu 導入は Issue #178 の管轄）。パート A の
   「default（スレッド並列）」はスカラー並列単発経路であり、SIMD/GPU の代替
   計測ではない。
4. `docs/spec` の EXT-2 ステータス更新（spec リポ側の作業。本リポからは行わない）。
5. `sled` / 自作 MVCC 等、永続化層自体の変更（本検証はいずれも扱わない）。

## 参照

- `docs/spec/05-tasks.md`（TASK-151・TASK-160）
- `docs/spec/04-behavior/extensions.md`（EXT-2）・`docs/spec/06-roadmap.md`（MS-6）
- `docs/spec/03-poc/f16-quantization-bandwidth/`（PoC-14。本 ADR では未実行）
- `crates/engine/src/core.rs`（`EngineCore::search` の次元早期検証）
- `crates/engine/src/parallel_search.rs`（既定 provider。ベクトル化なしの理由）
- `crates/engine/tests/extensions.rs`（`ext2_2000_dim_*` 3 ケース）
- `crates/engine/examples/high_dim_bench.rs`（実測ハーネス）
- `docs/design/multi-dim-table-coexistence.md`（TASK-91。同様の ADR 形式・実測手法の前例）
