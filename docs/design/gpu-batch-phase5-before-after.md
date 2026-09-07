# Phase 5 通し前後比較（bench-gpu-scaling・FAISS GPU 対照・Qdrant GPU 構築）

- **ステータス**: Recorded（記録専用・参考値。共有 QEMU 環境での計測のため
  `docs/design/benchmark-judgement-policy.md` §5 により Accepted/Rejected の
  採否根拠にはしない）
- **対応 Issue**: #544（本書）・親 #460（Phase 5 トラッキング）・ルート #455
- **関連ポインタ（spec・本文は転記しない）**: TASK-128〜130・CORE-6・CORE-16

## 1. 背景・目的

Phase 5（親 #460／ルート #455）で `engine::gpu_batch` に入った production 変更は
4 件。各実装 PR に付随する前後比較（#533・#537・#540・#543）は**変更単位**の
局所比較に留まっている。

| Issue | PR / commit | 変更 | 既存の局所比較 |
| --- | --- | --- | --- |
| #532 | #567 / `59bc7a8` | クエリタイル化（1 dispatch で最大 16 クエリ） | #533（`crossdb-bench.md`「#532 クエリタイル化の前後比較」節。before `b161d5b`） |
| #536 | #578 / `4ece69e` | workgroup 内部分 Top-k ＋ CPU 最終マージ（readback 12.7x 削減） | #537（`gpu-batch-topk.md`「前後比較実測」節。before `895e6cd`） |
| #539 | #591 / `ab32ccf` | `SHADER_F16` 対応時の f16 算術版 S0 シェーダ（f16 厳密往復クエリ時のみ選択） | #540（`gpu-batch-f16-arith.md` §8。in-process A/B） |
| #542 | #598 / `e14d53f` | i8 パック常駐 ＋ `dot4I8Packed`（opt-in・候補生成専用・primary 未接続） | #543（`gpu-batch-i8-packed.md` §8。before 不在） |

本書は Phase 5 着手前（#532 直前）→ 全適用後を**通しで** 1 セッション内に計測し、
FAISS GPU（`IndexFlatIP`）・Qdrant GPU 索引構築との対照値を同一セッションで
更新し、Apple UMA（Metal）でのゼロコピー可否を Issue #400 の先例に倣って
**実装前に静的確認**した結果を記録する。

## 2. 計測条件

| 状態 | commit | 内容 |
| --- | --- | --- |
| before（Phase 5 着手前） | `b161d5b` | #567 マージの親（#533 と同一 before） |
| after | `a40edd7` | 作業ブランチ作成時点の `origin/main`（Phase 5 全 4 変更を含む） |

- 両状態は `git archive <hash>` で個別ディレクトリへ展開し、独立した
  `CARGO_TARGET_DIR` で `cargo bench --bench gpu_scaling_bench -p engine
  --no-run --message-format=json` によりビルドした（release プロファイル・
  同一 `rustc 1.96.0`）。
- **ビルド条件の同一性**: `git diff --stat b161d5b a40edd7 -- Cargo.lock
  rust-toolchain.toml Cargo.toml crates/engine/Cargo.toml` は
  `crates/engine/Cargo.toml` への `[[bench]] hnsw_search_bench`（Issue #491）
  の追加のみ（依存変更なし）。`git diff --stat b161d5b a40edd7 --
  crates/engine/benches/gpu_scaling_bench.rs
  crates/engine/benches/harness/gpu_scaling.rs` は A/B/C 計測本体
  （`gpu_scaling:` 行・`MeasurementConfig`・warmup 20）に対して追加のみ
  （i8・stats・shader_ab 行の追加。import の並び替えを除き削除なし）である
  ことを確認した。
- **交絡の明示**: `b161d5b..a40edd7` には Phase 4 の CPU 変更（`isa.rs`・
  `batch_search.rs`・`kernel.rs`）が混入するため、CPU-SIMD 経路
  （`cpu_p50`/`cpu_p95` 列）は「変更を含まない参照区間」ではない。本書の
  参照区間ノイズ帯は **side ごと**（before のみ n=5／after のみ n=5）の
  run-to-run 幅として算出し、両方を併記したうえで大きい方を判定に使う
  （`benchmark-judgement-policy.md` §3〜§4。#533・#537 が使った
  「before+after pooled の `cpu_p50`」方式はそのまま流用できないため
  side 別方式へ変更した）。CPU-SIMD 自体の before/after 差は Phase 4 の
  副産物であり Phase 5 の所見には数えない（#530 の担当）。
- **after が測る経路の定義（非 vacuity）**: 既定クエリ（`QUERY_F16_EXACT`
  未設定）での f16 経路 ＝ タイル化（#532）＋部分 Top-k（#536）＋ unpack 版
  S0 シェーダ。f16 算術版（#539）は f16 厳密往復ガードにより構造的に
  非選択、i8（#542）は opt-in・primary 未接続。主表は production 既定経路の
  前後比較とし、#539／#542 の効果は再計測せず `gpu-batch-f16-arith.md`
  §8.3.1・`gpu-batch-i8-packed.md` §8 を引用する。全 6 規模点の after 側
  `gpu_scaling_stats:` 行（`docs/design/bench-data/gpu-scaling-ab/
  20260907T124155Z-stats.txt`）で `f16_full_readback_dispatches=0`・
  `f16_full_readback_fallbacks=0`（全量 readback への縮退なし）・
  `f16_arith_dispatches=0`・`f16_arith_guard_fallbacks=40`（既定クエリでは
  常に unpack 版が選ばれる契約どおり）を確認した。i8 側も全 6 点で
  `i8_mismatch=0`・`i8_recall_at_k=1.0000`（`i8_status=measured`）を確認済み。
- 環境: `lscpu` Model name `QEMU Virtual CPU version 2.5+`（KVM）・12 vCPU・
  GPU `NVIDIA GeForce RTX 3060`（driver 595.71.05）・`BENCH_DEDICATED_ENV`
  未設定。**専有環境ではないため Accepted/Rejected の採否根拠にしない**
  （`benchmark-judgement-policy.md` §5）。
- 各 run の生データ: `docs/design/bench-data/gpu-scaling-ab/
  20260907T124155Z-summary.tsv`（60 行＝6 点 × 5 ペア × 2 側）・
  `20260907T124155Z-env.txt`・`20260907T124155Z-stats.txt`（非 vacuity 根拠）・
  `20260907T124155Z-faiss-run{1..5}.json`・
  `20260907T124155Z-qdrant-{cpu,gpu}.json`。

## 3. `bench-gpu-scaling` 規模点の前後比較（交互 5 ペア・min-of-5／median-of-5）

`gpu_f16_p95`（GPU f16 経路・p95）の min-of-5・median-of-5、参照区間
（side 別 `cpu_p50` の run-to-run 幅）、固定 ±5% 帯・実測帯の両方を超える
場合のみ有効な変化として扱う判定（`benchmark-judgement-policy.md` §4）。

| rows:dim:batch | before f16p95 (min/med) µs | after f16p95 (min/med) µs | ratio (min-of-N) | 固定帯判定 | 参照帯 before | 参照帯 after | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100000:128:64 | 81704 / 82953 | 16355 / 17324 | 0.200 | Improved | 1.38% | 2.07% | 両帯超過・**Improved** |
| 20000:128:8 | 2756 / 3565 | 602 / 912 | 0.218 | Improved | 132.70% | 15.87% | 実測帯内・**判定不能**（before 側 `cpu_p50` が 5272〜12268µs と共有環境ノイズで大きく振れた） |
| 20000:128:64 | 11631 / 22704 | 3849 / 4515 | 0.331 | Improved | 3.86% | 163.69% | 実測帯内・**判定不能**（after 側 `cpu_p50` が 17063〜44994µs と 1 run 外れ値混入） |
| 100000:128:1 | 1258 / 1328 | 1391 / 1520 | 1.106 | Regressed | 1.18% | 10.76% | 実測帯内（僅差）・**判定不能**（`crossdb-bench.md` #533 節が既に「batch=1 は中立」と記録している点と整合） |
| 100000:128:256 | 327310 / 332163 | 63124 / 64074 | 0.193 | Improved | 0.65% | 1.31% | 両帯超過・**Improved** |
| 500000:128:64 | 441024 / 444539 | 79013 / 79519 | 0.179 | Improved | 0.57% | 0.56% | 両帯超過・**Improved** |

6 点中 3 点（`100000:128:64`・`100000:128:256`・`500000:128:64`）で固定帯・
実測帯の両方を超える一貫した改善（比 0.18〜0.20x。Phase 5 全 4 変更の通し
効果として 5〜5.6 倍高速化）を確認した。残り 3 点は共有 QEMU 環境のノイズ
（`cpu_p50` の run-to-run 幅が実測帯として 10〜164% と大きい）により実測帯内
にとどまり判定不能——これは production 変更が効いていないという意味ではなく、
本環境・本ペア数では変化を環境ノイズから切り分けて確定できないことを示す
（`benchmark-judgement-policy.md` の fail-closed な判定規約どおり）。
`100000:128:1` は `crossdb-bench.md`「#532 クエリタイル化の前後比較」節が
記録した局所比較でも中立と判定されており、本書の通し比較でも一貫している。

`mismatch`（`count_boundary_tolerant_mismatches` が境界同点を許容した後に
残った不一致数。GPU f16 vs CPU-SIMD・GPU f32 vs CPU-SIMD 両方の合算値）は
`100000:128:*`・`500000:128:64` の 4 点で全 run 0、`20000:128:8`・
`20000:128:64` の 2 点では before/after いずれも一貫して 1。before/after で
件数が同一であるという事実のみを確認済みで、原因（f16 側か f32 側か・
どの境界条件か）は未確認。`i8_recall_at_k` は全点全 run で 1.0000。

### 各 run の生データ

`docs/design/bench-data/gpu-scaling-ab/20260907T124155Z-summary.tsv`
（60 行）を参照。

## 4. FAISS（IndexFlatIP）GPU 対照

`scripts/crossdb_bench/gpu/faiss_batch_bench.py --rows 20000 100000 500000
--dims 128` を 5 プロセス独立起動し、`p50_us` の min-of-5・median-of-5 を
`rows=100000, dim=128, batch=64`（self の主計測点と同一格子点）で集計した。

| 経路 | run1〜5 p50 (µs) | min | median |
| --- | --- | --- | --- |
| FAISS CPU | 33950, 39949, 34945, 34947, 33060 | 33060 | 34945 |
| FAISS GPU f32 | 658, 633, 685, 666, 657 | 633 | 658 |
| FAISS GPU f16 | 761, 759, 779, 781, 766 | 759 | 766 |

self（after）の `100000:128:64` GPU f16 p95 min-of-5 = 16355µs（§3 表。
本書の主計測点では before 81704µs → after 16355µs と Phase 5 全体で約 5 倍
改善している）を FAISS GPU f32 min（633µs。統計量の単位が異なる点に注意
——self は p95、FAISS は p50 であり、単純倍率は参考値）と比べると約 25.8 倍。
FAISS GPU f16 median（766µs）との比較では約 21.3 倍。この対 FAISS 倍率自体は `crossdb-bench.md`「GPU 節」が Issue #537 時点（#532 タイル化・#536 部分 Top-k 適用後）で記録した「約 21 倍」（p95 vs FAISS `559b523` 時点値 753µs）から大きく変わらない水準にとどまる。ただし Issue #537 時点は既に #532・#536 適用後であり、この横ばいは Phase 5 全体の効果ではなく、**Issue #537 以降に加わった変更（#539 の f16 算術版・#542 の i8）が既定経路で非選択のため FAISS との差の縮小に寄与していない**ことを示す（#532・#536 自体の寄与は主計測点の before/after 改善に含まれている）。engine 側は Top-k を CPU で行いスコアバッファを
読み戻す構造（GPU→CPU 転送量が `rows × batch × 4` バイト。#536 で
partial Top-k 化済みだが k×workgroup 数×8 バイトへの縮小に留まる）が
引き続きボトルネックと推定される。

FAISS 計測条件: `cpu_condition=blas_disabled`（`distance_compute_blas_threshold`
を無効化し SIMD 直接計算に固定）・`omp_num_threads=12`・`faiss-gpu-cu12
==1.14.1.post1`。各 run の生データ: `docs/design/bench-data/gpu-scaling-ab/
20260907T124155Z-faiss-run{1..5}.json`。

## 5. Qdrant GPU 索引構築 対照

`qdrant/qdrant:v1.19.1`（CPU）・`qdrant/qdrant:v1.19.1-gpu-nvidia`（GPU。
`QDRANT__GPU__INDEXING=1`）を `containers_gpu.sh` で個別起動し（ポート
17333/17334・コンテナ名 `bench-qdrant-cpu-gpucmp`／`bench-qdrant-gpu`。他
セッションの既存 `bench-qdrant`〔ポート 16333/16334〕とは分離）、
`qdrant_gpu_build_bench.py --rows 100000 500000` で計測した。判定を行わない
対照値のため各構成 1 回のみ実行（N=5 は課さない）。

| rows | 構成 | upsert | index_build | search p50 / p95 µs | GPU ログ検出 |
| --- | --- | --- | --- | --- | --- |
| 100,000 | cpu | 1.69 s | 3.02 s | 1289 / 1597 | False |
| 500,000 | cpu | 8.85 s | 20.59 s | 952 / 1177 | False |
| 100,000 | gpu | 1.67 s | 5.04 s | 1275 / 1761 | True |
| 500,000 | gpu | 8.65 s | 27.59 s | 945 / 1121 | True |

100k で GPU 5.0s vs CPU 3.02s、500k で GPU 27.6s vs CPU 20.6s と、
`crossdb-bench.md` が記録した先行実測（`559b523` 時点。100k: 5.0s vs 3.0s、
500k: 21.1s vs 20.6s）と同じ傾向——この規模・この VM では GPU 索引構築の
利得は無い（むしろ 500k では GPU が CPU より遅い）。GPU 使用は
`gpu_log_signal.create_gpu_device_line=true`（コンテナログの
`Create GPU device` 行。`Found GPU device` は列挙のみで証拠にしない契約を
維持）で両規模点とも確認した。`qdrant_server_version` は CPU/GPU 両コンテナ
とも `1.19.1` で一致。

各 run の生データ: `docs/design/bench-data/gpu-scaling-ab/
20260907T124155Z-qdrant-{cpu,gpu}.json`。

## 6. UMA（Apple Metal）ゼロコピーの静的確認

本環境に Apple 実機は無く実測不可。Issue #400
（`docs/design/redb-insert-reserve-zero-copy.md`）の先例どおり、wgpu =30.0.1
のソースを根拠に**静的確認のみ**を行った（production 実装は行わない）。

確認した事実（wgpu 30.0.1 ソース。パスは crates.io レジストリ配下の相対パス）:

- `wgpu-hal-30.0.1/src/metal/device.rs::create_buffer`（465〜475 行付近）:
  `usage` が `MAP_READ`／`MAP_WRITE` を含む場合のみ `StorageModeShared`、
  それ以外は `StorageModePrivate`。
- `wgpu-core-30.0.1/src/device/queue.rs::write_buffer`（631〜685 行付近）:
  backend を問わず `StagingBuffer::new` → `staging_buffer.write(data)` →
  `write_staging_buffer_impl`（デバイスバッファへの blit）という host →
  staging → device の経路を通る（`gpu_batch.rs`／`packed_i8.rs` の行列・
  params・row_ids・query の全アップロードがこの `queue.write_buffer` を使用）。
- readback: `copy_buffer_to_buffer`（`STORAGE|COPY_SRC` → `MAP_READ|COPY_DST`）
  → `map_async`（`gpu_batch.rs::wait_and_read_buffer`）。
- `wgpu-types-30.0.1/src/features.rs::MAPPABLE_PRIMARY_BUFFERS`（748〜762 行
  付近）: usage 組み合わせ制約を外す feature。ドキュメントコメントに
  「共有メモリ環境以外では性能を著しく損ねうる」と明記。
- `wgpu-hal-30.0.1/src/metal/adapter.rs`: `hasUnifiedMemory()`
  （1066〜1068 行付近）の呼び出し結果が `PrivateCapabilities::device_type()`
  （1596〜1602 行付近）で `IntegratedGpu`／`DiscreteGpu` の判定に使われる。
  アプリからは `adapter.get_info().device_type` で間接的に観測できる。

結論:

1. host `Vec` → GPU バッファの**真のゼロコピー（memcpy ゼロ）は wgpu 30.0.1
   の API 上成立しない**（`mapped_at_creation` を使っても staging 経由の
   1 回の memcpy は必須という構造）。
2. UMA で削減可能なのは readback の blit 1 段（#536 後は k×workgroup 数×8
   バイトまで縮小済みで効果は限定的）とアップロードの staging 1 段
   （構築時 1 回）。
3. 採用する場合のゲート条件は `Backend::Metal &&
   DeviceType::IntegratedGpu` かつ `MAPPABLE_PRIMARY_BUFFERS` 対応時のみ
   （discrete GPU では fail-closed に既定経路のまま）。
4. 本 Issue では**production 変更を行わず**、Apple 実機での実測を伴う
   将来 Issue 候補として記録する（起票はオーナー判断。
   `out-of-scope-tracking` に従い勝手に起票しない）。

## 7. 判定

- **確定的カウンタ**: 全 6 規模点で `full_readback_dispatches=0`・
  `full_readback_fallbacks=0`（全量 readback への縮退なし）・
  `i8_mismatch=0`・`i8_recall_at_k=1.0000` を確認——これらは環境ノイズの
  影響を受けない確定的事実として**確定**できる。
- **レイテンシ（参考値）**: 6 点中 3 点（`100000:128:64`・`100000:128:256`・
  `500000:128:64`）で固定帯・実測帯の両方を超える一貫した改善（0.18〜0.20x。
  約 5〜5.6 倍高速化）を観測。残り 3 点（`20000:128:8`・`20000:128:64`・
  `100000:128:1`）は共有環境のノイズにより min-of-5 を主統計量とする判定では
  確定できない。ただし `100000:128:1` は全 5 ペアで after の f16 p95 が
  before を上回り（median 比約 1.145）、実測帯 10.76% を超える悪化方向の
  シグナル自体は観測された——主判定（min-of-5・両ノイズ帯超過）では
  確定的な Regressed とは判定していないが、悪化方向のシグナルが無かった
  わけではない点に注意。
- **FAISS 対照**: self（after）は FAISS GPU に対しなお約 21〜26 倍遅い。
  主計測点自体は Phase 5 全体（#532・#536 込み）で before 比約 5 倍改善して
  いるが、対 FAISS 倍率は Issue #537 時点（#532・#536 適用後）の局所値から
  横ばいであり、#537 以降に加わった変更（#539・#542）は既定経路で非選択の
  ため対 FAISS の差の縮小には寄与していない。
- **Qdrant 対照**: GPU 索引構築はこの規模・この VM では CPU 構築に対し
  優位性なし（先行実測と同じ傾向を再確認）。
- **UMA**: 静的確認のみ。production 変更なし。
- 上記により、本書のステータスは **Recorded**（記録専用・参考値）のまま
  据え置く。専有環境での確定判定・FAISS との差の解消（GPU 側 Top-k 選出等）
  はいずれもオーナー作業・別 Issue へ申し送る。

## 8. 限界・申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測・CORE-6／CORE-16 の絶対閾値
  判定はオーナー作業として引き続き未実施。
- Apple 実機（Metal・UMA）での実測と UMA ゼロコピー経路の実装は §6 の静的
  確認の結論に基づく将来 Issue 候補（起票はオーナー判断）。
- 既定格子 24 点のうち残り 18 点（dim=256 全点・batch=256 の一部等）の計測は
  対象外。
- i8 経路のクエリタイル化・部分 Top-k、`DEFAULT_I8_OVERSAMPLE` 変更の採否は
  `gpu-batch-i8-packed.md` §9 の既存申し送りのまま。
- Phase 4 CPU 変更（`isa.rs`／`batch_search.rs`）による CPU-SIMD 側の変化は
  #530（Phase 4 通し比較）の担当。
- `bench-*` への交互ペア自動集計の組み込みは規約 §9 の別 Issue 候補。

## 9. 再現手順

Bash の変数代入ではパス名展開（glob）が行われないため、ビルド後の実行
ファイルパスは Cargo の `--message-format=json` 出力から `jq` 等で解決した
完全パスを使う（プレースホルダーのまま `*` を変数へ代入しても文字列として
渡り、`bench_gpu_scaling_ab.sh` の実行可能ファイル検査で停止する）。

```bash
git archive b161d5b | tar -x -C <before-dir>
git archive a40edd7 | tar -x -C <after-dir>
BEFORE_BIN=$(cd <before-dir> && CARGO_TARGET_DIR=<before-target> cargo bench \
  --bench gpu_scaling_bench -p engine --no-run --message-format=json \
  | jq -r 'select(.executable != null) | .executable')
AFTER_BIN=$(cd <after-dir> && CARGO_TARGET_DIR=<after-target> cargo bench \
  --bench gpu_scaling_bench -p engine --no-run --message-format=json \
  | jq -r 'select(.executable != null) | .executable')
BEFORE_BIN="$BEFORE_BIN" AFTER_BIN="$AFTER_BIN" OUT_DIR=<out-dir> \
  scripts/bench_gpu_scaling_ab.sh 5 100000:128:64 20000:128:8 20000:128:64 \
    100000:128:1 100000:128:256 500000:128:64
```

FAISS 対照（5 回。リポジトリルートで実行する前提。`docker run -v` は
絶対パスのみ受け付けるため `$(pwd)` でバインドマウント元を解決する）:

```bash
docker run --rm --gpus all -v <scratch>:/work \
  -v "$(pwd)/scripts/crossdb_bench/gpu:/scripts:ro" bench-faiss-gpu \
  python /scripts/faiss_batch_bench.py --rows 20000 100000 500000 --dims 128 \
    --out /work/results/faiss-run<N>.json
```

Qdrant GPU 対照:

```bash
docker pull qdrant/qdrant:v1.19.1
docker pull qdrant/qdrant:v1.19.1-gpu-nvidia
scripts/crossdb_bench/gpu/containers_gpu.sh up qdrant_cpu
python scripts/crossdb_bench/gpu/qdrant_gpu_build_bench.py --rows 100000 500000 \
  --out qdrant-cpu.json --label cpu --container-name bench-qdrant-cpu-gpucmp
scripts/crossdb_bench/gpu/containers_gpu.sh down qdrant_cpu
scripts/crossdb_bench/gpu/containers_gpu.sh up qdrant_gpu
python scripts/crossdb_bench/gpu/qdrant_gpu_build_bench.py --rows 100000 500000 \
  --out qdrant-gpu.json --label gpu --container-name bench-qdrant-gpu
scripts/crossdb_bench/gpu/containers_gpu.sh down qdrant_gpu
```
