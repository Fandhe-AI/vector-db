# Phase 4 通しのチップ別前後比較と最速判定（Issue #530）

- ステータス: 実施済み（本開発環境〔共有 QEMU〕参考値のみ。Apple M／AMD Zen／
  Intel 実機の実測はオーナー申し送り）
- 対応: Issue #530（親 #459・ルート #455）
- 前提: [`docs/design/chip-kernel-guidelines.md`](chip-kernel-guidelines.md) §7・
  [`docs/design/benchmark-judgement-policy.md`](benchmark-judgement-policy.md)・
  [`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md)
  「Issue #519 追記」節・[`docs/design/dot-kernel-row-block.md`](dot-kernel-row-block.md)・
  [`docs/design/hnsw-f16-resident.md`](hnsw-f16-resident.md)・
  [`docs/design/hnsw-sq8-resident.md`](hnsw-sq8-resident.md)・
  [`docs/design/dot-kernel-branchless-tail.md`](dot-kernel-branchless-tail.md)

## 1. 背景・目的

Phase 4（チップ最適カーネル群。親 #459・ルート #455）は以下 8 系統を実装した。

| 系統 | Issue | 内容 |
| ---- | ----- | ---- |
| 行ブロック（4 行）カーネル | #510（AVX2+FMA／AVX-512F）・#511（NEON） | `search_range` の複数行同時内積 |
| f16 常駐 | #513〜#516 | HNSW 索引ノードの f16 常駐表現・F16C／NEON fp16 デコード付き dot |
| dim≥768 ACC=4 ディスパッチ | #517〜#519 | `dot_kernel` の複数アキュムレータ化・閾値実測 |
| i8／VNNI 常駐 | #520〜#523 | HNSW 索引ノードの対称 SQ8 常駐・整数 i8×i8 dot（VNNI／widen フォールバック） |
| NEON dotprod（i8） | #524・#525 | aarch64 `vdotq_s32` 版 i8 dot カーネル |
| 分岐なし tail | #527・#528・#529 | 零埋め固定長バッファによる分岐なし tail（既定不採用） |

いずれも本開発環境（QEMU 仮想 CPU。AVX-512／VNNI／NEON なし）では採否判定に
必要な実測ができないため個別 Issue でオーナー実機へ申し送られていた
（`docs/design/chip-kernel-guidelines.md` §7.5・§7.7）。本 Issue は Phase 4 着手前
SHA と適用後 SHA の**通し**前後比較を `make bench-chip`（#469）でチップ別に記録し、
チップごとの最速経路を判定・記録することを目的とする。

**本開発環境には Apple M／AMD Zen／Intel の実機が存在しない**（QEMU 仮想 CPU・
`avx2/fma/f16c` のみ検出）。したがって本 doc が確定できるのは以下のみである。

- (a) before/after 2 状態の**再現可能な**前後比較手順・ドライバの整備
- (b) 本環境（x86_64 QEMU）での実測を「参考値」として記録
- (c) 3 チップ実機の実測欄は「未計測・オーナー申し送り」のまま明記

production コード（`crates/engine/src/`・`crates/wire-server/src/`）は本 Issue
で無変更。

## 2. 計測条件

| 状態 | commit | 選定理由 |
| ---- | ------ | -------- |
| before（Phase 4 着手前） | `91f6a18`（PR #564 ADR #508 の squash commit） | 親 `ff9476a` と `crates/`・`Cargo.lock`・`Cargo.toml`・`rust-toolchain.toml` が完全同一であることを確認済み。`docs/design/hybrid-rrf-phase6-before-after.md` の状態 A と同一 SHA のため Phase 間で基準点を共有できる |
| after（Phase 4 適用後） | `6184491`（`origin/main` HEAD。#614 まで含む） | Phase 4 全 sub-issue マージ後 |

- `git diff --stat 91f6a18 6184491 -- Cargo.lock rust-toolchain.toml` は空
  （依存・toolchain 差分なし）
- 両状態とも `rustc 1.98.1 (48a229cea 2026-09-01)`（`channel = "stable"`）
- 両状態は `git archive` で独立ディレクトリへ書き出しそれぞれ独自の `target/`
  でビルド（ブランチ HEAD 参照は使わない。PR #575 codex 指摘の踏襲）
- `crates/engine/benches/chip_bench.rs`・`benches/harness/chip.rs`・
  `benches/harness/env_report.rs` は before/after で完全同一
  （`git diff --stat 91f6a18 6184491 -- <これらのパス>` が空。計測器が
  before/after で変わっていないことの機械的確認）

### 2.1 交絡の明示

`91f6a18..6184491` には Phase 4 以外の production 変更が約 30 件含まれる
（Phase 2: #563 遅延デコード・#583 可視ビットマップ・#569/#601/#603 二次索引・#592 単文 INSERT／
Phase 3: #574 prefetch・#585 CSR・#596/#600 HNSW 構築・#606/#608/#616/#619／
Phase 5: #567/#578/#591/#598 GPU／Phase 6: #565/#572／wire: #573／#562 広域取得）。
本 doc は区間を次の 3 群に分けて帰属を明記する。

- **Phase 4 に帰属できる区間**: `dot_kernel` ワークロード全点
  （`<working_set>/dim=<n>/{ns_per_dot,median_ms}`）、`knn_profile` の
  `S5_search_parallel`（`search_range` → `dot_block4`）、`dot_kernel_ab` の
  `current`（1 行版 dot の #518 効果）・`block4_ab`／`block4_ab_ref`
  （行ブロックカーネルの #510〜#512）
- **Phase 4 単独には帰属できない区間**: `feature_128`／`feature_768`
  （wide-retrieval・SQL 表層・GPU 等 Phase 2/3/5/6 の変更を含む。§3.3 参照）
- **参照区間（ノイズ帯）**: `knn_profile` の `S1_redb_scan`／`S2_header_decode`
  （dot を通らない redb 走査・ヘッダデコード段。`chip-kernel-guidelines.md`
  §7.3 の指定を踏襲）

## 3. `bench-chip` 前後比較

### 3.1 手順（本環境で実施）

1. `git archive 91f6a18` / `git archive 6184491` を独立ディレクトリへ展開
2. 各状態で `cargo bench --bench dot_kernel_bench -p engine --no-run` →
   `cargo bench --bench knn_profile_bench -p engine --no-run` →
   `cargo build --release -p engine --example feature_bench` →
   `cargo bench --bench chip_bench -p engine --no-run`（計測窓にビルド時間を
   混入させない事前ビルド）
3. `scripts/bench_chip_ab.sh`（新規。本 Issue で追加）を
   `BEFORE_DIR`／`AFTER_DIR`／`AB_PAIRS=5`／`BENCH_CHIP_WORKLOADS=<対象>` で実行
4. `scripts/bench_chip_ab.sh --summarize <dir>` で TSV 集約

`dot_kernel`・`knn_profile` ワークロードはそれぞれ単独（他の `bench_chip_ab.sh`／
`bench_dot_kernel_ab.sh` プロセスを同時起動しない状態）で AB_PAIRS=5 を完走した。
`feature_128`／`feature_768` は本環境の共有負荷・時間制約により今回は実測を
見送った（§8 申し送り）。

### 3.2 `dot_kernel`（Phase 4 に帰属できる区間）

生データ: [`bench-data/phase4-chip-ab/20260907T201653Z-chip-ab-dot-kernel-summary.tsv`](bench-data/phase4-chip-ab/20260907T201653Z-chip-ab-dot-kernel-summary.tsv)

| メトリクス | before min | before median | after min | after median | ratio(min) | ratio(median) | 判定クラス(min) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `arena_scale/dim=100/ns_per_dot` | 7.12 | 11.26 | 6.94 | 8.82 | 0.975 | 0.783 | Neutral |
| `arena_scale/dim=128/ns_per_dot` | 10.56 | 19.61 | 10.35 | 13.60 | 0.980 | 0.694 | Neutral |
| `arena_scale/dim=384/ns_per_dot` | 61.22 | 76.43 | 64.52 | 70.27 | 1.054 | 0.919 | Regressed(min)／Improved(median) |
| `arena_scale/dim=768/ns_per_dot` | 133.59 | 144.30 | 117.37 | 135.11 | 0.879 | 0.936 | Improved |
| `arena_scale/dim=1536/ns_per_dot` | 255.37 | 285.45 | 223.54 | 264.67 | 0.875 | 0.927 | Improved |
| `cache_resident/dim=768/ns_per_dot` | 44.29 | 45.65 | 28.49 | 29.55 | 0.643 | 0.647 | Improved |
| `cache_resident/dim=1536/ns_per_dot` | 105.83 | 107.12 | 55.79 | 57.28 | 0.527 | 0.535 | Improved |
| `diagnostic_ab/simd_vs_scalar_ratio` | 0.176 | 0.178 | 0.112 | 0.115 | 0.636 | 0.646 | Improved |

dim≥768（ACC=4 ディスパッチの対象域。#517〜#519）で cache_resident 側は
最大 47% 改善（min-of-N）、arena_scale 側も 12〜13% 改善。dim=384（ACC=4 の
対象外域）は min では 5.4% 悪化・median では 8% 改善と方向が割れており、
`docs/design/dot-kernel-multi-accumulator.md`「Issue #519 追記」節の既存所見
（`dot_block4` に 8〜11% の退行所見あり）と整合する。閾値 768 は据え置きの
まま。dim=100/128（レーン幅未満）は ±5% 帯に収まり Neutral。

### 3.3 `knn_profile`（帰属可能区間と参照区間）

生データ: [`bench-data/phase4-chip-ab/20260907T201653Z-chip-ab-knn-profile-summary.tsv`](bench-data/phase4-chip-ab/20260907T201653Z-chip-ab-knn-profile-summary.tsv)

| メトリクス | 帰属 | before min | after min | ratio(min) | 判定クラス |
| --- | --- | --- | --- | --- | --- |
| `S1_redb_scan/median_ms` | 参照区間 | 0.975 | 1.204 | 1.235 | Regressed（＝ノイズ） |
| `S2_header_decode/median_ms` | 参照区間 | 1.136 | 1.660 | 1.461 | Regressed（＝ノイズ） |
| `S5_search_parallel/median_ms` | Phase 4 帰属 | 0.299 | 0.640 | 2.140 | Regressed |
| `S0prime_count_star/median_ms` | 参照外（Issue #478 由来） | 1.990 | 0.054 | 0.027 | Improved（Phase 4 外） |

**重要な所見**: dot を一切通らない参照区間（`S1_redb_scan`／`S2_header_decode`）
自体が min-of-5 比で +23.5%／+46.1% の「悪化」を示した。これは production の
退行ではなく、**本開発環境（複数の並行タスクが同居する共有 QEMU ホスト）の
run-to-run ノイズが `benchmark-judgement-policy.md` §4 の固定 ±5% 帯を大きく
超える**ことを実測で確認したものである（`bench_chip_ab.sh` 単体を単独実行
していても、同一ホスト上の他プロセスの影響は排除できない）。したがって
`S5_search_parallel` の ratio(min)=2.14 も、Phase 4（行ブロックカーネル）由来の
効果と環境ノイズを本環境の 1 セッションでは分離できない。`dot_kernel`
ワークロード単体（3.2 節）の方が測定対象の粒度が細かく working_set 別の内訳が
取れるため、Phase 4 の効果判定は 3.2 節を主、本節は「参照区間のノイズ実測値」
としての参考に留める。

`S0prime_count_star`（`COUNT(*)` 経路）の劇的改善（min-of-N 比 27 分の 1）は
Phase 4 の対象外（Issue #478 VisibleBitmapCache・Phase 2 由来）であり、本 Issue
の評価対象ではない。

## 4. `dot_kernel_ab`（`current` 前後・block4／tail A/B）

生データ: [`bench-data/phase4-chip-ab/20260907T201653Z-dot-kernel-ab-summary.tsv`](bench-data/phase4-chip-ab/20260907T201653Z-dot-kernel-ab-summary.tsv)

`scripts/bench_dot_kernel_ab.sh`（Issue #519 で新設済みの既存スクリプト）を
before/after の `dot_kernel_bench` バイナリで N=5 交互実行した。この実行は
`knn_profile` の 1 回目の試行（後に破棄・§8 参照）と一部時間帯が重複しており、
`current` 系列の一部ペア（pair1・pair5 の before）に外れ値
（cache_resident dim=100 が通常 0.14〜0.19ms のところ 0.57〜0.65ms）が
混入している。min-of-N は外れ値に頑健なため 3.2 節の結論への影響は小さいと
判断するが、本節の `block4_ab`／`block4_ab_ref` 行は参考値として扱う。

`block4_ab`（after 内 A/B。1 行版 vs block4 版）の ratio は 5 ペア×2 working set
（cache_resident／arena_scale）×2 dim（128／768）の 20 観測中、0.93〜1.10 の
範囲に収まった（block4 版が dim=128 で概ね 5〜7% 高速・dim=768 は概ね ±1%）。
`block_ref`（プロセス内反復のノイズ参考値）は 0.04〜1.84 と広く散らばり、
これも本開発環境の測定ノイズが大きいことを裏付ける。

## 5. 精度 3 arm（f32／f16／i8）

`docs/design/hnsw-f16-resident.md`「Issue #516 追記」節・
`docs/design/hnsw-sq8-resident.md`「Issue #523 追記」節・
`docs/design/chip-kernel-guidelines.md` §7.7（Issue #526）に既存の実測（本環境・
`make bench-knn-precision-resident`）が記録済みのため、本 Issue では新規実測を
行わず引用する。要点（詳細は各 doc 参照）:

- f16 常駐: 常駐メモリの削減方向を確認済み（f32 比 約半分オーダー）
- i8（SQ8）常駐: 常駐メモリの削減方向を確認済み（f32 比でさらに削減）
- 3 精度いずれも Recall 3 ゲート（hybrid・rerank・query-planning）で
  brute_force 対照との完全一致を確認済み（#412・#515・#523）
- 本環境（QEMU x86_64）でのスモーク実測は `kernel_isa dot=Avx2Fma f16=F16c
  i8=Avx2Widen`（Issue #526 記録）——AVX-VNNI／NEON 経路は本環境では検証不能

## 6. crossdb `vector_knn` 再計測

本 Issue の計測時間予算（§2〜§4 の交互実行に時間を要した）・共有環境での
`vector_knn` 系フェーズが Phase 2/3/5/6 変更を多く含み Phase 4 単独に
帰属できない（§2.1）ことを踏まえ、**本 Issue では self A/C 再計測を実施しな
かった**。`docs/design/crossdb-bench.md` の既存記録（`vector_knn` 786µs・
Qdrant HNSW 目標 559µs）を参照値として引用するに留める。再計測はオーナー
実機での §9 手順の一部として実施可能。

## 7. 判定（最速判定）

契約クラス別に整理する。

### 7.1 既定 brute-force エンジン（結果ビット同一契約）

- **f32 1 行版 → f32 block4 版**が Phase 4 で production 既定を変えた唯一の
  経路である。ただし block4 版はまだ既定切替されていない（`SimdKernel::dot`
  の既定は 1 行版のまま。行ブロックカーネルは `search_range` 経路にのみ配線
  済みで、既定切替の採否は個別 Issue のスコープ外）
- 分岐なし tail（#527〜#529）は既定不採用のまま（Issue #529 の判断を継承）

### 7.2 ANN opt-in（`SearchEngineKind::Hnsw`）

- `hnsw`（f32）／`hnsw_f16`／`hnsw_i8` の 3 候補生成経路が opt-in で選択可能
  （最終スコアは常に `kernel::dot` の f32 再計算）
- 3 経路とも Recall 3 ゲート同一閾値を通過済み（§5）
- 常駐メモリは f32 > f16 > i8 の順（各既存 doc 参照）
- 探索レイテンシの絶対優劣はチップの `kernel_isa`（VNNI／NEON dotprod の
  実機可用性）に依存するため、本環境だけでは決定できない

### 7.3 3 チップ表

| チップ | ISA 検出（`engine::isa`） | `kernel_isa`（dot／f16／i8） | 最速経路（既定 brute-force） | 最速経路（ANN opt-in・候補生成） | 根拠 | 判定 |
| --- | --- | --- | --- | --- | --- | --- |
| **本環境（x86_64 QEMU・参考値）** | `Avx2Fma` | dot=Avx2Fma／f16=F16c／i8=Avx2Widen | dim≥768: block4 版が min-of-N で 12〜47% 高速（§3.2）。dim<384: Neutral〜方向不定 | 3 arm とも Recall 完全一致（§5）。VNNI 非搭載のため i8 の整数カーネル効果は未計測 | §3.2・§4・Issue #526 | **参考値・採否根拠にしない**（`benchmark-judgement-policy.md` §5） |
| **Apple M1／M2／M3／M4** | （未計測） | （未計測。期待値: dot=Neon／f16=NeonFp16／i8=NeonDotprod。Issue #525 実装済み） | 未計測・オーナー申し送り | 未計測・オーナー申し送り | — | **未計測** |
| **AMD Zen 4**（Ryzen 7000／EPYC Genoa） | （未計測） | （未計測。期待値: `Avx512f`／`Avx512Vnni` 検出見込み） | 未計測・オーナー申し送り | 未計測・オーナー申し送り | — | **未計測** |
| **AMD Zen 5**（デスクトップ／EPYC Turin／Ryzen AI 300） | （未計測） | （未計測。期待値: Zen 4 同様＋拡張命令） | 未計測・オーナー申し送り | 未計測・オーナー申し送り | — | **未計測** |
| **Intel**（Ice Lake-SP／Sapphire Rapids／Emerald Rapids／Alder Lake〜Arrow Lake） | （未計測） | （未計測。世代により `Avx512Vnni`／`AvxVnni` のいずれかが期待される） | 未計測・オーナー申し送り | 未計測・オーナー申し送り | — | **未計測** |

QEMU 行の結論は共有仮想環境の参考値に過ぎず、production の既定切替を推奨
するものではない。既定切替（f16／i8 常駐化・分岐なし tail 採用・ACC=4
既定閾値の見直し）はいずれも個別 Issue（#519・#523・#516・#529）の判断どおり
オーナー判断へ申し送り済みであり、本 Issue でも新たな推奨は行わない。

## 8. 限界・申し送り

- **3 チップ実機の実測値そのもの**（Apple M／AMD Zen 4・5／Intel）はオーナー
  作業。手順は §9 参照
- `feature_128`／`feature_768` ワークロードは本 Issue の時間予算内で実測を
  見送った（§3.1）。`bench-chip-ab`（本 Issue で追加した Makefile ターゲット）
  で `BENCH_CHIP_WORKLOADS=feature_128,feature_768` 指定により追加実測可能
- crossdb `vector_knn` self A/C 再計測は未実施（§6）
- `knn_profile` ワークロードの参照区間（`S1_redb_scan`／`S2_header_decode`）
  自体が本環境で ±23〜46% の run-to-run 変動を示した。専有環境
  （`BENCH_DEDICATED_ENV=1`）での再実測がなければ `S5_search_parallel` 等の
  ratio は Phase 4 由来の効果と環境ノイズを分離できない
- `DOT_MULTI_ACC_MIN_DIM`（768）の最終確定・block4 経路の dim=384 での
  min-of-N 方向不定（§3.2）への対処は Issue #519 の申し送りを継承しオーナー
  判断のまま
- f16／i8 既定常駐化・`DEFAULT_PADDED_TAIL` 切替は既存の個別判断（#515・
  #516・#523・#529）どおり本 Issue でも提案しない

## 9. 再現手順

### 9.1 本 doc の再現（任意環境共通）

```sh
# 1. 2 状態を独立ディレクトリへ展開（ブランチ HEAD 参照は使わない）
git archive 91f6a18 | tar -x -C <state-before>
git archive 6184491 | tar -x -C <state-after>

# 2. 事前ビルド（計測窓にビルド時間を混入させない）
cd <state-before> && cargo bench --bench dot_kernel_bench -p engine --no-run \
  && cargo bench --bench knn_profile_bench -p engine --no-run \
  && cargo build --release -p engine --example feature_bench --example seed_docs \
  && cargo bench --bench chip_bench -p engine --no-run
cd <state-after>  && (同上)

# 3. chip_bench 前後比較
BEFORE_DIR=<state-before> AFTER_DIR=<state-after> AB_PAIRS=5 \
  make bench-chip-ab
scripts/bench_chip_ab.sh --summarize _/bench/chip-ab/<UTC ts>

# 4. dot_kernel_bench current/block4/tail A/B（既存スクリプト。Issue #519）
BEFORE_BIN=<state-before>/target/release/deps/dot_kernel_bench-<hash> \
AFTER_BIN=<state-after>/target/release/deps/dot_kernel_bench-<hash> \
  scripts/bench_dot_kernel_ab.sh 5

# 5. 精度 3 arm（f32/f16/i8。既存スクリプト。Issue #526）
AB_CANDIDATE_ENGINES="hnsw_f16 hnsw_i8" AB_PAIRS=5 \
  make bench-knn-precision-resident
```

### 9.2 チップ別のオーナー向け手順

1. `make detect-features`（`.github/workflows/detect-features.yml`
   `detect-apple` ジョブと同型）を最初に実行し、`kernel_isa` の期待値を確認する
   - **Apple**: `kernel_isa dot=Neon f16=NeonFp16 i8=NeonDotprod` を期待
   - **AMD Zen 4／5・Intel**（VNNI 搭載世代）: `dot=Avx512F`（または
     `Avx2Fma`）・`i8=Avx512Vnni`（非搭載世代は `AvxVnni` または
     `Avx2Widen`）を期待。`runtime_features` の `avx512f`／`avx512vnni`／
     `avxvnni` の実測値と一致するか確認する
2. 上記 §9.1 の手順をそのまま実行する（`AB_PAIRS` は既定 5 を推奨。
   `BENCH_DEDICATED_ENV=1` を各コマンドへ付与できるなら付与する）
3. 結果を本 doc §7.3 の 3 チップ表・§3〜§6 の該当節へ追記し、
   `docs/design/chip-kernel-guidelines.md` §7.5 の空テンプレートへも転記する
