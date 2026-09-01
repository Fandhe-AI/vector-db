# `isa.rs` dot カーネルの複数アキュムレータ化検討

- ステータス: **現状維持で close 可**（本コミットで計測ベンチ・ADR を追加。
  `crates/engine/src/isa.rs` の production コードは無変更）
- 対応: Issue #365（`perf(engine): isa.rs dot カーネルの複数アキュムレータ化`）
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
