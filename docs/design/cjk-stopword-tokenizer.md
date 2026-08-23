# ADR: BM25 トークナイザの CJK ストップワード除去

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-105（対象ビヘイビア: SEARCH-5）
- 前提: TASK-102（`crates/engine/src/sparse.rs` の BM25 疎検索カーネル。PR #138 でマージ済み）
- 関連: TASK-103・TASK-104・TASK-106

## 背景

TASK-102 で実装した `tokenize()` は CJK（ひらがな・カタカナ・CJK 統合漢字）を文字
ユニグラム＋隣接バイグラムへ分割するのみで、助詞・記号のストップワード除去を持た
なかった。CJK トークナイザの構成差は検索品質（direct Recall@20）に影響しうるため、
TASK-105 は「CJK コンテンツ（内容語）を保持したまま」助詞・記号の単独トークン化を
抑制する除去を実装し、除去有無の Recall@20 変動を測定・記録することを求める。

## 決定事項

1. **記号の除去（文字レベル）**: 中黒 `・`（U+30FB）を `is_cjk_char` のレンジから
   除外し、単語区切り（非トークン文字）として扱う。長音符 `ー`（U+30FC）はカタカナ語
   （例: 「サーバー」）の構成要素のため CJK 扱いのまま保持する。
2. **助詞ユニグラムの除去（トークンレベル）**: ユニグラムが助詞・形式的機能語の
   1 文字（`は`・`が`・`を`・`に`・`で`・`と`・`も`・`の`・`へ`・`や`）に一致する場合、
   そのユニグラムを出力しない。除去対象は静的な定数配列（`CJK_STOPWORD_UNIGRAMS`）
   で保持し、外部クレート・実行時設定注入には依存しない（依存最小方針）。
3. **CJK コンテンツの保持**: バイグラムは除去しない。助詞文字が内容語の内部に現れる
   ケース（例: 「もの」の「の」）を壊さないよう、除去はユニグラムの単独トークン化の
   抑制に限定する。
4. **API 設計**: `tokenize(text)` の既定動作を除去 ON に変更する。除去有無を比較
   測定するため `tokenize_with_options(text, remove_stopwords: bool)` を追加し、
   `tokenize()` はその薄いラッパとする。`SparseIndex::build`/`with_params`/`search`
   は既存どおり `tokenize()` のみを使うため、`SparseIndex` の公開 API・index/query 間
   のトークナイザ対称性は変更しない。

## 測定・記録（本リポジトリ内スコープ）

数万チャンク規模・日本語主体コーパスでの実測は TASK-106（並行フォローアップ）の
管轄であり本タスクのスコープ外とする。本タスクでは
`crates/engine/tests/sparse_stopwords.rs` に合成日本語ミニコーパス（30 件・正解 3 件）
を定義し、`tokenize_with_options` から独立に再計算した Okapi BM25 で除去 ON/OFF の
direct Recall@20 を比較した。

- `on_recall`（除去 ON）= 1.0（3/3 件が Top-20 に入る）
- `off_recall`（除去 OFF）= 1.0（3/3 件が Top-20 に入る）

この規模の合成コーパスでは、ノイズ文書（助詞は多用するが内容語を含まない文書）の
助詞一致によるスコアが、内容語一致文書のスコアを上回るほど蓄積しないため、除去
OFF でも Recall@20 は低下しなかった。ただし除去 ON はノイズ文書がスコア > 0 になる
件数を OFF 以下に抑える（`stopword_removal_does_not_increase_noise_document_matches`
で固定）。除去の効果がより明確に現れる規模（数万チャンク）での実測は TASK-106 に
引き継ぐ。spec 側 PoC の実測値は本ドキュメントへ転記しない。

## 影響

- `crates/engine/src/sparse.rs`: `is_cjk_char` の中黒除外・`CJK_STOPWORD_UNIGRAMS`・
  `tokenize_with_options` 追加・`tokenize` の既定除去 ON 化
- `crates/engine/tests/sparse.rs`（既存 TASK-102/SEARCH-1,3 テスト）: 無変更で
  通ることを確認済み（トークナイザ既定変更のリグレッションなし）
- `crates/engine/tests/sparse_stopwords.rs`（新規）: SEARCH-5 対応テスト

## スコープ外

- 数万チャンク規模・日本語主体コーパスでの実測（TASK-106）
- RRF 融合（TASK-103）・評価ハーネス回帰テスト化（TASK-104）
- 2 文字以上の複合助詞（「から」「まで」等）のトークンレベル除去。バイグラム除去は
  「CJK コンテンツ保持」と衝突しうるため初期実装では 1 文字ユニグラムに限定する

## 参照

- `docs/spec/05-tasks.md`（TASK-105・TASK-102・TASK-103・TASK-104・TASK-106）
- `docs/spec/04-behavior/search.md`（SEARCH-5）
