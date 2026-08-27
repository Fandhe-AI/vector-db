# ADR: 辞書的情報源の必須／補助区分と世代整合キャッシュによる連動

- ステータス: Accepted（TASK-109 で実装済み）
- 対応: TASK-109（MS-4・対象ビヘイビア: PLAN-5）
- 反映先（実装・テスト）: `crates/engine/src/dictionary.rs`・
  `crates/engine/src/core.rs::DictionaryCache`・
  `crates/engine/tests/dictionary.rs`
- 関連: TASK-120（増分インデックス反映）・TASK-169（`PrefilterCache` の世代整合
  キャッシュパターン）・TASK-110（後続。LLM クエリプランニング本体・本 ADR の対象外）

## 背景

LLM クエリプランニング（TASK-110 以降）が固定接頭辞コンテキストとして使う
「辞書的情報源」を、DB に索引化済みのコーパスから機械抽出するモジュールを実装する。
必須・補助の情報源区分は README.md「実装方針（要点）」・PLAN-5（対応ビヘイビア。
詳細は private spec 側）で公開済みの範囲に従う。

## 決定事項

### 1. シンボル辞書は必須実装、ファイルツリー・用語索引は補助情報源

`DictionaryConfig` にはファイルツリー（`enable_file_tree`）・用語索引
（`enable_term_index`）の無効化フラグのみを持たせ、シンボル辞書には無効化
スイッチを設けない。新しい情報源の追加は `DictionarySourceKind` へのバリアント
追加＋対応する抽出関数 1 つの追加で完結する構造とする（段階的追加可能な設計）。

### 2. 依存を追加せず手書きの行パーサ・軽量トークナイザで実装する

dependency-policy（依存最小・ユーザー承認制）に従い、regex 等の新規クレートを
追加しない。シンボル抽出は行頭定義（`fn`/`struct`/`enum`/`trait`/`impl`/`mod`/
`const`/`type`）を空白区切りトークン走査で検出する手書きパーサとし、用語索引の
トークナイザは `sparse.rs`（BM25 用・CJK バイグラム込み）とは責務が異なるため
共有せず `dictionary.rs` 内に閉じる。

この設計はコメント・文字列リテラル内の同形テキストを特別扱いしない（例:
`// fn foo()` は行頭トークンが `//` のため誤検出しないが、ブロックコメント・
文字列リテラル内に単独で定義行と同形のテキストが現れる稀なケースは誤検出しうる）。
辞書は LLM への補助コンテキストであり、過検出は recall 側の安全な劣化に留まり
認可・可視性判定には関与しない。

### 3. 増分インデックスとの連動は世代整合キャッシュ（post-commit フック不使用）

`core::DictionaryCache` は `core::PrefilterCache`（TASK-169）と同一の失効規約
（`(table, ctx)` キー・`storage.current_generation()` との不一致で破棄・
容量超過は LRU 追い出し・ロック毒化時はキャッシュを諦め非キャッシュ経路へ縮退）
を踏襲する。ファイル形 `INSERT`（単発・バッチとも）は
`tenant::replace_typed_rows_by_text_key` が世代を bump するため、次回
`EngineCore::dictionary_snapshot` 呼び出し時に自動的に再構築され、増分インデックス
の結果が反映される。

post-commit フックを持たない構成にしたのは以下の理由による。

- バッチ投入の途中失敗時に辞書側だけ不整合な部分更新が残る経路を構造的に排除できる
- プロセス再起動時に辞書キャッシュが消失しても、次回参照時に `redb` から自己回復し、
  永続化すべき追加状態を持たない
- `PrefilterCache` と同一パターンのため、レビュー・保守コストが小さい

トレードオフとして、`storage.current_generation()` はテーブル・書き込み種別を
問わず任意の write commit で単調増加するため、本キャッシュはこのテーブル自身への
書き込みだけでなく無関係な他テーブルへの書き込みでも保守的に失効する。テーブル
単位の精密な失効は持たない。これは意図的な単純化であり、誤って古い辞書を返す
経路（fail-open）よりも安全側（過剰な再構築）に倒す設計判断である。

### 4. `path`/`body` 列を持たないテーブルは固定英語メッセージで拒否する

新規 `CoreError` variant は追加せず、既存の `CatalogError::Invalid` を用いる
（`wire_code` 写像・`wire-server` 側の網羅的 match への影響を避ける）。エラー
メッセージは固定の英語文言とし、他テナントのデータ・存在情報を含めない。

## 影響

- `crates/engine/src/dictionary.rs`（新規）: 抽出層（純関数 API・storage 非結線）
- `crates/engine/src/core.rs`: `DictionaryCache`・`EngineCore::dictionary_snapshot`・
  `EngineCore::with_dictionary_config` を追加（`VectorCore` trait へは昇格しない
  固有 API のため `core-api-check` の対象外。`lib.rs` の `pub mod dictionary;`
  追加のみが `core_api.snapshot` の差分になる）
- `crates/engine/tests/dictionary.rs`（新規）: ファイル形 `INSERT`（単発・バッチ）
  からの反映・同一パス再送での世代失効・テナント境界非漏えい・再起動後の再構築・
  キャッシュヒットを検証

## スコープ外

- Ollama 常駐プロセス連携・クエリ展開（TASK-110）、ソフトブースト（TASK-111）、
  Recall 受け入れ検証（TASK-112〜113）は後続タスク
- 辞書のパス単位差分更新（再構築の最適化）・LLM プロンプトへの整形（レンダリング
  形式）は TASK-110 側で必要になった時点で拡張する
- 行走査で得たチャンク本文をパスの原本本文へ結合せずチャンク単位のまま抽出する
  ため、シンボルの行番号はチャンク相対値になる（チャンク化は行分割ベース・
  非オーバーラップのためシンボル自体の欠落は生じない）。Markdown の文字数分割で
  節が跨る場合の用語頻度の微差も同様に許容する（いずれも `dictionary.rs` モジュール
  ドキュメント参照）

## 参照

- `docs/spec/05-tasks.md`（TASK-109・TASK-110〜113・TASK-120・TASK-169）
- `docs/spec/04-behavior/query-planning.md`（PLAN-5）
- `docs/design/resend-semantics.md`（増分インデックスの置換セマンティクス）
