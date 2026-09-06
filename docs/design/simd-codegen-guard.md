# SIMD カーネル生成コード検査ガード

- ステータス: Accepted
- 対応: Issue #467（親 #456・ルート #455）
- 関連ポインタ: `docs/spec/05-tasks.md`（TASK-156）・`docs/spec/04-behavior/core-engine.md`（CORE-14）。spec 本文は転記しない
- production コード変更: なし（`crates/engine/src/`）。CI ガード・スクリプト・docs のみ

## 動機

`crates/engine/src/isa.rs` の x86_64 SIMD カーネル（`dot_avx2_fma`・`dot_avx512`）は
現状 intrinsics 不使用（`#[target_feature]` fn ＋ `dot_lanes` の LLVM 自動ベクトル化）
だが、Phase 4（ADR 別途）では `as_chunks` の固定長配列から `_mm256_set_ps` 等で
SIMD 値を構築する方式（新規 `unsafe` を持たない）を採る予定である。

この方式が「連続要素の単一ロード命令（`vmovups`／メモリオペランドの
`vcvtph2ps`）へ畳み込まれる」のは LLVM の最適化挙動であり言語仕様の保証では
ない。ストライド・非連続な要素順で `_mm256_set_ps`／`_mm_set_epi16` を呼ぶと
要素ごとの挿入命令（`vinsertps`／`vpinsr*`／`vunpck[lh]ps`）へ分解され、期待した
性能特性を失う。`rust-toolchain.toml` は `channel = "stable"` 追従（floating
stable）のため、toolchain 更新でこの畳み込みが退行しても気づけない構造だった。

本 Issue では、`cargo rustc --emit asm` で実際の生成コードを検査し、要素ごと
挿入命令の再混入を CI で機械的に検知するガードを追加した。

## 検査方式

`scripts/check_simd_codegen.sh`（`scripts/check_sort_determinism.sh`・
`scripts/check_core_api.sh` と同様の構成。fail-closed）:

1. **カバレッジガード**: `crates/engine/src/isa.rs`（存在すれば
   `crates/engine/src/isa/*.rs` も）から `#[target_feature]` 直後の fn 名を
   抽出し、スクリプト内のカーネル表（x86_64: `dot_avx2_fma`・`dot_avx512`）の
   和集合と一致することを確認する。新規カーネル追加時の登録漏れを検知する。
2. 専用 `CARGO_TARGET_DIR`（`target/simd-codegen`）で `engine` パッケージだけ
   `cargo clean` してから `cargo rustc --release -p engine --lib -- --emit
   asm` を実行する（`--emit asm` は cargo のフィンガープリントに含まれない
   追加出力のため、直前のビルドと同一フィンガープリントだと cargo が
   rustc を再実行せず `.s` が生成されない問題を避けるため、毎回明示的に
   クリーンしてからフルビルドする）。生成された `deps/engine-*.s` が
   ちょうど 1 個であることを assert する。
3. 各カーネルのシンボルを mangled 名（legacy `_ZN...17h<16進>E:` と
   v0 `_R...:` の両形式、長さ接頭辞つきで前方一致誤検知を排除）で解決し、
   ラベル行 〜 `.Lfunc_end<N>:` を関数本体として抽出する。
4. 関数本体に対し、禁止命令（`vinsertps`／`insertps`／`vpinsr[bwdq]`／
   `pinsr[bwdq]`／`vunpck[lh]ps`）が 0 件であることを確認する。
5. **非 vacuous 検査**: 広幅（ymm/zmm オペランドを持つ）`vfmadd[0-9]{3}ps` が
   1 件以上あることを確認する。検査対象の関数本体抽出そのものが壊れて
   「命令が無い＝pass」の空振り green になる事故を防ぐ。
6. シンボルが 0 件・2 件以上（曖昧）・関数本体抽出失敗（`.Lfunc_end` 不在）は
   いずれも ERROR（fail-closed）。

`--self-test` は検査ロジック自体の回帰テスト。cargo を使わず `rustc -O
--crate-type lib --crate-name fixture --emit asm` で用意した fixture に対し、
以下を確認する:

| fixture | 内容 | 期待 |
| --- | --- | --- |
| `pass_set_contig` | `as_chunks::<8>` → 連続要素から `_mm256_set_ps` → `_mm256_fmadd_ps`（`unsafe` ブロックは使うが intrinsics 呼び出し自体は安全な要素順） | PASS |
| `pass_set_contig`（v0 mangling） | 同上を `-C symbol-mangling-version=v0` で再コンパイル | PASS（シンボル解決の両形式確認） |
| `fail_set_strided` | `as_chunks::<16>` の偶数番ストライド要素から `_mm256_set_ps` | FAIL（`vinsertps`） |
| `fail_no_wide_fma` | `#[target_feature]` 付きだがスカラー演算のみ | FAIL（非 vacuous 検査） |
| 表にない名前をカーネルとして指定 | — | ERROR（シンボル不在・fail-closed） |

いずれも同一の `check_asm_file` 関数を実検査・self-test の両方が経由する
（検査ロジックの二重管理を避ける）。

## 実装上の注意点（デバッグで判明した事項）

- `#[target_feature]` の付いた private 関数は release ビルドでデッドコード
  除去され `.s` にシンボルが現れない。self-test fixture では `pub fn` にする
  ことで回避した。
- 関数本体抽出で `tail | awk '/marker/{exit}'` のように awk 側が早期に
  `exit` すると、`tail` がまだ書き込み中のパイプが閉じられ `SIGPIPE`
  （終了コード 141）を受け取る。`set -euo pipefail` の下ではこれがそのまま
  スクリプト全体の異常終了になる（`.s` ファイルが数十万行あるため必ず再現
  する）。awk 側を「マーカー以降はフラグで印字を抑制するだけで最後まで
  読み切る」実装に変更して回避した。

## 基線記録（2026-09-06・rustc 1.96.0 スナップショット）

pinned な期待値ではなく、toolchain（floating stable）drift の検出用に一度
実測した記録。`make simd-codegen-check` を再実行すればこの環境の最新値が
得られる。

| カーネル | 主な広幅命令 | 挿入系命令 |
| --- | --- | --- |
| `dot_avx2_fma` | `vmovups` × 5・`vfmadd231ps` × 4・`vfmadd132ps` × 1 | 0 |
| `dot_avx512` | `vmovups` × 5・`vfmadd231ps` × 4・`vfmadd132ps` × 1・`vextractf32x4` × 2 | 0 |

いずれも `dot_lanes` の LLVM 自動ベクトル化による現状実装であり、要素ごと
挿入命令は 0 件（intrinsics 未使用のため当然だが、ガード導入時点の基線として
記録する）。

## aarch64 の確認結果

`cargo rustc --release -p engine --lib --target aarch64-unknown-linux-gnu --
--emit asm` はクロスリンカ不要（`--emit asm` はコンパイルのみ）で成功する。
ただし `dot_neon` は独立シンボルとして **存在しない**。NEON は aarch64 の
baseline feature（アーキテクチャ仕様上必ず対応）のため、`SimdKernel::dot`
の呼び出し元と `#[target_feature]` が実質同一視され、コンパイラが呼び出し元
へインライン化してしまう。そのため本ガードの x86_64 と同じ「シンボルを
mangled 名で解決して本体を検査する」方式は aarch64 では成立しない。

レーン挿入命令の aarch64 対応物は `ins v.s[i]`／`ld1 {v.s}[i]`（レーン単位の
挿入・ロード）であり、ストライド要素から NEON レジスタを構築する fixture では
これらが観測される。`fp16`／`dotprod` 等 baseline 外の feature を使う
将来カーネルであれば独立シンボルとして残る見込みだが、現時点で対象カーネルは
`dot_neon` のみのため、aarch64 の CI ゲート化（`cross-check` ジョブへの
`--emit asm` 追加）は本 Issue の対象外とし、Phase 4 の ADR（#508）側の判断
事項として申し送る。

## 既知の限界

- `vunpck[lh]ps` は現在の基線で 0 件だが、将来の縮約 tail（水平和の実装）で
  正当な理由により出現する可能性がある。出現した場合は本ガードの禁止命令
  リストの見直し・許可マーカー方式の追加を検討する。
- 呼び出し元と同一 `target_feature` の関数へインライン化されるとシンボルが
  消え、fail-closed でシンボル未発見 ERROR になる。その場合はカーネルへ
  `#[inline(never)]` を付けるか、本スクリプトのカーネル表を見直す。
- `.s` ファイルが 1 個であることを assert する前提は release プロファイルの
  既定 codegen-units 設定に依存する（`-C codegen-units` を明示指定すると
  複数ファイルに分割されうる）。

## self-test fixture 一覧

`scripts/check_simd_codegen.sh --self-test` 参照（本文中「検査方式」節の表と
同一）。
