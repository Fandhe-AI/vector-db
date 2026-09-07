# `isa.rs` dot カーネルの複数アキュムレータ化検討

- ステータス: Issue #365 時点は**現状維持で close 可**（全 dim 一律の ACC=4 化は
  不採用）。Issue #517/#518 で条件付き再訪し、**dim 閾値（768）による ACC=4
  経路のディスパッチとして採用**（「Issue #518 追記」節参照）
- 対応: Issue #365（`perf(engine): isa.rs dot カーネルの複数アキュムレータ化`）・
  Issue #517（親）・Issue #518（実装）
- 前提: Issue #362（`docs/design/knn-stage-profile.md`「`dot_lanes` の実
  アセンブリ確認」節）で、AVX2+FMA 環境の `dot_avx2_fma` が単一 FMA 依存チェーン
  （4 段 unroll だが実質 1 系統）に律速されていることが判明済み

## 背景

`crates/engine/src/isa.rs` の `dot_lanes<LANES>` は `LANES` 個の `f32::mul_add`
アキュムレータへ積算するが、Issue #362 の逆アセンブル確認により AVX2+FMA 経路
（`dot_avx2_fma`）はこの `LANES` 本のレーンが単一の依存チェーンとして生成され、
複数の FMA 実行ポートを活かせていないことが判明した。hnswlib・Qdrant 等の実装は
複数の独立アキュムレータ（2〜4 本）で積算し FMA レイテンシではなくスループットに
律速させる構成を採る。本 Issue はこの構成への変更を検討し、マイクロベンチで
効果を確認したうえで採否を判断する。

## 設計: 2 段構造

`dot_lanes` を単一 `LANES` 幅から `WIDE`（= `LANES * DOT_ACCUMULATORS`）幅の
平坦アキュムレータ配列へ拡張し、`WIDE` 未満の端数を `LANES` 幅で処理する 2 段
構造を検討した。`WIDE` 個のレーンは互いに独立なため、LLVM が
`DOT_ACCUMULATORS` 本の独立 FMA チェーンへ展開することをスクラッチプロトタイプ
（`docs/spec` に依存しないリポ外の使い捨てクレート。§「参考プロトタイプ実測」）
で確認した。

`generic_const_exprs` が unstable のため `dot_lanes::<{LANES*ACC}, LANES>` の
ような const 式は const generic 引数の位置に書けず、`WIDE` を独立の const
generic として渡す形にした（呼び出し側の `dot_neon`/`dot_avx2_fma`/`dot_avx512`
内で `const WIDE: usize = LANES * DOT_ACCUMULATORS;` を計算してから渡す）。

`unsafe` の範囲（[`SimdKernel::dot`] 内の 3 箇所）・`#[target_feature]` 構造は
変更しない。演算順序の provider 間整合（同一 ISA・同一 `WIDE`/`LANES` では常に
同一順序）も維持する設計とした。

### 不採用形

プロトタイプでの逆アセンブル確認により、以下の形は LLVM が劣化コードを生成する
ことを確認し不採用とした。

- **入れ子配列 `[[f32; LANES]; ACC]` をイテレータで回す形**: LLVM が xmm/ymm
  混在の劣化コードを生成する。
- **`wide` アキュムレータを `as_chunks` で `LANES` 毎に畳んで縮約する形**:
  配列がメモリへ落ち大幅に悪化する。

採用した形（本 ADR「設計」節）は「平坦な `[f32; WIDE]` 主ループ ＋
`[f32; LANES]` 端数ループ ＋ スカラー端数」の 2 段構造で、縮約は固定順
（`wide` 和 → `narrow` 和 → 端数和のインデックス昇順逐次和）。

## 参考プロトタイプ実測（計画立案時・リポ外・AVX2+FMA・本開発環境）

実装方針を確定するため、リポ外の使い捨てクレートで候補形を比較した（数値は
参考。採否判断は下記「本実装での実測結果」節の値による）。cache 常駐（作業集合
64 KiB）での ns/dot:

| 候補形 | dim128 | dim384 | dim768 | dim1536 |
| --- | --- | --- | --- | --- |
| 現行 `dot_lanes::<8>`（1 チェーン） | 5.4 ns | 18.2 ns | 44.2 ns | 105.8 ns |
| 平坦 16 幅（=2 チェーン） | 5.4 ns | 17.4 ns | 30.7 ns | 61.3 ns |
| 平坦 32 幅（=4 チェーン） | 6.9 ns | 16.4 ns | 28.7 ns | 56.1 ns |

arena 規模（25,000 行 × dim768）では改善が約 8% にとどまり、DRAM 帯域律速の
大規模データでは効果が薄いことも確認した。この時点の見立てでは「小次元で
非劣化・大次元で改善」という前提のもと既定候補を ACC=2 としていたが、下記
「本実装での実測結果」節の通り、本実装（`crates/engine` の実 `dot_lanes` 2 段
構造・`dot_kernel_bench` 実測）では cache 常駐 dim100/dim128 で無視できない
悪化が確認され、この前提は成立しなかった。

## 測定設計

`crates/engine/benches/dot_kernel_bench.rs`（`make bench-dot-kernel`）が
`dims = [100, 128, 384, 768, 1536]` × `WorkingSet::{CacheResident, ArenaScale}`
の 10 通りで `engine::isa::current().dot` の ns/dot を計測する。判定ロジック
（時間非依存）は `benches/harness/dot_kernel.rs` にあり
`tests/dot_kernel_accept.rs`（`make ci` 対象）で回帰検証する。

採否は 2 コミット間の worktree A/B で判断した: (1) ベンチ・ハーネスのみを先に
コミットし `cargo bench --bench dot_kernel_bench -p engine --no-run` のバイナリ
を `baseline` として保存、(2) `isa.rs` を `DOT_ACCUMULATORS = 2` へ変更して
再ビルドしたバイナリを `cand2` として保存、(3) `DOT_ACCUMULATORS = 4` へ変更
した `cand4` を追加保存、(4) `baseline`/`cand2`/`baseline`/`cand4` を交互に
各 5 回実行し中央値を比較。ノイズ帯は比率 ±5%（`classify_change` の
`noise_band = 0.05`）。

決定規則（採用条件・AND）:

1. cache 常駐・arena 規模の全 dim で `Regressed` が無いこと
   （`ratio <= 1.0 + noise_band`）。
2. dim768 または dim1536 の cache 常駐で `Improved`（`ratio <= 1.0 - noise_band`）。
3. 実アセンブリで独立アキュムレータ ACC 本が確認できること。

いずれの候補も満たさない場合は現状維持（`isa.rs` 無変更）とする。

## 本実装での実測結果

環境: Linux x86_64・検出 ISA `Avx2Fma`・論理コア 12（本開発環境。専有環境では
ない——実行中 loadavg が概ね 6〜11 で継続的な他プロセス負荷があった。値の絶対
水準はこの負荷の影響を受けるが、baseline/candidate の交互実行のため相対比較
（ratio）自体への影響は限定的と考えられる）。5 回交互実行の中央値・
`candidate/baseline` 比率。

### ACC=2（cache 常駐）

| dim | baseline ns/dot | ACC=2 ns/dot | ratio | 判定 |
| --- | --- | --- | --- | --- |
| 100 | 5.41 | 6.18 | 1.142 | **Regressed** |
| 128 | 5.46 | 5.84 | 1.070 | **Regressed** |
| 384 | 18.12 | 16.16 | 0.892 | Improved |
| 768 | 44.41 | 31.64 | 0.712 | Improved |
| 1536 | 106.33 | 63.51 | 0.597 | Improved |

### ACC=2（arena 規模・25,000 行）

| dim | baseline ns/dot | ACC=2 ns/dot | ratio | 判定 |
| --- | --- | --- | --- | --- |
| 100 | 6.15 | 6.73 | 1.094 | **Regressed** |
| 128 | 8.27 | 8.52 | 1.030 | Neutral |
| 384 | 48.39 | 50.42 | 1.042 | Neutral |
| 768 | 109.39 | 126.64 | 1.158 | **Regressed**（高負荷環境でのばらつきが大きい区分。下記「限界」節） |
| 1536 | 230.84 | 240.96 | 1.044 | Neutral |

### ACC=4（cache 常駐）

| dim | baseline ns/dot | ACC=4 ns/dot | ratio | 判定 |
| --- | --- | --- | --- | --- |
| 100 | 5.47 | 8.73 | 1.596 | **Regressed（大幅）** |
| 128 | 5.52 | 8.28 | 1.500 | **Regressed（大幅）** |
| 384 | 18.36 | 16.79 | 0.914 | Improved |
| 768 | 44.97 | 32.69 | 0.727 | Improved |
| 1536 | 105.53 | 63.38 | 0.601 | Improved |

### ACC=4（arena 規模）

| dim | baseline ns/dot | ACC=4 ns/dot | ratio | 判定 |
| --- | --- | --- | --- | --- |
| 100 | 6.07 | 9.96 | 1.641 | **Regressed（大幅）** |
| 128 | 7.96 | 10.13 | 1.273 | **Regressed** |
| 384 | 48.70 | 46.58 | 0.956 | Neutral |
| 768 | 126.10 | 107.79 | 0.855 | Improved |
| 1536 | 255.86 | 207.92 | 0.813 | Improved |

## 実アセンブリ確認

決定規則の条件3（「実アセンブリで独立アキュムレータ ACC 本が確認できること」）
の根拠を再現手順として記録する。「参考プロトタイプ実測」節（リポ外の使い捨て
クレート）とは別に、本実装（`crates/engine` の実 `dot_lanes` 相当構造）の
`dot_avx2_fma` シンボルを実際に逆アセンブルして確認した。

### 手順

1. `isa.rs` の `dot_avx2_fma` を一時的に「設計」節の 2 段構造（`WIDE = LANES *
   DOT_ACCUMULATORS` の平坦アキュムレータ配列）へ書き換える（`DOT_ACCUMULATORS`
   は検証対象の値。baseline 確認時は無変更のまま）。
2. `cargo bench --bench dot_kernel_bench -p engine --no-run` でビルドし、
   `target/release/deps/dot_kernel_bench-<hash>` を確認用に退避する。
3. `nm <バイナリ> | grep dot_avx2_fma` でマングル済みシンボル名
   （`_ZN6engine3isa12dot_avx2_fma...`）を取得する。
4. `objdump -d --disassemble='<シンボル名>' -M intel <バイナリ>` で当該関数のみ
   逆アセンブルする。
5. 確認後 `isa.rs` を `git checkout -- crates/engine/src/isa.rs` 等で元に戻す
   （production コードは無変更のまま維持）。

### 確認結果

- **baseline（現行 `dot_lanes::<8>`。1 チェーン）**: 主ループは 4 段 unroll
  だが、`vfmadd132ps`／`vfmadd231ps` の宛先レジスタが `ymm1` 1 本に畳み込まれる
  逐次依存チェーンであることを確認した（1 反復内の 4 命令が `ymm1 → ymm1 →
  ymm1 → ymm0(=ymm1 コピー) → ymm0` と直列に連鎖する）。Issue #362
  （`docs/design/knn-stage-profile.md`「`dot_lanes` の実アセンブリ確認」節）の
  既存確認と整合する。
- **ACC=2**: 主ループの 1 反復に `vfmadd231ps ymm1,...` と
  `vfmadd231ps ymm2,...` が 2 組（計 4 命令）現れ、`ymm1` 系列と `ymm2` 系列が
  互いに参照し合わない独立したレジスタ依存チェーンであることを確認した
  （命令列がインターリーブされ、`ymm1`/`ymm2` それぞれの自己再帰のみで閉じる）。
- **ACC=4**: 同様に `ymm1`〜`ymm4` の 4 系列が独立して現れ、各系列内でのみ
  自己再帰する構造を確認した。

以上により、ACC=2・ACC=4 のいずれも「LLVM が `DOT_ACCUMULATORS` 本の独立 FMA
チェーンへ展開する」という「設計」節の想定どおりの機械語が生成されており、
決定規則の条件3は両候補とも満たすと判断できる（実測環境: Linux x86_64・検出
ISA `Avx2Fma`。「本実装での実測結果」節と同一開発環境）。

## 判断

ACC=2・ACC=4 のいずれも決定規則の条件 1（全 dim で `Regressed` なし）を満たさ
ない。cache 常駐 dim100/dim128 の悪化は 5 回の交互実行すべてで一貫して観測され
（ACC=2 で約 7〜14%、ACC=4 で約 50〜64%）、ノイズ帯（±5%）を明確に超える。
条件 2・3（dim768/1536 での改善・実アセンブリでの独立アキュムレータ確認）は
両候補とも満たすが、条件 1 が AND 条件であるため採用条件を満たさない。

`docs/spec/04-behavior/search.md` 等の対象ビヘイビアは KNN 経路全体の p95・
Recall を対象とし、内積カーネル単体の小次元性能を個別に規定していない。しかし
本リポの実装コーパス（`docs/design/knn-stage-profile.md` の測定で使われた
dim128 規模を含む）が小次元領域を含むため、小次元での 7〜14% の劣化を production
へ持ち込む判断は決定規則（§「測定設計」）に従い避け、**`crates/engine/src/
isa.rs` は変更しない**（`git checkout origin/main -- crates/engine/src/isa.rs`
で production コードを元に戻し、本コミットにはマイクロベンチ・ADR のみを含める）。

Issue #365 は本 ADR の実測表・判断根拠をもって**現状維持で close 可**とする。

## 限界

- 本開発環境は専有環境ではなく、実行中の loadavg が高め（6〜11、論理コア 12）
  だった。arena 規模 dim768 の ACC=2 実測（ratio 1.158）は cache 常駐の傾向
  （小次元悪化・大次元改善）や ACC=4 の arena 規模実測（dim768 で改善）とも
  整合しないばらつきを示しており、測定ノイズの寄与が大きい可能性がある。
  ただし cache 常駐 dim100/dim128 の悪化はノイズでは説明しづらい一貫した
  傾向であり、本 ADR の判断（現状維持）はこの一貫した悪化のみに基づく
  （arena 規模 dim768 の単一の逆行結果には依存しない）。
- AVX-512・NEON 実機でのマイクロベンチ実測は行っていない（本環境は AVX2+FMA
  のみ）。構造は ISA 間で共通のため小次元悪化の傾向自体は再現しうると推測
  されるが未検証。
- 交互実行 5 回・専有環境でない条件のため、専有環境での再実測（`docs/design/
  c1-p95-dedicated-env-reverification.md` と同種の運用者作業）があれば
  条件 1 の判定（特に arena 規模 dim768）がより確からしくなる。

## 再現手順

```sh
# ベンチ・ハーネスをビルド（isa.rs は無変更のまま）
cargo bench --bench dot_kernel_bench -p engine --no-run
# ビルド成果物を退避（target/release/deps/ には同一接頭辞の実行可能ファイルに
# 加え rustc が生成する .d dep-info ファイル（例: dot_kernel_bench-<hash>.d）が
# 同居し、glob だけでは実行可能ファイルとの区別がつかない。mtime 降順（ls -t）
# ソートに加えて `.d` 拡張子を除外し、find の出力順序がファイルシステム依存で
# タイムスタンプ順を保証しないのを避けつつ、誤って .d ファイルを baseline/cand
# として選ばないようにする）
cp "$(ls -t target/release/deps/dot_kernel_bench-* 2>/dev/null | grep -v '\.d$' | head -1)" /path/to/baseline

# isa.rs の DOT_ACCUMULATORS を変更（例: 2）して再ビルド
cargo bench --bench dot_kernel_bench -p engine --no-run
cp "$(ls -t target/release/deps/dot_kernel_bench-* 2>/dev/null | grep -v '\.d$' | head -1)" /path/to/cand2

# 交互実行して比較（例: 5 回）
for i in 1 2 3 4 5; do /path/to/baseline; /path/to/cand2; done
```

`make bench-dot-kernel` は単一バイナリの実測出力（`current` ラベルのみ）を
返す。前後比較には上記のように 2 バイナリを個別にビルド・退避する必要がある
（単一ビルド内 A/B にしなかった理由は本 ADR「不採用形」節の前段落
——`unsafe` 増加を伴う旧カーネル複製を避けたため）。

## スコープ外

- AVX-512・NEON 実機でのマイクロベンチ実測（本環境は AVX2+FMA のみ。正当性は
  `make check-cross` のクロスコンパイル確認・CI の x86_64 runner で担保）。
- intrinsics 直書き化（spec 側対象外領域・オーナー判断）。
- `kernel.rs`/`parallel_search.rs` 上位経路の変更、arena 規模での DRAM 帯域
  律速そのものへの対処（連続格納レイアウト検討 #364・キャッシュ検討 #363 の
  領域）。
- `.github/workflows/bench.yml` への配線（情報提供専用ベンチのため行わない）。

## 行間再利用（Issue #512）: 行ブロックカーネルの前後比較と採否

Issue #510・#511（`SimdKernel::dot_block4`。`docs/design/dot-kernel-row-block.md`）
は既に既定エンジン（`ParallelBruteForce` → `parallel_search::search_range`）へ
結線済みで、1 行版 `dot` とビット同一。本節は撤回可否の判断材料として、単一
ビルド内 block4 A/B（層 A）と別ビルド前後比較（層 B）を実測した記録である。
判定は `docs/design/benchmark-judgement-policy.md`（Issue #462）に従う。

### 環境

- CPU: `QEMU Virtual CPU version 2.5+`（KVM）・12 vCPU。フラグ `avx2` / `fma` /
  `f16c`（`avx512*` なし）
- 負荷: 共有・非専有。層 A 実測時 loadavg 約 3.9〜5.5、層 B 実測時 約 4〜9
  （`BENCH_DEDICATED_ENV` 未設定）
- rustc: `1.96.0`（`isa.rs::DetectedIsa` = `Avx2Fma`）

### 層 A: `dot_kernel_bench` 単一ビルド内 block4 A/B（Issue #512 追加分）

`BENCH_DOT_KERNEL_BLOCK_AB=1 make bench-dot-kernel` で dim=128／768 ×
`WorkingSet::{CacheResident, ArenaScale}` を計測。A（1 行版 `dot` を 4 行分
連続呼び出し。#510 以前の `search_range` 相当）・B（`dot_block4`）を
`harness::ab::run_ab` で交互実行（warmup 20・計測 50）し、計測前に全ブロック
で `dot_block4` が 1 行版とビット同一であることを検証済み（fail-closed）。

> **再計測の経緯**: 初出時の本節は、参照区間（`block4_ab_ref`）が「同一
> プロセス内の反復間ノイズ帯」（`(max-min)/min`。分母はプロセス内の反復値）
> のみを出力しており、`benchmark-judgement-policy.md` §4 が要求する
> 「変更を含まない参照区間の **run-to-run（プロセス起動間）** 幅」とは
> 異なる量だった（codex-review 指摘。プロセス内反復間ノイズは cache/CPU
> 周波数遷移由来で、プロセス起動間ノイズ〔loadavg 変動・スケジューラ由来〕
> とは性質が異なり、後者の代用にはできない）。また旧版の block4 A/B
> `min-of-N ratio` 列は各 run 比率の最小〜最大レンジの外側の値を含んでおり
> （正の時間値では `min(B)/min(A)` は各 run 比率の範囲内に収まるはずという
> 数学的制約と矛盾）、実際には保持していなかった生の A/B 時間値からの
> 再計算では復元できなかった（codex-review 指摘。旧版の値は誤りとして
> 破棄する）。本節はベンチ側の参照区間計測を A/B 側と対称な `repeat` 回
> 走査へ修正し（cache_resident でも `CACHE_RESIDENT_REPEAT`＝200 回反復を
> 反映。Cursor Bugbot 指摘）、参照区間の 1 プロセスぶんの代表値
> （`ref_median_ms`）を出力するよう変更（`crates/engine/benches/dot_kernel_bench.rs`・
> `benches/harness/dot_block.rs::render_block_ab_reference_line`）したうえで
> 全数値を実機で再計測した。以下は再計測後の値である。

per-run 生データ（N=5・プロセス単位起動。参照区間の対称化〔上記「再計測の経緯」〕
を含む本 doc・ベンチ更新コミットで計測。CPU・負荷条件は本節「環境」を参照）:

| working_set | dim | run1 | run2 | run3 | run4 | run5 | min-of-N | median |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| cache_resident | 128 | A=0.134 B=0.124 r=0.922 | A=0.134 B=0.123 r=0.921 | A=0.135 B=0.125 r=0.928 | A=0.136 B=0.126 r=0.923 | A=0.133 B=0.124 r=0.929 | 0.9248 | 0.9254 |
| cache_resident | 768 | A=0.179 B=0.082 r=0.458 | A=0.180 B=0.082 r=0.458 | A=0.180 B=0.082 r=0.457 | A=0.180 B=0.082 r=0.457 | A=0.180 B=0.082 r=0.457 | 0.4581 | 0.4556 |
| arena_scale | 128 | A=0.226 B=0.235 r=1.040 | A=0.231 B=0.241 r=1.045 | A=0.230 B=0.239 r=1.042 | A=0.230 B=0.240 r=1.045 | A=0.227 B=0.236 r=1.040 | 1.0398 | 1.0391 |
| arena_scale | 768 | A=3.086 B=2.632 r=0.853 | A=3.084 B=2.632 r=0.853 | A=3.086 B=2.600 r=0.843 | A=3.083 B=2.561 r=0.831 | A=3.074 B=2.610 r=0.849 | 0.8331 | 0.8462 |

（A = `single_row_median_ms`・B = `block4_median_ms`・r = `ratio`（B/A）。
min-of-N は `min(B の 5 run) / min(A の 5 run)`（`benchmark-judgement-policy.md`
§3 の統計量定義どおり。正の時間値であれば各 run の r の範囲内に収まる）。
median は `median(B の 5 run) / median(A の 5 run)`（`benchmark-judgement-policy.md`
§2「median-of-N: N ペアの中央値を採る統計量」・§3「統計量は min-of-N と median の
両方を必ず併記する」の定義どおり、min-of-N と対称に各系列〔A・B〕の中央値から
算出する。`harness/ab.rs::median_ratio`〔`summary_a.median / summary_b.median`〕
と同一方式であり、本節「層 B」の `S5_search_parallel` の median 集計方式とも
一致させた。旧版は各 run の比率 r の中央値〔`median(r)`。系列 A・B 双方の中央値が
同一 run で揃うとは限らないため一般に `median(B)/median(A)` とは一致しない〕を
誤って「median」欄に記載していた〔codex-review 指摘〕。両者の差は本節の実測値
では小さく〔例: cache128 は 0.9230→0.9254〕、下記「判定」節の分類結果〔`Improved`
／`ノイズ帯内`〕はいずれも変わらない。参考として各 run の比率 r の中央値
（旧版の値）も別の統計量として残す: cache128 0.9230・cache768 0.4570・
arena128 1.0420・arena768 0.8490。生データは `crates/engine/benches/harness/
dot_block.rs`・`benches/dot_kernel_bench.rs` の `measure_block_ab_stage` が
同一プロセス内で `run_ab`（interleaved）により出力する行から再現可能。実行
ログは `docs/design`（本 doc）以外には保存していないため、再現には「再現手順
（層 A）」節の手順を再実行すること）

> **算出精度の注記（codex-review 指摘）**: 上表の A・B 列は
> `render_block_ab_line`〔`crates/engine/benches/harness/dot_block.rs`〕が
> `{:.3}` で出力する小数第 3 位丸めの表示値であり、min-of-N・median 列は
> いずれもこの丸め後の A・B 表示値から算出した近似値である（丸め前の
> 生 `Duration` 値は本 doc・ベンチいずれにも保存しておらず、実測を伴わない
> 再集計はできない）。一方 r 列は同じ関数が丸め前の `ratio: f64` を
> `{:.3}` で丸めて出力した値であり、A・B を経由せず直接算出している。
> このため A・B 経由で丸め誤差が畳み込まれる min-of-N・median 列は、r 列
> （丸め前の高精度値に基づく）の 5 run 最小〜最大レンジからわずかに外れ
> うる。実際 cache_resident/768 は median 比 0.4556 が r 列レンジ
> 〔0.457〜0.458〕の外、arena_scale/128 も median 比 1.0391 が r 列レンジ
> 〔1.040〜1.045〕の外だが、他の 2 行（cache_resident/128・arena_scale/768）
> は丸め誤差がレンジ内に収まっている。いずれも丸めによる小さな乖離
> （0.0001〜0.001 台）であり、下記「判定」節の分類（`Improved`／
> `ノイズ帯内`）には影響しない。

参照区間（`block4_ab_ref`。A/B 側と同一 `repeat` 回で 1 行版 `dot` を反復走査。
block4 A/B の対象外）の 1 プロセスぶんの代表値（`ref_median_ms`）と、そこから
算出した run-to-run 相対ノイズ帯 `reference_band = (max-min)/min`
（`benchmark-judgement-policy.md` §4 が要求する量。分母は 5 run 中の最小値）:

| working_set | dim | run1 | run2 | run3 | run4 | run5 | reference_band |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cache_resident | 128 | 0.140 | 0.140 | 0.142 | 0.141 | 0.140 | 0.0143 |
| cache_resident | 768 | 0.179 | 0.178 | 0.178 | 0.178 | 0.178 | 0.0056 |
| arena_scale | 128 | 0.230 | 0.234 | 0.234 | 0.234 | 0.230 | 0.0174 |
| arena_scale | 768 | 3.099 | 3.101 | 3.174 | 3.070 | 3.094 | 0.0339 |

（単位は ms。参考: 各 run のプロセス内反復間ノイズ帯〔`block4_ab_ref` 行の
`band` フィールド。§4 が要求する run-to-run 幅とは別の量であり判定には
使わない〕は cache128 で 3〜7% 程度、arena128／arena768 は run3・run4 で
一時的に高く出る回があったが〔プロセス内の一時的な負荷変動と考えられる〕、
プロセス起動間で見た `ref_median_ms` 自体は上表のとおり run 間で安定して
いる）

**判定（両ノイズ帯を要件とする。`benchmark-judgement-policy.md` §4）**:

- dim768（cache_resident・arena_scale とも）: `Improved`（固定 ±5% 帯・実測
  run-to-run 帯の両方を超過）。cache_resident は min-of-N 比 0.4581・median 比
  0.4556（reference_band 0.56% を大きく上回る）。arena_scale は min-of-N 比
  0.8331・median 比 0.8462（reference_band 3.39% を上回る）。
- dim128 cache_resident: `Improved`（min-of-N 比 0.9248・median 比 0.9254。
  reference_band 1.43% に対し `|ratio-1|` は約 7.5〜7.7% で両ノイズ帯を超過）。
- dim128 arena_scale: `ノイズ帯内`（固定 ±5% 帯の範囲内。min-of-N 比
  1.0398・median 比 1.0391 はいずれも `dot_block4` 側がわずかに遅い方向だが、
  固定帯を超えていないため実測 reference_band〔1.74%〕を超えているか否かに
  関わらず「両ノイズ帯を超えること」という §4 の要件を満たさず、有効な変化
  としては扱わない。旧版はここを改善方向〔約 -1〜-3%〕と誤記していたが、
  参照区間の算出誤り〔上記「再計測の経緯」〕を修正した再計測では符号が
  逆——`dot_block4` がわずかに遅い方向——であることが判明した。Issue #365 が
  cache 常駐 dim100/128 で複数アキュムレータ化の効果が乏しいと判断した傾向
  とは整合するが、arena_scale/128 は「改善が乏しい」ではなく「ノイズ帯内で
  判定不能（むしろ悪化方向）」と訂正する）

### 層 B: production 経路（`bench-chip` knn_profile）前後比較

- before: `66e641c`（#510 マージ直前）・after: `631a050`（#510 マージ後）を
  それぞれ `git worktree add` で checkout し `cargo build --release --bench
  chip_bench` でビルド（`isa.rs::SimdKernel::dot` 呼び出し形のみが差分）
- `BENCH_CHIP_ROUNDS=1 BENCH_CHIP_WORKLOADS=knn_profile` で before/after を
  5 ペア交互実行（`S5_search_parallel` が `ParallelBruteForce::search` →
  `parallel_search::search_range` → `dot_block4`〔after〕/ 4×`dot`〔before〕を
  通る対象区間、`S1_redb_scan` が dot を通らない参照区間）

`S5_search_parallel/median_ms`（1 ペア = 1 プロセス起動あたり 1 サンプル）:

| pair | before | after |
| --- | --- | --- |
| 1 | 3.003 | 0.229 |
| 2 | 0.219 | 3.202 |
| 3 | 0.250 | 3.079 |
| 4 | 3.331 | 0.381 |
| 5 | 0.241 | 0.345 |

min-of-N: before=0.219ms・after=0.229ms（ratio 1.046）／median: before=0.250ms・
after=0.381ms（ratio 1.524）。

参照区間 `S1_redb_scan/median_ms`（同一プロセスが同時に計測。dot を通らない）:
before は 0.967〜0.979ms（幅 1.2%）、after は 0.968〜1.066ms（幅 10.1%）と
比較的安定している一方、`S5_search_parallel` は同一 before/after 内でも
0.219ms〜3.331ms（15 倍超）の 2 峰性のばらつきを示した。

**観測事実**と**未検証の原因仮説**を分けて記録する（codex-review 指摘。旧版は
「`ParallelBruteForce` のスレッドプール起動コスト」を原因として断定的に記述
していたが、`crates/engine/src/parallel_search.rs` の実装を確認したところ
`ParallelBruteForce` はスレッドプールを保持せず、検索のたびに
`std::thread::scope` 配下でワーカーを spawn する構成である。また `S5` は
`harness::protocol::run` の warmup フェーズ〔20 回〕を経てから計測フェーズを
計測するため、初回スレッド起動固有のコストが計測サンプルに現れる根拠も
実装からは確認できない）。

- 観測事実: `S5_search_parallel` は before/after いずれも 5 回のプロセス
  起動間（1 ペア = 1 プロセス起動あたり 1 サンプル）で 2 峰性（約 0.22〜
  0.38ms 群と約 3.0〜3.3ms 群）のばらつきを示し、同一プロセスが同時に計測
  する dot を通らない `S1_redb_scan`（安定・幅 1.2〜10.1%）には現れない。
- 未検証の原因仮説: knn_profile の小規模ワークロード（`S0` 由来の少数行）
  特有の何らかのスケジューリング・キャッシュ挙動が `search_range` 呼び出し
  経路（`dot_block4` 有無を問わず）で 2 峰化を引き起こしている可能性がある
  が、上記の実装確認により「スレッドプールの初回起動コスト」という原因は
  裏付けられない。原因の特定は本 Issue の範囲外とし、以下の判定は原因を
  問わず観測されたノイズ規模のみに基づく。

**判定**: min-of-N ratio（1.046）は固定 ±5% 帯の境界付近で `Neutral` 寄りだが、
median ratio（1.524）は大きく異なり、N=5・単発ラウンドでは判定不能
（`benchmark-judgement-policy.md` §5「共有 QEMU 本環境」は絶対値ベースの
確定判定を認めない区分であり、本節の層 B はその制約に加えて
原因未特定の 2 峰性ノイズ（プロセス起動間で観測。原因の特定は本 Issue の
範囲外）が層 A の µs〜ms 級改善を容易に覆い隠す規模であることを確認した、
という位置づけに留める）。
`simd_bench`（CORE-3/4・Recall 計算込み）・`feature_128`/`feature_768` の
追加実行は、本ノイズ規模を踏まえると層 B の結論を変える見込みが低く、
かつ Recall 計算を含む `simd_bench` は 1 ペアあたり数分規模を要するため、
本 Issue の実施時間内では見送り、AVX-512／NEON 実機での層 B 再実測
（Issue #530）へ申し送る。

### 判定（決定木）

`benchmark-judgement-policy.md` §5 により、共有 QEMU 本環境は perf 動機の
production 変更の「採用（Accepted）」根拠にはできない。層 A・層 B いずれも
一貫した `Regressed`（両ノイズ帯を超える悪化）は観測されなかったため、
「Rejected（撤回推奨）」の条件も満たさない。よって:

**「参考値・現状維持（条件付き）」**——`crates/engine/src/parallel_search.rs`・
`isa.rs`（#510・#511 で導入済みの `dot_block4` 結線）は据え置く。層 A の
dim768 改善（cache_resident min-of-N 比 0.4581・arena_scale min-of-N 比
0.8331。いずれも固定 ±5% 帯・実測 run-to-run 帯の両方を超過）・dim128
cache_resident 改善（min-of-N 比 0.9248。両ノイズ帯を超過）は本環境の証拠力の
範囲で「参考値」として記録する。dim128 arena_scale は固定 ±5% 帯の内側
（min-of-N 比 1.0398）のため「ノイズ帯内」（有効な変化と扱わない）、
層 B production 経路は判定不能として、最終採否を AVX-512／NEON 実機
（Issue #530）へ申し送る。

### 再現手順（層 A）

```sh
cargo bench --bench dot_kernel_bench -p engine --no-run
BIN="$(ls -t target/release/deps/dot_kernel_bench-* 2>/dev/null | grep -v '\.d$' | head -1)"
for i in 1 2 3 4 5; do BENCH_DOT_KERNEL_BLOCK_AB=1 "$BIN"; done
```

### 再現手順（層 B）

```sh
git worktree add /path/to/before 66e641c
git worktree add /path/to/after 631a050
CARGO_TARGET_DIR=/path/to/target-before cargo build --release --bench chip_bench \
  --manifest-path /path/to/before/crates/engine/Cargo.toml
CARGO_TARGET_DIR=/path/to/target-after cargo build --release --bench chip_bench \
  --manifest-path /path/to/after/crates/engine/Cargo.toml
# 交互に BENCH_CHIP_ROUNDS=1 BENCH_CHIP_WORKLOADS=knn_profile BENCH_CHIP_OUT_DIR=... で 5 ペア実行
```

### 限界・スコープ外

- AVX-512F・NEON 実機での層 A・層 B 実測（本環境は AVX2+FMA のみ実行可能。
  正当性は `make simd-codegen-check`・`make check-cross` の生成コード検査で
  担保。実機実測は Issue #530）
- `simd_bench`（CORE-3/4・Recall 込み）・`bench-chip` の `feature_128`／
  `feature_768` ワークロードでの層 B 追加実測（層 B は本節の 5 ペアで
  ノイズ規模が層 A の効果を上回ることを確認済みのため、追加ワークロードは
  結論を変える見込みが低いと判断し本 Issue の範囲では見送り）
- `hnsw.rs`・`batch_search.rs`・`rls.rs` の 1 行 `dot` 呼び出しの行ブロック化
  （`docs/design/dot-kernel-row-block.md` §6 と同一のスコープ外）
- production 変更（`parallel_search.rs`・`isa.rs`）は無変更（本 Issue はテスト・
  ベンチ・docs 専任。撤回条件を満たしていないため撤回作業も対象外）

## Issue #518 追記: dim 閾値による ACC=4 経路のディスパッチ

Issue #365 が不採用とした理由は「全 dim 一律の ACC=4 化」が小次元（dim100/128）
を悪化させる点にあり、dim768/1536 側は改善方向だった。Issue #517（親）・
Issue #518（実装）はこの知見を踏まえ、**ベクトル長を実行時に見て閾値
（`isa::DOT_MULTI_ACC_MIN_DIM` = 768）以上のときだけ** `dot_lanes_multi_acc`
（ACC=4）へ分岐し、閾値未満は既存 `dot_lanes` を変更せずそのまま通す構成で
条件付き採用した。

- 分岐は `dot_neon`／`dot_avx2_fma`／`dot_avx512` の各 `#[target_feature]` fn
  内で `a.len().min(b.len()) >= DOT_MULTI_ACC_MIN_DIM` により行い、新規
  `unsafe` は追加していない（`tests/isa.rs::
  unsafe_is_confined_to_isa_module_with_safety_comments` の個数 3 は不変）
- dim<768 の経路は `dot_lanes` 本体を一切変更していないため、dim=128 を含む
  既存テストはすべて無変更で green（受け入れ条件）
- dim>=768 は演算順が変わるため丸め誤差が変化しうるが、本リポの Recall
  フィクスチャ（`hybrid_recall.rs`・`rerank_recall.rs`・
  `sparse_cache_recall.rs` の大規模段は dim 800、`query_planning_recall.rs`
  は dim 1000）はいずれも 0/1 の整数値ベクトル（one-hot 系）を使っており、
  整数値の内積は加算順序に依らず厳密に一致するため、層 A 固定値アサーション
  は構造的に不変である。実測でも `cargo test --workspace` は全 green
  （層 A 固定値含む）を確認済み
- AVX2+FMA 以外（AVX-512・NEON）での実測は本 Issue の範囲外（Issue #365 と
  同じ限界を引き継ぐ）。閾値そのものの確定・チップ別実測・前後比較は
  Issue #519 の担当
- `PADDED_TAIL`（tail 処理方式切り替え。Issue #528）は `dot_lanes_multi_acc`
  にも貫通させ、端数（`a_rem`／`b_rem`）と `narrow` レーンの縮約は `dot_lanes`
  と同じ `reduce_lanes` を経由する（手書きの逐次和には戻さない）。これにより
  `SimdKernel::dot_with_scalar_tail`／`dot_with_padded_tail` の契約は
  dim>=768 でも dim<768 と同一の形で維持される
- `SimdKernel::dot_block4`（行ブロックカーネル。Issue #510・#511・
  `docs/design/dot-kernel-row-block.md`）の intrinsics 本体
  （`x86_block4`／`neon_block4`）自体は `dot_lanes_multi_acc` へ対応させて
  いない（行ブロック側の ACC=4 化は本 Issue のスコープに含めない・要否は
  Issue #519 へ申し送り）。dim>=768 でも `dot_block4(rows, query)[i] ==
  dot(rows[i], query)` というビット同一契約（`kernel.rs::dot` を共有する
  `CpuScalarProvider`・`ParallelSearchProvider` 間の Top-k 整合の根拠）を
  崩さないため、`isa.rs::SimdKernel::dot_block4_impl` は dim>=768 を
  「4 行と `query` の長さが不揃いなときの縮退経路」（1 行版 `dot_impl` を
  4 回呼ぶ既存経路）へ意図的に合流させた。dim>=768 は非一様長ではないが、
  同じ「intrinsics ブロックカーネルへ入らず 1 行版を 4 回呼ぶ」経路を再利用
  することで、1 行版が `dot_lanes_multi_acc` へ分岐した効果を `dot_block4`
  経由の呼び出しにも自動的に波及させる（新規 intrinsics カーネルの追加なし）。
  `tests/isa.rs::dot_block4_matches_single_row_dot_bit_exact_across_dims` は
  dim 768/1000/1536 を含む元の全 dim 帯でビット同一のまま検証する
