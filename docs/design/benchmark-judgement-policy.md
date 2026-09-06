# 性能判定の計測規約（交互 min-of-N・ノイズ帯併記・専有環境判定）

- ステータス: **Accepted**（既存慣行〔Issue #365・#366・#400・#401〕の統合であり
  production コード〔`crates/engine/src/`〕は無変更。後続 perf Issue〔#463 以降〕
  が本 doc を受け入れ条件の前提として参照するため Proposed で止めない）
- 対応: Issue #462（親 #456／ルート #455）
- 関連ポインタ: TASK-83（Conditional Go 条件7）・TASK-127（CORE 系ゲート）・
  TASK-130（CORE-6/7/16）。判定内容・数値基準は spec 側が SSOT であり、
  本 doc には記載しない

## 1. 背景・目的

Issue #365（dot カーネル多アキュムレータ化）・#366（距離計算/Top-k 2 段分離）・
Issue #400（redb `insert_reserve`）・#401（書き込み経路前後比較）の各 ADR は、
それぞれ本文中で個別に計測規約（交互実行・per-run 生データ保持・参照区間
ノイズ帯・共有環境の数値を採否根拠にしない、等）を定めてきた。ADR ごとに統計量（min-of-N か
median か）・ペア数（3〜5）・ノイズ帯の定義（固定 ±5% か参照区間実測か）が微妙に
異なり、後続 Issue は「Issue #365・#366 の判定規約」と口頭参照するしかない状態に
あった。

本 doc は既存の計測規約を 1 箇所に集約し、次の 3 点を提供する。

1. 判定規約（統計量・ペア数・ノイズ帯の定義）の統一
2. 本開発環境で構造的に判定できない施策種別の列挙
3. 後続 perf Issue がそのまま貼れる受け入れ条件テンプレート

本 doc は後続 perf Issue の受け入れ条件から参照される、本リポ側の SSOT として
位置づける。spec 側の受け入れ基準の数値（閾値そのもの）は対象外であり、あくまで
「どう測り、どう判定するか」という方法論を定める。

## 2. 用語

| 用語 | 意味 |
| --- | --- |
| before / after | 比較対象の 2 状態（変更前コミット／変更後コミット、あるいは baseline 実装／候補実装） |
| ペア | before 1 回 → after 1 回（3 候補以上の場合は baseline → cand1 → baseline → cand2 → … の輪番）を 1 組とした計測単位 |
| 交互実行 | ペア単位で before/after を交互に実行する方式。逐次実行（before を N 回連続 → after を N 回連続）の対義語 |
| min-of-N | N ペアのうち各系列の最小値を採る統計量。環境ノイズは基本的に加算方向にしか働かないという前提に立つ |
| median-of-N | N ペアの中央値を採る統計量。外れ値の影響を抑える |
| 参照区間 | 変更を含まない区間（対象の変更が影響しないフェーズ・パス）。その run-to-run 差分を実測ノイズ帯として使う |
| 固定ノイズ帯 | 比率に対する固定の許容幅（既定 ±5%）。`crates/engine/benches/harness/dot_kernel.rs::classify_change` の `noise_band` |
| 実測ノイズ帯 | 同一計測セッションで得た参照区間の run-to-run 幅 |
| 専有環境 | 他プロセスと CPU/IO リソースを共有しない環境（`BENCH_DEDICATED_ENV=1` の自己申告） |
| 共有環境 | 専有環境以外（本開発環境の QEMU VM・GitHub ホステッド runner を含む） |
| 判定クラス | `Improved` / `Neutral` / `Regressed`（`classify_change` の `ChangeClass` と同名。固定ノイズ帯で比率を 3 分類する） |

## 3. 計測プロトコル（必須事項）

- **交互実行**: before 1 回 → after 1 回を 1 ペアとし **N ≥ 5 ペア**を実行する
  （本 doc で統一。Issue #401 の 3 ペア・#366 の「最低 4 ペア」はいずれも本 doc の
  下限 5 未満のため、今後の新規計測では不可とする）。逐次実行は時間方向の交絡
  （測定順で環境条件が変化する）を招くため禁止する。3 候補以上を比較する場合は
  `baseline/cand1/baseline/cand2/...` の輪番（Issue #365 方式）とする。
- **ビルド条件の統一**: before/after は同一プロファイル・同一 `Cargo.lock` で
  ビルドする。バイナリを退避し、同一プロセス条件（同時実行プロセス等）で実行
  する。
- **per-run 生データの記録を必須とする**（Issue #366 の教訓: min-of-N のみを
  保持し生データを残さなかったため、事後の再判定ができなくなった経緯がある）。
  実測記録には min・median・各 run の値（または値列）を残す。
- **統計量は min-of-N と median の両方を必ず併記する**（本 doc で新たに統一。
  従来は ADR ごとに min-of-N のみ〔#366〕・median のみ〔#365〕・両方〔#400〕と
  分かれていた）。レイテンシ・所要時間系の主統計量は min-of-N（環境ノイズは
  加算方向のみという前提）とし、median を交差確認として併記する。CORE-7 型の
  「劣化率 %」のような分位点ゲートは既存どおり試行間 median を主統計量とする。
- **環境記録**: `lscpu` の Model name・命令セットフラグ（`avx2`／`fma`／`f16c`／
  `avx512*`／`neon` 等の有無）・`nproc`・各 run 時点の `loadavg`・同時実行プロセス
  の有無・`BENCH_DEDICATED_ENV` の設定有無・計測対象コミットの hash（before/after
  双方）を記録する。

## 4. ノイズ帯の定義（2 種を両方満たすこと）

判定に効かせる差分は、次の 2 種のノイズ帯を**両方**超えていることを要件とする。
片方のみを超える場合は「ノイズ帯内」として記録し、採否の根拠にしない。

1. **固定相対帯**: ±5%（`benches/harness/dot_kernel.rs::classify_change` の
   `noise_band = 0.05`）。`ratio = after / before` を計算し `Improved` /
   `Neutral` / `Regressed` に分類する。
2. **実測帯**: 同一計測セッションで得た「変更を含まない参照区間」の run-to-run
   幅（例: Issue #366 の S5prime 系列 -12.5%、Issue #401 の before 側 3 run 幅、
   `docs/design/crossdb-bench.md` の同一条件 2 回実測 716 vs 761 µs）。参照区間は
   計測を始める前に固定して指定する。

非退行判定（参照区間方式）は Issue #401 §2.2 の閾値を既定として継承する。
p50 が before の ×1.05 以下、p95 が before の ×1.10 以下であれば非退行とし、
超過した場合は before 側の run-to-run 幅と比較してノイズ帯内か退行の疑いかを
切り分ける。

## 5. 環境別の証拠力（何を結論できるか）

| 結論種別 | 共有 QEMU 本環境 | 専有環境（`BENCH_DEDICATED_ENV=1` 自己申告） | GitHub ホステッド runner |
| --- | --- | --- | --- |
| production 変更の棄却（Rejected・現状維持） | 可（両ノイズ帯を超える一貫した悪化＋静的解析／実アセンブリの裏付けがある場合。Issue #365・#400 の先例） | 可 | 対象外（perf 系ベンチは opt-in・CI 非配線が基本） |
| 構造的非退行の記録（参照区間方式） | 可（Issue #401 の先例） | 可 | 対象外 |
| perf 動機の production 変更の採用（Accepted） | **不可**（採否根拠にしない。参考値として記録するに留める） | 可 | 不可 |
| 絶対閾値ゲートの確定判定（TASK-83 条件7・CORE-6/7/16） | **不可** | 可 | 不可（GPU 非搭載・専有性なし） |
| 段別内訳の帰属分析（比率のみ。Issue #356・#362 型） | 可 | 可 | 対象外 |
| 複数規模点の同一プロセス内逐次比較 | **不可**（Issue #313 の教訓。規模点をまたぐ逐次測定は比較不能なノイズが乗る） | 規模点ごとにプロセスを分ければ可 | 対象外 |

`BENCH_DEDICATED_ENV=1` は運用者の自己申告のみで成立するフラグであり、自動検出
の仕組みは持たない。GitHub ホステッド runner へは注入しない（README「C1 p95
専有環境再測定（TASK-83）」節・`.github/workflows/bench.yml`・
`docs/design/ci-gate-variables.md`「意図的に据え置く事項」と整合）。

## 6. 本開発環境のプロファイルと判定不能な施策種別

本 doc 立案時点で確認した本開発環境の実測プロファイル:

- CPU: `QEMU Virtual CPU version 2.5+`（KVM）・12 vCPU・31 GB
- 命令セットフラグ: `avx2` / `fma` / `f16c` のみ。`avx512*` は無し
- GPU: RTX 3060（PCIe パススルー）
- 負荷: 別プロジェクトのコンテナが常駐し loadavg 約 2（非専有）

この環境では次の施策種別が構造的に判定不能（実行不能を含む）である。

| 種別 | 理由 | 関連 Issue | 代替手段 |
| --- | --- | --- | --- |
| dot カーネル・距離カーネルの命令レベル最適化 | cache 常駐 dim での差が固定 ±5% 帯内に収まりやすく、共有 QEMU 環境のノイズと切り分けが困難 | #365・#463 | 実アセンブリ確認・静的解析との併用 |
| キャッシュ規模依存のレイアウト最適化（CSR 化・prefetch・チャンク連続格納） | 仮想 CPU のキャッシュ階層が実機と異なる | #364・#489・#492 | 専有実機での再実測 |
| AVX-512／VNNI 経路 | 対象命令が存在せず実行不能 | #510・#520・#528 | 対応 ISA を持つ環境での実測 |
| NEON／Apple Silicon／Metal | 対象 ISA・GPU が存在しない | #468・#524・#313 | チップ別手動計測手順（#469） |
| 絶対値 p95 閾値ゲート | 共有環境の絶対値は spec 閾値との比較に使えない | #314・TASK-83 条件7 | 専有環境（`BENCH_DEDICATED_ENV=1`）での運用者実測 |
| GPU バッチの規模点比較 | PCIe パススルー VM 特有のノイズ・複数規模点の逐次測定が不能（Issue #313 の教訓） | #313 | 規模点ごとにプロセスを分けた単発実測 |

判定不能な種別で得られた数値は「参考値」として記録し、専有環境（または該当 ISA
／GPU を持つ環境）での再実測をオーナーへ申し送る。専有環境の確保自体は
オーナー作業であり、本 doc の対象外とする（チップ別の手動計測手順は #469 を
参照。#469 が策定次第、本 doc から相互リンクする）。

命令レベル最適化・キャッシュレイアウトの生成コード検査手順は
`docs/design/hotpath-implementation-survey.md`・`docs/design/chip-kernel-guidelines.md`
（いずれも Issue #470 で作成予定。本 doc の時点では未作成）を参照する。

## 7. 受け入れ条件テンプレート

後続 perf Issue の受け入れ条件へそのまま貼れる形を示す。

### 7.1 チェックリスト（Issue 本文用）

- [ ] 対象 crossdb フェーズ名（§7.3 の固定語彙から選択）
- [ ] 競合の実測値（p50・p95）と出典（`docs/design/crossdb-bench.md` の該当表
      ＋計測時点のコミット hash）
- [ ] self の before・after コミット hash
- [ ] 環境（専有 or 共有・CPU 命令セットフラグ・`nproc`・各 run の `loadavg`）
- [ ] ペア数（N ≥ 5）
- [ ] min-of-N と median の両方を記録
- [ ] 参照区間名とその実測ノイズ帯
- [ ] 固定帯 ±5% での判定クラス（`Improved` / `Neutral` / `Regressed`）
- [ ] 環境適格性（共有 QEMU 環境の場合は「参考値・採否根拠にしない」と明記）
- [ ] production 変更の有無

### 7.2 実測記録表のひな形

| 区間 | before min | before median | after min | after median | ratio (min-of-N) | 判定クラス | 参照区間帯 |
| --- | --- | --- | --- | --- | --- | --- | --- |

### 7.3 crossdb フェーズ名の固定語彙

出典: `docs/design/crossdb-bench.md`。値の再転記は最小限に留め、必ず計測コミット
を添える。

- レイテンシ 13 フェーズ: `vector_knn` / `vector_knn_where` /
  `where_compound_count` / `agg_count` / `agg_multi` / `group_by_having` /
  `hybrid_rrf` / `mode_recall` / `mode_precision` / `udf_call` /
  `rls_isolation` / `explain` / `ingest`
- 広域取得 5 フェーズ: `bulk_knn_k200` / `bulk_knn_k1000` /
  `bulk_knn_where_k200` / `bulk_hybrid_k200` / `scan_where_nosort_k500`
- スループット・品質: `ingest_bulk` / `ingest_single_stmt` / `recall_at_10` /
  `recall_at_10_strict`

### 7.4 記入例

`vector_knn` フェーズ: self 786/1147 µs（p50/p95）vs Qdrant HNSW 559/627 µs
（`docs/design/crossdb-bench.md` 記載時点のコミット `559b523`）。self 側の
before/after 比較を追加する場合は本 doc §7.2 のひな形に従い、min-of-N ≥ 5 ペア
・参照区間併記で記録する。

## 8. 既存 ADR との対応表

| 出典 Issue | 本 doc の吸収先 | 統一に伴う差分 |
| --- | --- | --- |
| #365（dot カーネル多アキュムレータ） | §3〜4 | 統計量: median のみ → min-of-N と median の両方を必須化（本 doc で新規統一）。固定 ±5% 帯の定義はそのまま採用 |
| #366（距離計算/Top-k 2 段分離） | §3〜4 | ペア数: 「最低 4 ペア」→ 5 ペア以上へ引き上げ。min-of-N のみ → median も併記に変更。per-run 生データ必須の教訓はそのまま §3 に反映 |
| #400（redb `insert_reserve`） | §3〜4 | ペア数 5・min-of-N と median の併用は本 doc の方針と一致（変更なし）。参照区間方式のノイズ帯定義もそのまま採用 |
| #401（書き込み経路前後比較） | §3〜5 | ペア数: 3 → 5 以上へ引き上げ。参照区間の非退行閾値（p50 ×1.05・p95 ×1.10）はそのまま §4 に継承 |
| CORE-7（Issue #302） | §3 | 分位点ゲート型の「劣化率 % は試行間 median」という方針はそのまま継承 |
| TASK-83 条件7（Issue #314） | §5〜6 | 「共有 QEMU 環境の絶対値は判定材料にならない」「`BENCH_DEDICATED_ENV` は自己申告」という既存整理をそのまま §5 の表へ反映 |

## 9. スコープ外・申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）の確保自体はオーナー作業として従来どおり
  未実施のまま引き継ぐ。
- `docs/design/hotpath-implementation-survey.md`・
  `docs/design/chip-kernel-guidelines.md`（Issue #470 が作成予定）との相互リンク
  化は、#470 マージ後に別途行う。
- 既存 ADR（#365・#366・#400・#401）本文の計測規約記述を本 doc への参照へ
  書き換える retro 編集は行わない。各 ADR は当時の計測記録として残置し、本 doc
  の §8 で吸収関係を示すに留める。
- `CLAUDE.md`「ステータス」行が同一内容で重複している事象は本 Issue 以前からの
  既存事象であり、本 Issue のスコープ外とする。
- `feature_bench`／`bench-*` ハーネスへ「交互ペア実行・min/median 自動集計」を
  組み込む自動化は別 Issue 候補（本 Issue は規約の明文化までを範囲とする）。
- Recall ゲート（`hybrid_recall.rs` 等の決定的フィクスチャによる時間非依存の
  判定）は本 doc の対象外（本 doc はレイテンシ・所要時間系の計測を対象とする）。
