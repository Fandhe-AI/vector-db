# CLAUDE.md

## Overview

Rust 製のローカルファースト・vector 特化クエリ DB の実装リポジトリ。「正解を含むデータ群を広く返す」ことを設計思想とし、LLM のコンテキストとして渡す用途に最適化する。

- **本リポは public**。仕様・ビヘイビア定義の SSOT は private リポ [vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec)（`docs/spec` submodule）。**spec 本文を public 資産へ転記しない**（[spec-confidentiality](.claude/rules/spec-confidentiality.md)）
- 接続プロトコル: PostgreSQL wire プロトコル v3 互換の自作実装（外部プロトコルライブラリ非依存）
- クレート構成: `engine`（コアロジック）＋ `wire-server`（lib+bin）の workspace（`crates/`）
- 永続化: `redb` ベース / 安全性: RLS 相当のテナント境界・fail-closed のエラー契約（`wire_code`）
- 依存は最小・`=x.y.z` 完全固定・ユーザー承認制（[dependency-policy](.claude/rules/dependency-policy.md)）
- ステータス: workspace 雛形構築済み（TASK-66）。wire プロトコル層は実装済み（TASK-67・TASK-68・TASK-69・TASK-70・TASK-71）。SQL 表層は許可リスト検証（TASK-74）・束縛と実行計画（TASK-75）・取得モード切替構文（TASK-161）・`precision` モードの実行契約（TASK-162）・宣言的 UDF 呼び出し（TASK-79）・宣言的フィルタ API（TASK-147）まで実装済み。簡易クエリプロトコルを SQL 表層へ接続（TASK-73・WIRE-1）。`precision` モードの評価ハーネス実装済み（TASK-163。目標値の確定はユーザー判断待ち・`.github/workflows/recall.yml` 未接続）。モード切替・`precision` 契約の wire 経由 3 クライアント検証実装済み（TASK-165・SQL-12・SEARCH-9）。RLS 暗黙適用の全読み取り経路一般化検証を実施済み（TASK-138）。障害回復の `operation_id` 必須化ガード実装済み（TASK-92）。テーブル単位 `operation_id` 台帳実装済み（TASK-93）。台帳エントリへの内容照合ハッシュ（`operation_id` 再送時の内容一致/不一致判定。同一内容の再送は `23505`、内容不一致は `22023`。TASK-94・RECOVER-3 の重複拒否契約を包含する形で TASK-101・RECOVER-10 として実装済み）。集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`、TASK-166・SQL-13）・`GROUP BY`/`HAVING` 集計（TASK-167・SQL-14）まで実装済み。集計クエリの wire 経由 3 クライアント検証実装済み（TASK-168・SQL-13・SQL-14）。増分インデックス反映（TASK-120。ファイル形 `INSERT`（`path`/`body` 列指定）→ チャンク化 → 注入型 `Embedder` によるベクトル化 → 同一パス置換書き込み。外部埋め込みサービス実クライアントは未接続、注入点までが実装範囲）。増分インデックスの回帰テスト実装済み（TASK-121。本リポ独自の回帰基準による。Issue #281 で `t_full` の測定方式を見直し、一括投入 API による全体再構築計測・処理量カウント判定・規模スケーリング判定の 3 本柱へ再構成し、退行の実測モデルによる vacuous pass 防止テストを追加）。一括投入の処理量上限（TASK-122・INDEX-4。本リポ独自の実装上の上限を `batch_limits.rs` で判定し、`EngineCore::execute_insert_sql_batch` から複数ファイルのバッチ投入を受け付ける）実装済み。エラー契約 `wire_code` 写像の共通分類実装済み（TASK-152・ERR-2）。ErrorResponse への横断写像を `wire-server/src/error_response.rs` として実装済み（TASK-153・ERR-1・`RECOVER-5` (3) ポインタ）。バッチ検索の実 GPU バックエンド（`wgpu` =30.0.1・オーナー承認済み〔2026-08-26〕）を `gpu_batch.rs` として接続済み（TASK-128〜130・Issue #178。ベンチは CORE-6・CORE-16 とも実測経路へ配線済み〔CORE-16 は f32 常駐対照経路 `GpuF32ContrastBackend` を Issue #234 で追加〕）。辞書的情報源抽出パイプライン実装済み（TASK-109・PLAN-5）。LLM クエリプランニング（TASK-110・PLAN-1。常駐 LLM プロセス〔Ollama〕へのクエリ展開クライアントを `query_planner.rs` として実装。依存追加なしの自作 HTTP/1.1・JSON。辞書スナップショットを固定接頭辞として束ねる注入点は `EngineCore::with_query_planner`／`plan_query`。実 Ollama への疎通確認は対象外・プロセス内 TCP スタブで契約を固定）実装済み。ソフトブースト機構（TASK-111・PLAN-1。TASK-110 のヒント〔`path_hint`／`kind_hint`〕を RRF 融合後スコアへの加点として反映する `hybrid::apply_soft_boost`／`hybrid_search_boosted` をハードフィルタ化しない設計で `hybrid.rs` に追加。`sql/exec.rs` への結線は対象外）実装済み。クエリプランナーのモード推定（TASK-164・PLAN-11。`query_planner.rs` の展開結果へ `mode_hint` を追加し、`sql/mode.rs::resolve_mode_with_planner`／`EngineCore::plan_query_with_mode` として注入。解決契約の詳細は spec のビヘイビア定義（PLAN-11）を参照）実装済み。クエリ展開の受け入れ基準検証（TASK-112・PLAN-1・PLAN-2。`crates/engine/tests/query_planning_recall.rs` として、決定的コーパスと決定的スタブ `LlmClient` により「言い換え語彙のみのクエリ（intent）」「コーパス語彙と一致するクエリ（direct）」双方の展開なし/展開あり Recall@20 を回帰テスト化。テスト専任タスクにつき production コード無変更）実装済み。同ハーネスへ数万チャンク規模の `direct` カテゴリ限定 Recall 回帰を追加済み（TASK-113・PLAN-3。`intent` の大規模測定は対象外。層 B ゲート `QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE` は `.github/workflows/recall.yml` の environment `recall-gate` へ接続済み）。質問類型推定・ティアリング（TASK-115・PLAN-8。対話ティア／高精度ティアの判定を `tiering.rs` として実装し `EngineCore::with_tiered_query_planner`／`plan_query_with_classification` から接続。判定基準は本リポの実装既定値として差し替え可能にした。詳細は `docs/design/query-tiering-criteria.md` 参照）実装済み。再埋め込み規則（TASK-114・PLAN-10。`query_planner.rs`（`render_reembedding_text`／`reembed_expansion`）・`EngineCore::plan_and_embed_query` として実装。SQL 表層〔`USING PLAN`〕への結線は対象外）実装済み。`USING PLAN('<query>')` 専用構文パース・ディスパッチ（TASK-77・SQL-5。`ORDER BY` と相互排他の規範形〔括弧必須。`$n` パラメータ形式・括弧なし・重複は `42601`〕を許可リストで受理し、`sql::using_plan::bind_expansion` が TASK-110 の `plan_query` 展開結果を再埋め込みのうえ既存 C4 ハイブリッド実行形へ一意にディスパッチ。ソフトブースト〔`path_hint`/`kind_hint`〕の `sql/exec.rs` 結線は対象外）実装済み。`EXPLAIN` 構文結線（TASK-78・SQL-6。`sql::allowlist::Statement::Explain`・`sql::explain` として実装。詳細は spec のビヘイビア定義（SQL-6）を参照）実装済み。commit 成功境界と応答一意性（TASK-96・RECOVER-5。`recovery::commit_boundary`）実装済み。commit 成功境界を跨いだ panic の観測可能性側（TASK-97・RECOVER-6。panic フックによる緊急応答〔TASK-153・ERR-1・`RECOVER-5` (3) ポインタ〕の同期送出を `recovery::panic_hook` として実装。安全性側の abort は RECOVER-5 の既存ガードが引き続き担う）実装済み。障害回復の直近操作照会 API（TASK-98・RECOVER-7。`LastOperationLookup`）実装済み。ソフトブースト機構（TASK-111）を一般化するスコアリングブースト API（TASK-148・EXT-4。`scoring_boost.rs`。SQL 表層への構文露出は対象外）実装済み。内部エラーの fail-fast 統一契約（TASK-99・RECOVER-8。回復可能エラーは既存の ERR-1/`wire_code` 応答で処理継続、panic は経路・スレッドを問わずプロセス終了を `recovery::fail_fast` として実装。`wire-server::main` が `panic_hook::install_panic_hook` の直後に結線し TASK-97・RECOVER-6 の緊急応答経路を維持）実装済み。索引反映途中失敗の注入試験（TASK-100・RECOVER-9。`crates/engine/tests/index_failure_injection.rs` の commit 前失敗・commit 後の検索段失敗・再オープン系に加え、再構築処理そのものの途中失敗〔`crates/engine/src/arena.rs` の `build_filtered_with_limits_failure_mid_rebuild_leaves_committed_rows_intact_and_recovers`〕まで）実装済み。LLM クエリプランニングのソフトブーストヒントと RLS 事前フィルタの統合検証を実施済み（TASK-139。`crates/engine/tests/plan_rls_boost.rs`・`docs/design/plan-rls-boost-interaction.md`）。ティア別レイテンシ受け入れ基準の検証ハーネス実装済み（TASK-116・対象ビヘイビア PLAN-4, PLAN-6, PLAN-7〔判定内容・測定段階は spec 側が SSOT〕。`crates/engine/benches/tier_latency_bench.rs`・`benches/harness/tier.rs`・`tests/tier_latency_accept.rs`。常駐 Ollama 前提のため CI 経路は持たず（GitHub ホステッド runner に常駐 Ollama が無く self-hosted は組織承認済み例外の範囲外のため。PR #269 Codex 指摘）、`make bench-tier` を Actions 外の承認済み計測環境で運用者が直接実行する（README「ティア別レイテンシ受け入れ基準の実測手順」）。目標値の確定・実測はユーザー判断待ち・`docs/design/tier-latency-acceptance.md` 参照。LLM 不正応答（`PlanError::InvalidResponse`）試行の除外・上限ガードを追加済み〔Issue #316。`harness/protocol.rs::run_fallible`・`harness/tier.rs` の分類関数・既定除外上限で実装。除外対象は `InvalidResponse` のみで `Timeout` 等は従来どおり致命扱い。production コード無変更・テスト専任〕）。`USING PLAN` の wire プロトコル経由・実クライアント実行検証（TASK-117・PLAN-9 確定化の層 A。`wire-server` バイナリへの `--planner-endpoint`／`--planner-model`／`--embedder-hashing-dim` opt-in 注入 CLI と、`crates/wire-server/tests/wire_using_plan.rs` による決定的スタブ経由の成功系・fail-closed 系・RLS 不変検証。実 Ollama・実クライアント 3 種での PLAN-9 数値基準実測ハーネスは未整備・別タスク）実装済み。拡張構文（SQL-5〜7・9・10）の wire 経由・3 クライアント実行検証（TASK-82。セッション経由の SQL 実行経路〔`EngineCore::execute_sql_in_session`〕へ `INSERT` ディスパッチを追加し wire 経由での `INSERT` 受理へ切り替え〔読み取り可視性の既定は拡大せず、書いた本人も同一 wire セッションでは読み戻せない非対称は維持。詳細は `docs/design/three-client-e2e-harness.md`〕。層 A に `wire_hint_order.rs`・`wire_udf_call.rs`・`wire_insert_operation_id.rs` を追加し、層 B `crates/wire-server/tests/extended_syntax_e2e.rs`〔`#[ignore]`・`make e2e-three-client`〕を新設）実装済み。`PrefilterCache::insert` の世代競合を `DictionaryCache` と同じ fail-closed 契約〔`None` で拒否〕に統一（Issue #280）。テーブル単位世代カウンタの拒否精度は現状維持で確定（Issue #285・`docs/design/table-generation-rejection-granularity.md`）。回帰ベンチ・Recall ゲートの閾値 ↔ spec ポインタ対応表・設定手順を整理（Issue #286・`docs/design/ci-gate-variables.md`。実値設定・`workflow_dispatch` 疎通確認はマージ後の管理者作業として引き続き未実施）。閾値注入を Actions variables から secrets へ移行（Issue #286。`env:` ブロックがログへ値付きで印字される variables では閾値〔spec 由来の非公開数値〕が public な Actions ログへ漏えいするため。opt-in フラグ `BENCH_CORE6`／`BENCH_CORE16`／`BENCH_DEDICATED_ENV` は非機密として variables のまま維持）。さらに `bench.yml` の閾値 secrets は repo レベルから main 限定 Environment `bench-gate`（`recall-gate` と同方針）へ再移設（codex-review P0 指摘・PR #299。`workflow_dispatch` の任意 ref 書き換えで repo レベル secrets を任意処理へ渡せる経路を塞ぐ）。Recall 系閾値ゲートの実測値出力は `RECALL_VERBOSE` opt-in・`GITHUB_ACTIONS` 下拒否に限定（Issue #303）。CORE-7 ゲート（動的窓集約を経由する単発クエリ経路の p95 劣化上限）の測定方式を Issue #302 で単発クエリ p95 の A/B（複数試行中央値採用）へ再整合し、旧来の集約器 push/drain 単体比較は合否に数えない診断へ降格（`docs/design/core7-dynamic-window-gate.md`）。小規模段 Recall@20 ゲート未達の engine 側原因調査（Issue #307・SEARCH-1・SEARCH-3。詳細は `docs/design/hybrid-recall-regression.md`「小規模段ゲート未達の engine 側原因調査（Issue #307）」節参照）実施済み。hybrid Recall@20 ゲート（小規模・大規模）の engine 側改善（Issue #310・SEARCH-1・SEARCH-3。RRF 融合の同点順位規約 `hybrid.rs::TieRank`〔既定 `GroupEnd`〕・密プール境界の同点グループ完全化〔`complete_boundary_tie_group`〕。詳細は `docs/design/hybrid-recall-regression.md`「Issue #310: engine 側改善」節・`docs/design/rrf-tie-break-determinism.md` 参照。fixture パラメータ非変更・production 変更〔`TieRank::GroupEnd` 既定・境界同点グループ完全化〕はオーナー判断で採用済み）実装済み。`recall.yml` の 3 ゲート（hybrid・rerank・query-planning）を独立評価・AND 集約へ変更（Issue #311・`docs/design/recall-gate-independent-evaluation.md`）。`rerank.rs::LexicalOverlapReranker` の既定重みを等重み（1.0:1.0）から fused 優位（`fused_weight:lexical_weight` = 3.0:1.0）へ変更し、`rerank_recall.rs` 大規模段の非劣化アサーション `after_hits20 >= baseline_hits20` の red（Issue #310 対応後に発生した字句一致優先による正解脱落）を解消（Issue #310・`docs/design/rerank-recall-regression.md`「実測結果」節参照）。境界同点グループ再取得ループ（PR #320・Issue #310）の単発クエリレイテンシへの寄与を計測するベンチを追加（Issue #324・CORE-7・PLAN-4/6/7 関連ポインタ。`crates/engine/benches/hybrid_latency_bench.rs`・`benches/harness/hybrid_latency.rs`・`tests/hybrid_latency_accept.rs`。単一ビルド内の A/B〔通常コーパス vs プロトタイプクラスタによる同点誘発コーパス〕方式で `make bench-hybrid` から実行する手動専用ベンチ。CORE-7 は `hybrid_search` を通らない測定経路のため実測が構造的に不変であることを 1 回実測で確認し、`bench-tier`〔PLAN-4/6/7〕の前後比較は常駐 Ollama 前提のため運用者実測へ申し送り。詳細・実測値は `docs/design/hybrid-refetch-latency.md` 参照。production コード〔`hybrid.rs`〕は無変更）。CORE-16（f16 常駐 vs f32 常駐）ゲートの Apple GPU（Metal）fail 切り分け（Issue #313。本開発環境〔NVIDIA GeForce RTX 3060・Vulkan backend〕でのクリーンな単独計測〔方法 A〕により engine 側シェーダの性能不足〔H3〕は否定できたが、Apple GPU/UMA 環境要因〔H1〕は未検証、逐次測定ノイズ〔H2-逐次〕と並ぶ共同主因候補にとどまり、discrete GPU 実測のみで Metal 固有のシェーダ性能問題を断定的には否定できない（詳細・判断根拠は `docs/design/core16-f16-resident-gate.md`「判断」節参照）。規模点診断 `BENCH_CORE16_DIAG`／`BENCH_CORE16_DIAG_SCALE_INDEX` を `batch_bench.rs` へ opt-in で追加（1 プロセス = 1 規模点。複数規模点の同一プロセス内逐次測定は比較不能な証拠と判明したため撤去。PR #326）。詳細・環境別 pass/fail 表は `docs/design/core16-f16-resident-gate.md` 参照。`gpu_batch.rs` は無変更）実施済み。SQL 表層 C1 p95 の専有環境での僅差 fail への対応（Issue #314・TASK-83 条件7。SQL 表層 C1 経路の行数比例な固定コスト削減——`storage::decode_row_embedding_and_metadata_into` によるスクラッチバッファ再利用、`sql/exec.rs` の不要な `Vec<Value>` 確保省略、`core.rs::execute_validated_in_session` による二重パース排除）実装済み。専有環境での再実測・条件7 の最終判定はオーナー作業として引き続き未確定（`docs/design/c1-p95-dedicated-env-reverification.md` 参照）。`USING PLAN` が実 Ollama 経由で SQL エラーなしに 0 行を返す事象の調査（Issue #315・SEARCH-3・SEARCH-9・PLAN-11。決定的フィクスチャでの検証により、プランナー推定 `precision` の確信度ゲートによる契約どおりの空集合応答として再現可能であることを確認。ただし実 Ollama の `mode` 値・実埋め込み出力そのものは未確認であり、実事象がこの経路によるものかは原因候補・最有力仮説の位置づけ。`EXPLAIN` の `mode_source` による切り分け手順を含め `docs/design/using-plan-precision-empty-result.md` 参照。production コード無変更）実施済み。rerank 大規模段 improvement@20 ゲート未達の是正（Issue #330・SEARCH-7。`rerank.rs::LexicalOverlapReranker::rank_lexical`（字句一致順位）の同点規約を位置順位から `rank_fused` と同じ `GroupEnd` へ統一し、字句信号の到達上限まで改善幅を引き上げ（after hits20 388→389）。fixture パラメータ非変更。詳細・原因分析は `docs/design/rerank-recall-regression.md`「Issue #330」節参照。マージ後の `recall.yml` 実測・Issue 記録はオーナー／管理者作業として申し送り）実装済み。PR #331 で同点規約整合後もなお改善幅ゲート `RERANK_RECALL_MIN_R20_IMPROVEMENT` が絶対差基準ではプール外正解の到達不能分まで分母に含み過小評価する構造だったため、オーナー承認済みの spec 改訂（vector-db-spec#7）に合わせ判定基準を候補プール上限に対する相対比率（`(after − baseline) / (pool_ceiling_hits20 − baseline_hits20)`。改善余地が構造的にほぼ 0 の場合は非劣化のみで判定）へ再定義（Issue #330・`rerank_recall.rs::RerankRecallResult::improvement_ratio`。実測 2/9 ≈ 0.2222。詳細は `docs/design/rerank-recall-regression.md`「Issue #330 追記」節・`docs/design/ci-gate-variables.md` 参照。production コード〔`crates/engine/src/`〕は無変更・テスト専任）実装済み。クロスエンコーダ型リランカーの実 ONNX バックエンド接続と自然言語 fixture による効果実測（Issue #333・SEARCH-7 方式変更。`rerank.rs` の `CrossEncoderBackend` trait・`CrossEncoderConfig`・`CrossEncoderReranker`・`CrossEncoderError` に加え、オーナー承認済み〔2026-08-30〕で `deny.toml` へ `RUSTSEC-2024-0436`〔`paste`・unmaintained のみ〕の ignore を登録のうえ `ort` `=2.0.0-rc.13`／`tokenizers` `=0.23.1`〔いずれも optional・`cross-encoder` feature 限定〕を追加し、実 ONNX 推論バックエンド `OnnxCrossEncoderBackend`（`rerank/cross_encoder_onnx.rs`）を接続。自然言語 fixture（`tests/fixtures/nl_qa.rs`）でのクロスエンコーダ実測ハーネス（`tests/rerank_cross_encoder_recall.rs`・`make rerank-cross-encoder-eval`）を追加し実測を実施——SEARCH-7 相対基準〔ratio ≥ 0.6〕には未到達（fixture の文書生成構造〔正解概念が非流暢な追記フレーズに偏る〕が事前学習済みモデルの評価に適さないことが主因と分析。詳細・実測値・原因分析は `docs/design/rerank-recall-regression.md`「Issue #333 追記」節参照。到達可否は Issue #330 へ報告）。自然言語 fixture の再設計とクロスエンコーダ improvement_ratio の再実測（Issue #337・SEARCH-7。`tests/fixtures/nl_qa.rs` の文書テキスト生成方式を再設計し、正解概念の非流暢な追記フレーズを全廃して全概念を流暢な自然文へ埋め込む方式へ変更〔正解集合を決めるキーワード抽選用 rng ストリームとは分離し比較可能性を維持。`ceil20`＝79 で不変を実測確認済み〕。再設計後もクロスエンコーダは SEARCH-7 相対基準〔ratio ≥ 0.6〕に未到達（実測 `improvement_ratio`＝0.0000。原因は fixture の非流暢構造だけでなく、語彙規模に対する概念間の表層語重複・ドメイン語彙密度と汎用モデルの学習分布の乖離の可能性が残る、と分析を更新）。詳細・実測値・原因分析は `docs/design/rerank-recall-regression.md`「Issue #337」節参照。到達可否は Issue #330 へ報告。production コード〔`crates/engine/src/`〕は無変更・テスト専任）実施済み。2 方式（字句一致・クロスエンコーダ）× 2 fixture の全実測未達（0.222・0）を根拠に、オーナー承認済み spec 改訂（vector-db-spec#8・2026-08-31）で SEARCH-7 の改善幅相対基準を実コーパス評価まで informational（非ブロッキング・実測値の記録と報告のみ必須）へ降格し、本リポ側も `rerank_recall.rs` のブロッキング判定を「絶対下限（`RERANK_RECALL_MIN_R20_LARGE`）＋非劣化」のみへ変更（`RERANK_RECALL_MIN_R20_IMPROVEMENT` は不読・`recall.yml` の env からも除去。improvement_ratio は informational ログとして出力継続。実コーパス評価時にブロッキング化を再判定。`docs/design/ci-gate-variables.md`・`docs/design/rerank-recall-regression.md`「SEARCH-7 改訂（2026-08-31）」節参照）。hybrid 実行が参照する `SparseIndex` のテーブル世代整合キャッシュ（Issue #357。`sql/sparse_cache.rs::SparseIndexCache`。フィルタなし hybrid クエリに限り `(table, PolicyContext)` × テーブル単位世代でキャッシュし、`sql/exec.rs::execute_statement` の Hybrid 分岐へ結線。`PrefilterCache`/`DictionaryCache` の fail-closed 契約〔Issue #280〕を踏襲しつつ、insert 失敗時は呼び出し元が自身のクエリスナップショットから構築した索引をそのまま使う契約に意図的に差別化。詳細は `docs/design/sparse-index-cache.md` 参照）実装済み。hybrid_rrf クエリの段別内訳プロファイル切り分け（Issue #356・親 Issue #355。`crates/engine/benches/hybrid_profile_bench.rs`・`benches/harness/hybrid_profile.rs`・`tests/hybrid_profile_accept.rs`。`make bench-hybrid-profile` から実行する手動専用ベンチで `SparseIndex::build` が hybrid 経路上乗せコストの過半を占めることを実測確認。詳細・実測値は `docs/design/hybrid-rrf-latency-breakdown.md` 参照。production コード無変更）実施済み。_rrf クエリの段別内訳プロファイル切り分け（Issue #356・親 Issue #355。`crates/engine/benches/hybrid_profile_bench.rs`・`benches/harness/hybrid_profile.rs`・`tests/hybrid_profile_accept.rs`。`make bench-hybrid-profile` から実行する手動専用ベンチで `SparseIndex::build` が hybrid 経路上乗せコストの過半を占めることを実測確認。詳細・実測値は `docs/design/hybrid-rrf-latency-breakdown.md` 参照。production コード無変更）実施済み。集計経路（TASK-166・TASK-167・SQL-13・SQL-14）の embedding 非参照デコードスキップと必要列限定デコード（Issue #350。`ReferencedColumns`・`DecodeTier` による 3 段階デコード〔ヘッダのみ／dim+マスク済みスカラー列／embedding 込み完全デコード〕を `sql/aggregate.rs`・`sql/group_by.rs` へ導入し、`row_codec::scan_scalar_columns_masked`・`storage::decode_row_dim_and_metadata_borrowed` を新設。RLS 可視性判定・TABLE-12 のキー/ヘッダ tenant 整合検査は全 tier で維持。詳細は `docs/design/aggregate-decode-skip.md` 参照）実装済み。他は未実装（タスクは spec リポの `05-tasks.md`（TASK-66〜165）、マイルストーンは `06-roadmap.md`（MS-1〜6）参照）

## Repository Structure

```text
vector-db/
├── CLAUDE.md / AGENTS.md          # Claude 運用方針 / レビュー観点集（codex-review の基準）
├── README.md                      # 概要・実装方針（要点）・開発環境構築
├── Makefile                       # タスクランナー（make setup / make ci / docker-*）
├── lefthook.yml                   # git hooks（rustfmt・secrets-guard・Conventional Commits・clippy/test）
├── Dockerfile / compose.yaml      # 環境非依存の開発コンテナ（make docker-ci）
├── deny.toml                      # cargo-deny 設定（make deny で有効化済み）
├── rust-toolchain.toml            # stable + rustfmt/clippy（単一真実源）
├── commitlint.config.mjs          # Conventional Commits 検証設定
├── skills-lock.json               # 導入スキルのロックファイル
├── docs/
│   ├── design/                    # 設計ドキュメント（ADR 形式・public）
│   └── spec/                      # vector-db-spec submodule（private・要アクセス権）
├── .github/workflows/
│   ├── ci.yml                     # lint-docs + rust-ci（fmt/clippy/test/cargo-deny）+ crash-test + crash-test-interrupt + crash-test-cross-table + core-api-check + sort-determinism-check + cross-check（aarch64 クロスコンパイル確認）の CI
│   ├── bench.yml                  # TASK-127 性能・Recall 受け入れ基準（CORE-5 は Issue #176 で usearch 接続済み・既定ゲート）+ TASK-130 バッチ高速化受け入れ基準（CORE-6/16 は GPU 搭載環境向けの Issue #178 opt-in）の回帰ベンチ（workflow_dispatch + 週次 schedule）+ TASK-83 SQL 表層 C1 p95 専有環境再測定（Conditional Go 条件7・workflow_dispatch 限定）
│   ├── recall.yml                 # TASK-104 ハイブリッド検索 Recall 回帰の層 B 閾値ゲート（workflow_dispatch + 週次 schedule。environment recall-gate + strict モードで閾値未評価runの誤green化を防止。pull_request 非対応＝spec 閾値の非公開ログ漏えい防止。PR ゲートは層 A が担う）
│   └── codex-review.yml           # PR 自動レビュー wrapper
├── .claude/
│   ├── agents/                    # カテゴリ別 subagent 定義
│   ├── rules/                     # 運用ルール
│   ├── skills/                    # npx skills add 導入スキル
│   ├── workflows/                 # implement-issue-tree.js (相対 symlink)
│   └── settings.json              # SessionStart / PostToolUse hooks
├── scripts/                       # 補助スクリプト（crash_test.sh・crash_test_interrupt.sh・crash_test_cross_table.sh・check_sort_determinism.sh 等。make 経由で実行）
├── Cargo.toml                     # workspace 定義（members: crates/engine, crates/wire-server）
└── crates/                        # engine（lib）/ wire-server（lib+bin）workspace
```

## 委譲方針（必読）

main セッションはオーケストレーションに徹し、調査・実装・レビューは subagent へ委譲してコンテキスト消費を抑える。詳細は [delegation](.claude/rules/delegation.md)（調査）・[delegation-impl](.claude/rules/delegation-impl.md)（実装）を参照。

### パスベース切り替え表

| 対象 | 調査 | 作成・編集 |
| ---- | ---- | ---------- |
| `crates/engine/` | explorer | engine-builder |
| `crates/wire-server/` | explorer | wire-builder |
| `docs/spec/`（private） | explorer（ポインタ表記） | 変更しない（spec リポ側で管理） |
| 外部仕様（pg wire v3・redb 等） | reference-researcher | — |
| テスト・lint | test-runner / linter | — |
| ドキュメント | explorer | docs-writer |

### model 配分表

| 用途 | model |
| ---- | ----- |
| 複雑な横断判断・アーキテクチャ設計 | opus または fable（fable は特に大規模設計・横断判断の最上位 tier） |
| 調査・生成・実装・レビュー | sonnet |
| 機械的集計・lint・ドキュメント更新 | haiku |

## Sub-agents

| カテゴリ | subagent_type | model | 役割 |
| -------- | ------------- | ----- | ---- |
| research | explorer | sonnet | コードベース・spec 横断調査（spec はポインタ表記） |
| research | reference-researcher | sonnet | 外部仕様・依存候補クレートの調査 |
| implement | engine-builder | sonnet | engine クレート（検索カーネル・認証・RLS・永続化）実装 |
| implement | wire-builder | sonnet | wire-server クレート（pg wire v3 自作実装）実装 |
| testing | test-runner | sonnet | cargo test / clippy 実行と失敗解析 |
| quality | reviewer | sonnet | AGENTS.md P0/P1/P2 観点のレビュー |
| quality | security-auditor | sonnet | テナント境界・wire 入力・spec 漏えい・OWASP 監査 |
| quality | linter | haiku | rustfmt / clippy / markdownlint 等の機械的確認 |
| docs | docs-writer | haiku | README・CLAUDE.md・ドキュメント更新 |

## Rules

| ファイル | 内容 |
| -------- | ---- |
| [delegation.md](.claude/rules/delegation.md) | 調査フェーズの委譲原則・パスベース切り替え |
| [delegation-impl.md](.claude/rules/delegation-impl.md) | 実装フェーズの委譲マッピング・標準フロー |
| [coding-rust.md](.claude/rules/coding-rust.md) | Rust 規約（untrusted 入力・fail-closed・unsafe 原則禁止） |
| [security.md](.claude/rules/security.md) | OWASP Top 10・秘密情報混入防止・テナント境界 |
| [japanese-style.md](.claude/rules/japanese-style.md) | 日本語出力スタイル |
| [conventional-commits.md](.claude/rules/conventional-commits.md) | Conventional Commits 詳細規約（type/scope 一覧） |
| [code-comment-style.md](.claude/rules/code-comment-style.md) | コメント規約（役割・責務・呼び出し文脈の埋め込み） |
| [out-of-scope-tracking.md](.claude/rules/out-of-scope-tracking.md) | スコープ外事項の Issue 追跡フロー |
| [spec-confidentiality.md](.claude/rules/spec-confidentiality.md) | **リポ固有・P0**: private spec のポインタ表記運用 |
| [dependency-policy.md](.claude/rules/dependency-policy.md) | **リポ固有**: 依存最小・`=x.y.z` 固定・ユーザー承認制 |

## Current Skills

`npx skills add`（Fandhe-AI/agent-cli-skills ほか）で導入済み。ロックは `skills-lock.json`。

- **ワークフロー系**: create-commit / create-pr / create-issue / create-issue-tree / create-plan / implement-issue / implement-issue-tree / implement-review / implement-review-pr / update-issue-tree / update-docs / comment-code
- **メンテ系**: init-claude / update-claude / contribute-skill / sync-skills-lock / setup-repo-guards
- **リファレンス系**: rust / github-docs / commitlint / lefthook / editorconfig / nvidia-cuda / amd-rocm / apple-silicon

## Conventions

- **環境構築・検証**: `make setup`（submodule → rustup → lefthook）で構築し、push 前に `make ci`（CI と同等のチェック）をローカル実行する。cargo 系ターゲットは workspace 追加（TASK-66）により有効化済み。`make lint`／`make test`（lefthook pre-push 含む）は `--all-features` で実行するため `contrast-bench` feature 経由で usearch（optional 依存）の C++ ビルドが走る。**C++17 コンパイラが必須**（GitHub ホステッド `ubuntu-latest` には同梱済み。ローカルに無い場合 `make lint`／`make test`／`make ci` が失敗する。詳細は README「回帰ベンチの Environment `bench-gate` secrets」節）
- **日本語**: やりとり・報告・コミット説明文・コード内コメントは日本語（プログラム出力文字列は英語）
- **Conventional Commits**: commitlint で検証。`--no-verify` 禁止
- **セキュリティレビュー**: PR 作成前に OWASP Top 10＋AGENTS.md P0 観点（spec 漏えい・テナント境界・wire 入力検証）を確認
- **ユーザー承認フロー**: 依存の追加・更新 / Issue 起票 / 既存ファイル上書き / implement-issue の実装開始（計画承認後）は必ずユーザー承認を経る
- **spec ポインタ運用**: `docs/spec` の内容は TASK-nn・ビヘイビア ID・パスのポインタ表記でのみ参照する

## hooks（settings.json）

- **SessionStart**: 日本語・委譲・Conventional Commits・`--no-verify` 禁止・spec 漏えい注意・依存承認制のリマインダーを表示
- **PostToolUse**（Edit|Write）: `*.rs` 編集後に rustfmt で自動整形。edition は workspace の正である `Cargo.toml` から取得し（lefthook.yml の rustfmt-check と同一方針）、Cargo.toml / jq / rustfmt 未導入時は何もしない。整形失敗は隠さず hook のエラーとして報告される
