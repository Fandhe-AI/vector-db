# 回答正答率評価基盤（TASK-118）

## 位置づけ

- 対応タスク: `docs/spec/05-tasks.md` の TASK-118（MS-4・基盤・工程管理。詳細な完了条件は
  spec 本文を参照。ポインタ表記のみで本ドキュメントには転記しない）
- 検索結果を LLM のコンテキストとして渡した際の回答正答率を LLM 採点で実測する
  フォローアップ評価。実装タスク群をブロックしない独立タスク
- 本ディレクトリにコミットされているのは**評価基盤（スクリプト・テスト・実行手順）のみ**。
  評価データ・PoC 資産は private spec 側（`docs/spec/03-poc/eval-base/`。ポインタ表記）に
  あり、本体（public リポ）には含めない

## 重要: 正式測定はオーナー承認後

spec 上、採点基準（採点プロンプト）・サンプル数の評価設計は人間担当である。
本基盤に同梱している `config.example.json` の値は**暫定（オーナー承認前）**の配線検証用
サンプルにすぎない。**正式な測定・結果レポートの確定は、オーナーの評価設計承認後に実行する**。

## 構成

| パス | 内容 |
| ---- | ---- |
| `answer_accuracy.py` | 評価スクリプト本体（Python 3 標準ライブラリのみ・外部依存なし） |
| `config.example.json` | 設定例。ダミー値のみ。実設定は `config.json`（git 管理外）に置く |
| `fixtures/sample.jsonl` | 配線検証用の架空データ（本リポで新規作成。spec 由来データではない） |
| `fixtures/scoring_prompt.txt` | 配線検証用の採点プロンプト例 |
| `tests/test_answer_accuracy.py` | ユニットテスト（`unittest`。ネットワーク・実 LLM・spec データ不要） |

## 実行手順

### 1. 配線検証（`--dry-run`。LLM 接続なし）

```bash
python3 scripts/eval/answer_accuracy.py --config scripts/eval/config.example.json --dry-run
```

設定検証・データ読み込み・レポート雛形出力までを LLM 接続なしで確認できる。
出力は `_/reports/`（git 管理外）に書き出される。

### 2. 実測（オーナー承認後）

1. `scripts/eval/config.json`（git 管理外。`.gitignore` 対象）を作成し、
   `config.example.json` を土台に以下を承認済みの値へ差し替える:
   - `llm_endpoint` / `model`: 実際に使う LLM エンドポイント・モデル
   - `data_path`: private spec 側の評価データパス（例: `docs/spec/03-poc/eval-base/...`。
     `docs/spec` submodule の取得が前提。未取得の場合は明示エラーで終了する）
   - `scoring_prompt_path`: 承認済みの採点プロンプトファイル。**採点基準のみを書き、
     出力形式の指示（「CORRECT と返せ」等の固定ラベル指示を含む）は書かないこと**。
     出力形式はスクリプトが実行ごとに生成するランダム判定トークンの指示
     （system メッセージ側）で強制され、採点プロンプト内に出力形式指示を書いても
     トークン指示が優先される（矛盾した指示は判定不能率の上昇を招くだけで、
     固定ラベル出力が正答計上されることはない）
   - `sample_size` / `max_sample_size`: 承認済みのサンプル数（上限は
     `HARD_MAX_SAMPLE_SIZE` でさらに丸められる）
2. LLM の API キーは環境変数（`config.json` の `api_key_env` で指定したキー名）で渡す。
   `config.json` にトークンそのものを書かない
3. 実行する:

   ```bash
   EVAL_LLM_API_KEY=xxxx python3 scripts/eval/answer_accuracy.py --config scripts/eval/config.json
   ```

4. `_/reports/answer_accuracy_<timestamp>.md` に正答率・判定不能率・サンプル別結果が
   出力される。結果レポートの公開可否・取り扱いはオーナーが判断する

## テスト

```bash
python3 -m unittest discover scripts/eval/tests
# または
make test-eval
```

## 設計上の注意（fail-closed）

- 設定の必須キー欠落・型不正・サンプル数上限超過は即エラー終了する
- 評価データのパスが存在しない場合（private submodule 未取得等）は英語のエラー
  メッセージで終了する
- 採点出力は厳格パースする。判定は採点呼び出しごとに生成されるランダム判定トークンとの
  一致のみで行い、トークン不一致（固定ラベル文字列 `CORRECT` 等を含む）・パース不能は
  「判定不能」として不正解側に倒す（正答率の分子に計上しない。候補回答内の埋め込み指示で
  grader に固定文字列を出力させても正答計上には到達できない）
- LLM エンドポイントは設定ファイルで明示指定されたもの以外へ接続しない
  （リダイレクト追従なし・http/https のみ）
- 非 dry-run 実行時、`api_key_env` で指定した環境変数が未設定/空の場合は LLM 呼び出し前に
  明示エラーで終了する（キー未設定を「判定不能率 100%」のレポートとして誤誘導しない）
- レポート書き出し（`output_dir` への書き込み）が失敗した場合も未処理の traceback にせず、
  fail-closed な終了コードで報告する
