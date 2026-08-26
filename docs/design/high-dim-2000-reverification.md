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
スコープ外とした。2000 次元での残タスクの詳細は `docs/spec/05-tasks.md`
（TASK-151・TASK-160）・`docs/spec/06-roadmap.md`（MS-6）・
`docs/spec/04-behavior/extensions.md`（EXT-2）を参照。本 ADR はそのうち
本リポジトリの管轄範囲（production 検索経路での複数次元共存・その実測）を扱う。

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
4. **CPU-SIMD / GPU の相対性能再検証（PoC-14 ハーネス再実行）**: 本セッションからは
   private ハーネス（`docs/spec/03-poc/f16-quantization-bandwidth/`。TASK-160・
   PoC-14）にアクセスできず、fail-closed に「未測定」として記録する
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

2 回実行した（2 回目は「DB ファイルサイズを `Storage`/`EngineCore` を close した後に
計測する」よう `high_dim_bench.rs` を修正した後の再実行。詳細は下記コラム参照）。
下表は 2 回目（計測経路修正後）の値を採用する。1 回目との差は「判断材料」の
run-to-run 変動の節で扱う。

### パート A: 単発経路の次元スケーリング（provider 直接、row_count=20,000、k=10）

| provider / dim | p50 | p95 | max |
| --------------- | --- | --- | --- |
| default（スレッド並列） dim=768 | 1.585ms | 1.743ms | 1.799ms |
| default（スレッド並列） dim=2000 | 4.270ms | 4.847ms | 5.012ms |
| CpuScalarProvider（単一スレッド） dim=768 | 5.782ms | 5.957ms | 6.375ms |
| CpuScalarProvider（単一スレッド） dim=2000 | 15.958ms | 16.108ms | 16.248ms |

- 次元比（2000/768、p50）: default（スレッド並列）= 2.69 倍、CpuScalarProvider = 2.76 倍。
  演算量ベースの理論比（2000/768 ≒ 2.60）に近い値だった。
- 並列化による短縮幅（p50）: 768 次元で 5.782ms → 1.585ms（約 3.6 倍）、2000 次元で
  15.958ms → 4.270ms（約 3.7 倍）。

**1 回目との差（run-to-run 変動）**: 1 回目の実行では次元比が default（スレッド並列）
= 3.51 倍・CpuScalarProvider = 2.85 倍と、2 回目よりも理論比から離れた値だった
（本ホストは他プロセスと共有する仮想化環境であり、`multi-dim-table-coexistence.md`
と同じ限界を持つ）。1 回目の上振れの原因を「スレッド起動・結合コストが 2000 次元では
相対的に薄まりにくいため」と推測したが、固定オーバーヘッドは計算量の増加とともに
相対的に軽くなる方向に働くはずであり、この説明は筋が通らない。2000 次元 ×
20,000 行のベクトル領域（約 160 MB）は L3 キャッシュを大きく超えるため、12 スレッド
並列がメモリ帯域で頭打ちになり単一スレッドより劣化率が大きくなる、という帯域律速の
仮説の方が説明として妥当と考えられるが、本 ADR ではこの仮説の追加検証は行わない
（原因の切り分けは製品コードへの SIMD/GPU 導入判断（Issue #177/#109/#178）の
管轄）。**次元比の絶対値は 1 回の実測に強く依存するため、判断材料としては
「理論比（2.6 倍）から大きく外れない」という定性的な水準に留める。**

### パート B: 共存状態の end-to-end（`EngineCore::search`、1 テーブルあたり row_count=10,000、k=10）

| config | p50 | p95 | DB ファイルサイズ |
| ------ | --- | --- | ------------------ |
| solo（2000 次元単独） | 2.050ms | 2.260ms | 134,746,112 bytes |
| coexist（768 + 2000 次元共存） | 2.212ms | 2.355ms | 134,746,112 bytes |

- 共存によるオーバーヘッド（p95、coexist vs solo）: +4.2%（1 回目は -0.6%。いずれも
  数 ms オーダーの計測でノイズの範囲内とみられ、明確な劣化とは判断しない）。
- **DB ファイルサイズは両条件で同値だった（2 回とも）**。1 回目は `file_size` を
  `Storage`/`EngineCore` が生存したまま（=redb がまだファイルを確定させていない
  可能性がある時点で）呼んでいたため測定手順の疑義があったが、`core` を `drop`
  してから計測するよう修正した 2 回目でも同一の値（134,746,112 bytes）が再現した。
  データ量で説明すると、solo は 10,000 行 × 2000 次元 × 4 バイト ≒ 80,000,000 bytes、
  coexist は追加で 10,000 行 × 768 次元 × 4 バイト ≒ 30,720,000 bytes が乗るが、
  合計（≒110,720,000 bytes）はいずれも 134,746,112 bytes 未満であり、`redb` が
  ページを大きな単位（成長チャンク）で確保する結果、両条件とも同じチャンク数へ
  切り上げられて同一ファイルサイズになった可能性と整合する（`redb` 内部の
  正確な確保単位までは本 ADR では調査していない。仮説として記録するに留める）。

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
  示さなかった**（パート B、p95 で 1 回目 -0.6%・2 回目 +4.2%。いずれも数 ms
  オーダーの計測でノイズの範囲内）。本ホストは仮想化 CPU 上の共有環境であり、
  `multi-dim-table-coexistence.md` と同じ限界（他プロセスの同時稼働可能性）を
  持つため、この数値は目安に留める。
- **次元スケーリングの比率は 2 回の実測とも理論比（≒2.60 倍）から大きく外れず、
  正しさ側の懸念は見られない**。ただし絶対値は run ごとに変動した（default
  スレッド並列で 1 回目 3.51 倍・2 回目 2.69 倍）。共有仮想化環境での 1 回の実測に
  基づく比率を確定値として扱わない（詳細・上振れ時の仮説はパート A 実測結果の
  コラム参照。製品コードへの SIMD 導入判断は Issue #177 / #109 の管轄）。
- **GPU/CPU-SIMD の相対性能そのものは今回再検証できなかった**。EXT-2 の判断条件は
  `docs/spec/04-behavior/extensions.md`（EXT-2）参照。本 ADR では該当する判断材料を
  提供できていない（下記「制約・スコープ外」参照）。

## 制約・スコープ外

1. **CPU-SIMD / GPU の相対性能再検証（PoC-14 ハーネス）は未実施**。本セッションからは
   private ハーネス（`docs/spec/03-poc/f16-quantization-bandwidth/`。TASK-160・
   PoC-14）にアクセスできなかった。submodule が初期化済みの環境で同ハーネスを
   再実行し、本 ADR の該当節を追記する必要がある（再実行手順・回収すべき測定項目は
   ハーネス側の定義に従う。本 ADR では転記しない）。
2. **実埋め込み分布での再確認は未実施**。本 ADR・`tests/extensions.rs`・
   `examples/high_dim_bench.rs` はいずれも決定論的な合成ベクトル（xorshift32）
   を使う。2000 次元の実埋め込みモデル・データセットの選定はオーナー判断事項。
3. **製品コードでの CPU-SIMD/実 GPU 経路は本 ADR の対象外**。`crates/engine` に
   SIMD カーネルは未実装（`parallel_search.rs` はスレッド並列のみ。Issue #177 /
   #109 の管轄）、実 GPU 経路も未接続（`batch_search.rs::BatchEngine` は f16
   パックの CPU 参照実装。wgpu 導入は Issue #178 の管轄）。パート A の
   「default（スレッド並列）」はスカラー並列単発経路であり、SIMD/GPU の代替
   計測ではない。（追記: SIMD カーネル自体は Issue #109（TASK-156・PR #202）で
   その後導入済み。回帰ベンチの測定対象差し替えは Issue #177 で対応済み。本 ADR の
   歴史的記述は当時の状況としてそのまま残す。）
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
