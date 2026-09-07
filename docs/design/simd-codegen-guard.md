# SIMD カーネル生成コード検査ガード（要素ごと挿入命令の不在チェック）

- ステータス: **Accepted**（CI ガード追加。production コード〔`crates/engine/src/`〕は無変更）
- 対応: Issue #467（親 #456／ルート #455）
- 関連ポインタ: TASK-156（CORE-14。`isa.rs` の実行時 ISA 検出）・
  `docs/design/chip-kernel-guidelines.md`§0.3・`docs/design/hotpath-implementation-survey.md`

## 1. 背景・目的

`crates/engine/src/isa.rs` は現状 intrinsics を使わず、`#[target_feature]` 付き
safe fn 内の `dot_lanes::<LANES>`（`as_chunks` による固定長チャンク化 ＋
`f32::mul_add`）を LLVM の自動ベクトル化に委ねている。`chip-kernel-guidelines.md`
§0.3 で検討した後続 Phase（f16 常駐等）の intrinsics 導入方式は、「`as_chunks` で
得た固定長配列の要素から `_mm256_set_ps`／`_mm_set_epi16` 等を構築し、新規
`unsafe` を持たない」形を想定している。

この `set` 構築が単一のロード命令（`vmovups`／`vcvtph2ps` 等のメモリオペランド
付き命令）へ畳み込まれるのは **LLVM の最適化挙動であって言語仕様の保証ではない**。
連続要素からの `_mm256_set_ps(a[7], a[6], ..., a[0])` は現行 rustc/LLVM では
単純なロードへ最適化されるが、この前提が rustc/LLVM の更新やコードの書き方の
変化で崩れた場合、要素ごとの挿入命令（`vinsertps`／`vpinsr*`／`vunpck*` 等）が
アセンブリに残ってしまい、SIMD 化の意図（複数要素を 1 命令でロードする）が
実質的に達成されないまま気付かれない可能性がある。

本ガードは、実際に生成されたアセンブリを機械検査することで、この前提の崩れを
CI 段階（PR 作成前）で検出できるようにする。

## 2. 検査方式

`scripts/check_simd_codegen.sh`（`check_sort_determinism.sh`・`check_core_api.sh`
と同型の、`cargo test` を経由しない軽量シェルスクリプト）が以下を行う。

1. `cargo rustc -p engine --release --lib -- --emit asm` で release ビルドの
   アセンブリ（`target/.../deps/engine-<hash>.s`）を生成する（追加の `-C` フラグは
   付けない。実際の release ビルドとの忠実性を優先）
2. 生成された `.s` から、マングル名に `isa` モジュールのセグメント（`3isa`）を
   含む関数をラベル〜`.cfi_endproc` 単位で抽出する
3. target 別の必須シンボル（下記）が対象集合に含まれることを確認する
   （vacuous pass 防止。§3 参照）
4. 各関数の命令列に禁止命令（§4）が 1 件でも含まれていれば非ゼロで終了する

### なぜ「関数名」ではなく「モジュール単位」で対象を絞るか

x86_64 では `dot_avx2_fma`／`dot_avx512` は独立シンボルとして残る（`#[target_
feature]` fn は非 feature 呼び出し元へインライン化されないため）。一方
**aarch64 では `dot_neon` のシンボルが存在しない**——NEON は aarch64 の baseline
feature のため `SimdKernel::dot` 本体へインライン化される。関数名で対象を固定
すると aarch64 側の検査が何も対象を見つけられず、常に pass する（vacuous pass）
状態になってしまう。そのため対象は「`isa` モジュール配下の全関数」とし、
代わりに target 別の必須シンボルが実際に抽出されていることを別途検査する。

必須シンボル（マングル名の長さ接頭辞付きセグメント）:

| target | 必須シンボル |
| ------ | ------------ |
| x86_64 | `dot_avx2_fma` かつ `dot_avx512` |
| aarch64 | `SimdKernel::dot`（`dot_neon` はインライン化されるため対象外） |

## 3. 禁止命令集合と根拠

| target | 禁止パターン |
| ------ | ------------ |
| x86_64 | `vinsertps`／`insertps`、`vpinsr{b,w,d,q}`／`pinsr{b,w,d,q}`、`vunpcklps`／`vunpckhps` |
| aarch64 | レーン指定 `ld1 {...}[n]`、`ins v...`、`mov v_.{b,h,s,d}[n]` |

`vinsertf128`／`vinserti128`（128→256 bit の結合命令）は**禁止集合に含めない**。
これは 128 bit レジスタ 2 個を 256 bit へ結合する正当な操作であり、要素ごとの
ギャザー構築とは別物。実測で `_mm_insert_ps` 明示 intrinsic や変換後の
`_mm256_set_ps`（型変換を伴う構築）はこの命令へ最適化されることを確認しており、
これらを誤って禁止すると自然な実装まで拒否してしまう（self-test fixture として
不適と判断した経緯は §6 参照）。

`vunpcklps`／`vunpckhps` は現行基線（§5）で 0 件だが、水平和（レーン総和）の
実装次第では将来出現しうる。出現した場合はパターンの緩和ではなく、該当関数の
実装方法（水平和の書き方）を見直す運用とする。

## 4. パターンを弱める上書き経路の不存在

`isa.rs` の CORE-12 方針（環境変数・設定ファイルによる ISA 検出の上書き禁止）と
同じ思想で、本スクリプトは検査パターンを弱める・無効化する環境変数上書き経路を
一切持たない。`check_sort_determinism.sh` が備える行コメント許可マーカー方式
（`sort-determinism: allow <理由>`）のような個別除外機構も設けない。パターン変更
は本スクリプト自体の修正（＝コードレビュー経由）に限定する。

環境変数 `SIMD_CODEGEN_TARGET_DIR` は `cargo rustc --emit asm` の出力先ディレクトリ
を分離するためだけに使い、検査ロジックには一切影響しない。

## 5. 基線記録（現行 `isa.rs`。rustc 1.96.0・LLVM 22.1.2）

現行コードは本ガードを pass する（禁止命令 0 件）。実測した主要命令カウント:

- x86_64
  - `dot_avx2_fma`: `vfmadd231ps` ×4・`vfmadd132ps` ×1・`vmovups` ×5、
    水平和は `vshufps` ×2／`vshufpd` ×2・`vmovshdup` ×2・`vextractf128` ×1
  - `dot_avx512`: `dot_avx2_fma` に加え `vextractf32x4` ×2
  - 禁止命令（`vinsertps`／`vpinsr*`／`vunpck*`）はいずれも 0 件
- aarch64
  - `SimdKernel::dot`: `ldr q`／`ldp q`・`fmla v.4s`・`dup v.4s`
  - レーン挿入命令（`ld1 {...}[n]`／`ins v`／`mov v_.[bhsd][`）は 0 件

## 6. self-test fixture の設計

`--self-test` は検査ロジック自体の回帰テストモードで、実ソースを使わずその場で
生成した fixture を `rustc -O --crate-type lib --emit asm` でコンパイルし、以下を
確認する。

- pass すべき形: `as_chunks::<N>` の**連続**要素から `_mm256_set_ps`／
  `vsetq_lane_f32` を構築するコード（現行 `dot_lanes` 相当の自動ベクトル化を含む）
  → 禁止命令 0 件で pass すること
- fail すべき形: `as_chunks::<2N>` から**ストライド 2** で要素を抜き出す
  ギャザー構築 → x86_64 は `vinsertps`、f16 変換を伴う `_mm_set_epi16` は
  `vpinsrw` を検知して fail すること。aarch64 のストライド `vsetq_lane_f32` は
  レーン指定 `ld1 {...}[n]` を検知して fail すること
- 必須シンボル不在の fixture → 「required symbol missing」で fail すること

不適だった fixture（再提案防止のための記録）:

- `_mm_insert_ps` 明示 intrinsic は `vbroadcastss` へ最適化され、`vinsertps` を
  **残さない**ため fail fixture として不適
- 要素変換後の `set_ps`（型変換を挟む構築）は `vinsertf128`／`vshufps` へ最適化
  され、`vinsertps` を残さないため同様に不適

## 7. 既知の限界

- 本ガードは「現行の LLVM が特定の書き方をどう最適化するか」を固定するもので
  あり、rustc/LLVM の更新で最適化挙動が変わればガードが fail する（これは
  ガードの目的そのもの——期待どおり検出できたことを意味する）
- `[profile.release]` に LTO・codegen-units 変更を加えた場合、生成される
  シンボル構成が変わりうる。必須シンボル検査（§2 手順 3）がその種の構成変更を
  検知する
- aarch64 は本開発環境でのクロスコンパイル（`cargo rustc --target
  aarch64-unknown-linux-gnu --emit asm`。リンク不要）による検証であり、実機での
  検証は対象外（`docs/design/hnsw-parallel-build.md` 等、他の aarch64 関連確認と
  同様に別 Issue の管轄）
- キャッシュ（actions/cache 等）を導入していないため、CI 実行のたびに engine
  クレートの release フルビルド（wgpu 等の依存を含む）が走る。CI 実行時間が
  問題になった場合はキャッシュ導入を検討する（follow-up）

## 8. 運用

検査パターンの変更・対象モジュールの追加（将来 `f16.rs` 等の新設カーネルへの
拡張）は、`scripts/check_simd_codegen.sh` の修正と本 ADR の更新をセットで行う。
