# wgpu 導入と GPU バッチ経路の有効化

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- **依存追加はオーナーの明示承認を得ていない。マージ前に承認が必要**
  （`.claude/rules/dependency-policy.md`。承認事項は
  `crates/engine/Cargo.toml` のコメント・本文 §1 参照）
- 対応: TASK-128〜130（Issue #178。ポインタ: `docs/spec/05-tasks.md`）
- 関連ビヘイビア: CORE-6, CORE-8, CORE-16, EXT-2（ポインタ:
  `docs/spec/04-behavior/core-engine.md`・`extensions.md`）
- 前提: TASK-129（`batch_fallback.rs`。PR #148/#152/#154 でマージ済み。GPU 経路を
  CPU 上の参照実装 `GpuReferenceBackend` で実装していた）

## 1. 依存追加の承認事項

| 項目 | 内容 |
| ---- | ---- |
| クレート | `wgpu = { version = "=30.0.1", default-features = false, features = ["std", "vulkan", "metal", "dx12", "wgsl"] }`（`crates/engine/Cargo.toml`） |
| 目的 | Vulkan/Metal/DX12 の compute を単一 API で扱うため。各 GPU API を自作 FFI で叩くと依存最小方針に反して肥大化する |
| ライセンス | MIT OR Apache-2.0（本リポと同一） |
| メンテ状況 | gfx-rs/wgpu。crates.io の 30.0.1 系。`rust-version = 1.87.0`（本リポ toolchain は stable。充足） |
| 推移的依存 | 上記 feature 構成で 102 crate（`Cargo.lock` 反映済み）。主要: `wgpu-core`/`wgpu-hal`/`wgpu-types`/`naga`/`ash`/`gpu-allocator`/`libloading`/`bitflags`/`thiserror`/`hashbrown`/`indexmap`/`smallvec`/`log`/`bytemuck`（`gpu-allocator` 経由の推移的依存。本体は `bytemuck` を使わず `to_ne_bytes`/`from_ne_bytes` でバイト変換する） |
| cargo-deny | `make deny` で `advisories ok, bans ok, licenses ok, sources ok`（warn: `hashbrown`/`syn` の重複バージョンのみ。`multiple-versions = "warn"` 方針どおり） |
| ビルド | `cargo check`／`cargo clippy --all-targets -- -D warnings` green。Vulkan ローダは `libloading` による実行時動的ロードのため、リンク時要件・CI runner への追加パッケージは不要 |

feature はゲート化しない（CORE-12: 経路を外部から上書きする機構を設けない方針。GPU
の可否は実行時の初期化結果のみで決まる）。

## 2. 設計方針

### 2.1 責務配置

```text
FallbackBatchEngine（batch_fallback.rs・CORE-8）
  ├─ primary: Box<dyn BatchBackend>  ← gpu_batch.rs::GpuBatchBackend
  │     ├─ 初期化失敗 → PrimarySlot::Unavailable（既存。FallbackEvent{Init}）
  │     ├─ 実行時エラー → runtime_latched + CPU 縮退（既存。FallbackEvent{Runtime}）
  │     └─ Ok(hits) → revalidate_primary_hits（既存 P0 防御。GPU 出力を信頼しない）
  └─ cpu: CpuFallbackMatrix（f32 原本）→ run_batch_search（既存・CORE-3 同一カーネル）
```

`FallbackBatchEngine::build_with_gpu`（新設）が `GpuBatchBackend::try_new` を
`backend_factory` として渡す。`GpuReferenceBackend`（CPU 参照実装）はテスト・
ベンチの配線疎通用に残し、`build_with_gpu_reference` も互換維持する。

### 2.2 GPU バックエンド（`gpu_batch.rs`）の契約

- プロセス共有の `GpuContext`（`Instance`/`Adapter`/`Device`/`Queue`/
  `ComputePipeline`）を `OnceLock` で 1 回だけ初期化する（初期化 `Err` も
  キャッシュし、以降の `try_new` は即 `InitFailed`）
- `InstanceDescriptor::new_without_display_handle()` を使い、`WGPU_*` 環境変数を
  読む `from_env` 系は使わない（CORE-12 と整合）
- `request_adapter(HighPerformance, force_fallback_adapter=false)`。
  `AdapterInfo.device_type == Cpu`（lavapipe 等）は `InitFailed` 扱い（「GPU
  経路」の capability として偽陽性にしない）
- `request_device` の `required_limits` は adapter の実測 limits をそのまま
  要求する。常駐行列（`packed()`）のバイト数が
  `max_storage_buffer_binding_size` を超える場合は `InitFailed`（CPU 縮退へ）
- `device.on_uncaptured_error` 相当の防御として `push_error_scope`/
  `pop_error_scope`（LIFO）で Validation/OutOfMemory を捕捉し、
  `set_device_lost_callback` でデバイスロストをラッチする
- WGSL は `const` 文字列で埋め込み（外部ファイル読み込みなし）。
  `unpack2x16float`（コア機能。`shader-f16` 拡張不要）で
  `batch_search.rs::pack_f16x2` と同じビット解釈の f16 → f32 復元を行い、
  積和は f32 で行う
- テナント境界・可視性判定は `policy.rs::PolicyContext::is_visible` の単一
  照合パスを CPU 側で使う（`gather_reachable_rows`）。GPU では積和のみを行い、
  テナント境界判定を GPU に持ち込まない
- 計算量 DoS 対策は `batch_search.rs::MAX_BATCH_WORK` と同じ定数を dispatch
  前に適用する（§3 の設計方針縮小により、`run_batch_search` のテナント別
  精緻化ではなく `rows × queries × dim` の粗い見積もりを使う。安全側）
- 非同期 API（`request_adapter`/`request_device`/`map_async`/
  `pop_error_scope`）は std-only の `block_on`（`Waker::noop()` +
  ポーリングループ）で同期化する（`pollster` 依存を追加しない）
- `unsafe`・`bytemuck` は使わない。バイト列変換は `to_ne_bytes`/
  `from_ne_bytes` のみ。バッファサイズは `checked_*`/`saturating_*` で導出する

### 2.3 スコープ縮小事項（当初計画からの差分）

実装時間の制約により、以下は当初計画（`BatchPlan` によるテナント別グループ
dispatch・チャンク分割・ベンチ A/B 実測配線）から縮小した。挙動の正しさ・
安全性には影響しないが、性能特性・網羅性が計画より狭い:

- `batch_search.rs::run_batch_search` の内部（テナント別精緻化された work
  budget・行外側ループ）は変更していない。`GpuBatchBackend` は独自に
  `gather_reachable_rows`（クエリ単位で `PolicyContext::is_visible` を
  1 行ずつ判定）を実装しており、共通化（`BatchPlan`/
  `push_visible_candidate`/`finalize_selection` の抽出）は行っていない
- dispatch はクエリ単位（バッチ内の複数クエリを 1 回の dispatch にまとめる
  最適化は行わない）。行数が `GPU_SCORE_BUFFER_BUDGET_BYTES`
  （32 MiB）を超える場合はクエリ内で行チャンクへ分割する
- `benches/batch_bench.rs`（TASK-130 の CORE-6/16 opt-in ゲート）は実 GPU
  経路への実測配線を行っていない。バックエンド自体は接続済みのため、後続の
  作業で `FallbackBatchEngine::build_with_gpu` を使った A/B 計測へ配線できる
- GPU バッファ（スコア/リードバック/クエリ）の呼び出し間再利用（CORE-15 の
  プール方針を GPU ステージングへ拡張）は行っておらず、呼び出しごとに
  確保・解放する
- `VECTOR_DB_TEST_REQUIRE_GPU` strict モード・`make test-gpu` ターゲットは
  未実装。結合テスト（`tests/gpu_batch.rs`）は GPU の有無を実行時に判定し、
  利用不能な環境では該当分岐を `eprintln!` で報告して早期 return する

## 3. ローカル実測（開発環境）

開発コンテナ内で GPU（NVIDIA、Vulkan backend）が実際に利用可能であることを
確認し、以下を実測した（値そのものは性能基準ではないため、pass/fail の記録
のみ。spec 由来の閾値は転記しない）:

- `GpuBatchBackend::try_new` → adapter/device 初期化成功（`DeviceType` が
  `Cpu` でないことを確認）
- 手計算した内積（4 次元・複数行）との一致を GPU dispatch 経由で確認
  （`crates/engine/src/gpu_batch.rs` の `gpu_batch_search_matches_hand_computed_dot_products_when_available`）
- 複数テナント混在バッチでのテナント混入 0 件・奇数次元でのパディング
  正しさ・CPU オラクル（`kernel.rs::CpuScalarProvider`）とのスコア一致
  （相対誤差 1e-3 以内）を確認
  （`crates/engine/tests/gpu_batch.rs`）
- GitHub ホステッド runner（CI）には GPU が無いため、CI では常に
  「初期化失敗 → CPU-SIMD 縮退」分岐のみが実走する。開発環境（GPU あり）で
  両分岐（成功・初期化失敗）を実際に確認する仕組みは持たない
  （`GpuBatchBackend::try_new` を強制的に失敗させる注入経路を作ると
  CORE-12 の「経路を外部から上書きする機構を設けない」方針に反するため）。
  初期化失敗分岐は `tests/batch_fallback.rs`（モック `BatchBackend`
  実装によるエラー注入）が別途カバーする

## 4. セキュリティ考慮（OWASP Top 10 + AGENTS.md P0）

| 観点 | 対応 |
| ---- | ---- |
| A03 インジェクション | WGSL は `const` 埋め込み。外部文字列からシェーダ・パラメータを組み立てない |
| A01 アクセス制御（テナント境界 P0） | GPU は積和のみ。可視性判定は CPU 側の `PolicyContext::is_visible` 単一照合パス。`FallbackBatchEngine::revalidate_primary_hits` が GPU 出力を独立再検証する（既存 P0 防御を維持） |
| A04 不安全な設計 / DoS | バッファサイズは `checked_*`・定数予算（`GPU_SCORE_BUFFER_BYTES`）で制限。計算量は `MAX_BATCH_WORK` を dispatch 前に適用 |
| fail-closed / panic 禁止 | `unwrap`/`expect`/添字アクセスは使わない。wgpu のエラー・デバイスロスト・ポーリング失敗はすべて `Result` で伝播する |
| 情報漏えい | エラー/イベント文字列にテナント ID・クエリ・adapter 製品名を含めない |
| A05 設定ミス / CORE-12 | GPU 経路を強制・無効化する環境変数・feature flag を `src/` に設けない（`InstanceDescriptor::new_without_display_handle()`） |
| A06/A08 依存・サプライチェーン | `=30.0.1` 完全固定、`Cargo.lock` コミット、`cargo deny` green。**オーナー承認は未取得。マージ条件とする** |
| `unsafe`（P1） | 追加しない。`tests/isa.rs` の走査テストで機械的に担保（`unsafe` トークンが `gpu_batch.rs` に存在しないことを含む） |
