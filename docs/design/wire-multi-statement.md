# 簡易クエリのセミコロン区切り複数文実行

- ステータス: **Accepted（実装既定値）**
- 対応: Issue #938
- ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-16・`docs/spec/05-tasks.md` TASK-219
- 関連ポインタ: WIRE-4（1 メッセージ長上限）・SQL-8（許可リスト構造検証・単一文契約）・
  SQL-31（`BEGIN`/`COMMIT`/`ROLLBACK`・未実装）・RECOVER-12（複数文単位トランザクション・
  未実装）・ERR-1／ERR-2（`wire_code` 契約）

## 背景・目的

`'Q'`（簡易クエリ）本文全体を 1 文として `EngineCore::execute_sql_in_session`
へ渡す既存実装（TASK-73・WIRE-1）は、末尾の 1 個の `;` は許容するが、その後ろに
余剰トークンがあると `sql::allowlist::Parser::expect_end_of_statement` が
「複数文は未対応」として `42601` で拒否していた（SQL-8）。本 Issue は、1 つの
簡易クエリメッセージに含まれるセミコロン区切りの複数文を順に実行し、各文の
応答を順に返した後、`ReadyForQuery` を 1 回だけ返す（WIRE-16）。

## 分割の実装

分割・文種別分類は `crates/engine/src/sql/statement_splitter.rs`
（`split_statements`・`classify_statement`・`check_write_placement`）が担い、
wire 層（`crates/wire-server/src/simple_query.rs`）は SQL の字句知識を持たない
まま呼び出すだけの構成にした（既存の責務境界を維持）。

### 分割規則

- 文字列リテラル（`'...'`。`''` エスケープを含む）の中にある `;` は分割点に
  しない。
- リテラル外で `--`・`/* */`（SQL コメント）・`"`（二重引用符識別子）・
  未終端の文字列リテラルを検出した場合は分割せず元テキスト全体を
  そのまま渡す（`SplitOutcome::Single`）。これらは `lexer::tokenize` が
  常に拒否する構文であるため、分割せずに渡しても既存と同一の `42601` に
  収束する。単体テスト（`statement_splitter.rs::tests::
  comments_double_quotes_and_unterminated_literals_are_single_and_lexer_agrees`）
  で分割器の拒否規則と lexer の拒否規則が連動していることを機械的に固定した。
  この設計により「コメントや未終端引用符で区切りを隠し、後続の文を密輸する」
  経路を構造的に塞いでいる。
- 非空文（前後空白 trim 後）の上限は `MAX_STATEMENTS_PER_QUERY = 16`
  （実装既定値）。超過は `54000`（`PayloadTooLarge`）で 1 文も実行しない。
- 空文（`;;`・先頭の `;`・末尾の余剰 `;`・空白のみの区間）は無視する。
  非空文が 0 個なら `EmptyQueryResponse` を返す。
- 既存の単一文契約（`;` なし、または末尾に 1 個だけの `;`）は
  `SplitOutcome::Single` として元テキストを無加工のまま渡す。これにより
  単一文の応答バイト列・エラーコード・メッセージは構造的に不変のまま保たれる
  （`crates/wire-server/tests/wire16_multi_statement.rs::
  single_statement_behavior_is_unchanged` で固定）。

### 文種別分類と「書き込みは最後の 1 文のみ」の制約

明示 `BEGIN`（SQL-31）と複数文単位のトランザクション機構（RECOVER-12）は
未実装のため、書き込み系文が含まれる複数文メッセージでは「書き込みが最後の
1 文に限られる」形のみを受理する（fail-closed）。

- `StatementEffect::ReadOnly`: `SELECT`（検索・集計・広域取得）・`EXPLAIN`。
- `StatementEffect::SessionLocal`: `SET ...`・`CREATE FUNCTION ...`。
- `StatementEffect::Write`: `INSERT`（UPSERT 含む）・`UPDATE`・`DELETE`・
  `TRUNCATE`、および読み取り専用・セッション局所のいずれとも判定できない
  未知の先頭語（fail-closed の既定。将来 engine に書き込み系構文が追加された
  場合に誤って許可しないための安全側の既定）。
- `StatementEffect::Rejected`: 字句解析に失敗する文、および字句解析には
  成功しても構造上どの `core.rs::execute_sql_in_session` の分岐にも到達し
  得ない先頭トークン形（`Token::Number`・`Token::Punct`・`Token::StringLiteral`・
  `Select` 以外の `Keyword`・`QualifiedIdent` 等）。実行すれば必ず
  `validate_sql` の許可リスト外（`42601`）で拒否され副作用が起きないため、
  位置に関わらず許可する（`check_write_placement` の対象外）。

`check_write_placement` は `Write` が最後の文以外の位置にある場合、または
`Write` が 2 個以上ある場合（後者は必ず一方が最後以外に来るため同じ規則で
拒否される）に `0A000`（`FeatureNotSupported`）で 1 文も実行せずに拒否する。

## 原子性（暗黙トランザクション）の扱い

「書き込みは最後の 1 文のみ」という制約の下では、追加の分散トランザクション
機構なしに WIRE-16 の原子性要件が構造的に成立する。

- 先行文（すべて読み取り専用・セッション局所）がエラーになった場合、書き込み
  文はまだ実行されていない。
- 最後の書き込み文自身がエラーになった場合は、その文の redb トランザクション
  が単独で原子的に失敗する（既存の単一文書き込み経路と同一の commit 境界）。
- 書き込み文の後ろに文はない。

この制約はまた、`_response_boundary`（RECOVER-5 (3)）・緊急応答登録
（RECOVER-6・`crate::recovery::panic_hook`）の「1 メッセージにつき commit は
高々 1 回」という前提を保つ副次的な理由でもある。書き込みが最後の 1 文に
限られることで、複数文メッセージでも commit 成功境界を跨いだ panic の扱いは
既存の単一文契約と同一のまま拡張できる。

### セッション状態の巻き戻し

複数文メッセージの途中でエラーが発生した場合、`SET search_mode`・
`CREATE FUNCTION` によるセッション局所の変更もメッセージ受信前の値へ巻き戻す
（PostgreSQL の暗黙トランザクション内で `SET` が巻き戻るのと同じ意味論）。
`execute_and_respond` が複数文モードへ入る際に `SessionState`（`Clone`
導出済み）のスナップショットを取得しておき、いずれかの文が失敗した時点で
復元する。単一文経路（`SplitOutcome::Single`）はこの clone を行わないため、
既存の単一文レイテンシ・アロケーションコストは不変。

### 制約を緩める条件

SQL-31（`BEGIN`/`COMMIT`/`ROLLBACK`）・RECOVER-12（複数文単位の
トランザクション機構）が実装された時点で、「書き込みは最後の 1 文のみ」の
制約を外す判断を再度行う。それまでは、書き込みを含む複数文の原子性を
安全側（受理範囲を狭める）に倒して保証する。

## 応答順序

各文の応答（`RowDescription`/`DataRow`*/`CommandComplete`、または
`ErrorResponse`）を順に送出し、`ReadyForQuery` は最後の文の応答にのみ付ける
（`crates/wire-server/src/simple_query.rs::Finish` 引数。`ReadyForQuery`／
`Continue` の 2 値）。途中の文がエラーになった場合は、その時点で
`ErrorResponse`＋`ReadyForQuery` を送って打ち切り、残りの文は実行しない
（`respond_error_and_ready` は `finish` に関係なく常に `ReadyForQuery` を
送る——途中エラーで打ち切るため、その時点で応答が確定する）。

`respond_command_complete`／`respond_query_result`／`respond_rows_with_tag`
はいずれも `StatementStatus`（`Completed`／`Failed`）を返し、複数文
オーケストレーション（`execute_and_respond`）がこれを見て次の文へ進むか
セッション状態を巻き戻して打ち切るかを判定する。単一文の応答バイト列
（`Finish::ReadyForQuery` 固定）は分割前と完全に同一。

## RECOVER-5/6 との関係

- `_response_boundary`（RECOVER-5 (3)。commit 成功境界と応答一意性）は
  `'Q'` 本文 1 通全体を覆ったまま変更しない。上記のとおり 1 メッセージに
  つき commit は高々 1 回に限られるため、既存の保証範囲をそのまま維持できる。
- 緊急応答の登録（RECOVER-6・`EmergencyResponseRegistration::register`）は
  文ごとに独立して張り直す（`run_statement` 内。旧 `execute_and_respond`
  本体から抽出）。書き込み文は最後の 1 文に限られるので、実質的に意味を
  持つのは最後の文の登録だけになる。
- 先行文の応答は `ResponseBuffer::push_frame`／`flush`（Issue #481）によって
  完全なフレーム単位でしかソケットに出ない。そのため、最後の文の commit 後に
  panic しても、緊急応答が書きかけのフレームに混入することはない（常に
  フレーム境界の後に続く）。

## スコープ外

- `BEGIN`/`COMMIT`/`ROLLBACK`（SQL-31）・複数文単位のトランザクション機構
  （RECOVER-12）。
- `ReadyForQuery` の状態バイト（WIRE-19。`I` 固定のまま）。
- 拡張クエリプロトコル（WIRE-11）。
- `EngineCore::execute_sql`（非セッション API）・`execute_sql_in_session` の
  engine 側単一文契約。複数文対応は wire の `'Q'` 経路のみに実装し、
  `crates/engine/tests/rls_implicit.rs` の「`...; SELECT 1` → `42601`」という
  engine API 契約は不変のまま残す。
- HTTP／NoSQL 表層。SQL テキストを受けず束縛済み計画で動くため WIRE-16 の
  対象外であり、HTTP への射影変更もない。
- 3 クライアント e2e（`three_client_e2e.rs` 等）への追加は opt-in の任意
  追加に留め、本 Issue では必須にしない。

## 挙動変化の明記（受理範囲の拡大）

以前は `42601` だった一部の入力が、本変更で受理側へ変わる（空文を無視する
帰結・単一文契約の外側を複数文経路が拾う帰結）。

- `SELECT 1;;`（末尾の余剰 `;`）・`;SELECT 1`（先頭の `;`）・`;`・`; ;`
  （非空文 0 個。`EmptyQueryResponse` を返す）。
- セミコロン区切りの複数文そのもの（本 Issue の主目的）。

これらはいずれも受理範囲の拡大であり、RLS・fail-closed・テナント境界の
契約を緩めるものではない（各文は独立に既存の許可リスト検証・RLS 暗黙適用を
通る）。

## 検証

- `crates/engine/src/sql/statement_splitter.rs`（単体テスト）: 分割規則・
  文種別分類・書き込み配置検査・上限。
- `crates/wire-server/tests/wire16_multi_statement.rs`（層 A 結合テスト）:
  応答順序・エラー時の打ち切り・セッション状態の巻き戻し・RLS 不変
  （RLS-9/10 の応答同一性を含む）・単一文の既存挙動の不変性。
- 回帰: `crates/engine/tests/rls_implicit.rs`（engine API の単一文 `42601`
  契約）・既存 wire 結合テスト一式（`wire1_simple_query.rs` 等）・
  `wire_fault_injection_cli.rs`（commit 後 panic の緊急応答経路が文単位の
  登録でも成立すること）。
