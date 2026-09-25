#!/usr/bin/env node
// node `pg`（無改造）で明示トランザクションの状態遷移（WIRE-19・Issue #943）を
// 観測するクライアント。
//
// `crates/wire-server/tests/three_client_e2e.rs`（`#[ignore]`）から子プロセス
// として起動される。node `pg` は psycopg のような公開の
// `transaction_status` API を持たないため、`pg.Client` が内部で保持する
// `Connection`（EventEmitter。`pg_client.js` と同じく変更を加えない既存の
// public プロパティを読むだけで、`pg` のソース自体は一切書き換えない）が
// 発行する `readyForQuery` イベント（`pg-protocol` の
// `ReadyForQueryMessage.status`）を購読して、受信した `ReadyForQuery` の
// 状態バイト（`'I'`/`'T'`/`'E'`）をそのまま stdout へ 1 行ずつ出力する
// （`docs/design/three-client-e2e-harness.md`「トランザクション状態遷移
// （Issue #943・WIRE-19）」節参照）。
//
// SQL の発行はクエリシーケンス（`BEGIN`/`INSERT`/`SELECT`/`COMMIT`/`BEGIN`/
// 構文エラー/`ROLLBACK`）を `client.query()` で明示的に順に送る
// （psycopg 版と異なり、pg は接続の autocommit/no-autocommit を自動切替
// しないため、`BEGIN`/`COMMIT`/`ROLLBACK` を明示的に送る。observation
// 対象はあくまで受信する `ReadyForQuery` の状態バイトであり、送信側の
// 挙動は無改造クライアントの標準的な使い方の範囲に留める）。
//
// 環境変数（すべて必須。ダミー資格情報以外の秘密情報を書かない）:
// - WIRE_HOST / WIRE_PORT / WIRE_USER / WIRE_PASSWORD: 接続情報。
// - WIRE_TXN_INSERT_SQL: `BEGIN` の後に実行する単一行 `INSERT`。
// - WIRE_TXN_SELECT_SQL: 同一トランザクション内で実行する別の・未書き込みの
//   テーブルへの `SELECT`。
// - WIRE_TXN_VERIFY_SQL: `COMMIT` 後に読み戻しを確認する `SELECT`
//   （`INSERT` と同じテーブル）。
// - WIRE_TXN_BAD_SQL: 2 周目で失敗させるための構文エラー文。
//
// 成功時は終了コード 0、失敗時は理由を stderr へ出力し終了コード 1
// （silent skip はしない）。

const host = process.env.WIRE_HOST;
const port = process.env.WIRE_PORT;
const user = process.env.WIRE_USER;
const password = process.env.WIRE_PASSWORD;
const insertSql = process.env.WIRE_TXN_INSERT_SQL;
const selectSql = process.env.WIRE_TXN_SELECT_SQL;
const verifySql = process.env.WIRE_TXN_VERIFY_SQL;
const badSql = process.env.WIRE_TXN_BAD_SQL;

if (
  !host ||
  !port ||
  !user ||
  password === undefined ||
  !insertSql ||
  !selectSql ||
  !verifySql ||
  !badSql
) {
  process.stderr.write("pg_txn_status: missing required WIRE_* environment variables\n");
  process.exit(1);
}

let pg;
try {
  pg = require("pg");
} catch (e) {
  process.stderr.write(`pg_txn_status: pg module is not installed: ${e}\n`);
  process.exit(1);
}

// `pg-protocol` の `ReadyForQueryMessage.status` は単一文字の文字列
// （'I'/'T'/'E'）としてそのまま渡ってくる（数値コードではない）。
const KNOWN_STATUSES = new Set(["I", "T", "E"]);

function statusName(status) {
  return KNOWN_STATUSES.has(status) ? status : `OTHER(${status})`;
}

const client = new pg.Client({
  host,
  port: Number(port),
  user,
  password,
  database: "irrelevant-db-name",
  connectionTimeoutMillis: 5000,
});

client.on("error", (e) => {
  process.stderr.write(`pg_txn_status: connection error: ${e}\n`);
});

// `client.connection` は `pg.Client` が内部で保持する `Connection`
// （wire フレーミングを扱う EventEmitter）で、`readyForQuery` イベントは
// 受信した `ReadyForQuery`（'Z'）メッセージそのものを渡す。ここでは
// リスナー登録のみを行い、`pg`/`pg-protocol` のソースには一切手を加えない。
const statuses = [];
client.connection.on("readyForQuery", (msg) => {
  statuses.push(statusName(msg.status));
});

async function main() {
  await client.connect();
  await client.query("BEGIN");
  await client.query(insertSql);
  await client.query(selectSql);
  await client.query("COMMIT");

  const verify = await client.query(verifySql);
  if (verify.rows.length === 0) {
    throw new Error("expected at least one row after commit");
  }

  await client.query("BEGIN");
  let badSqlFailed = false;
  try {
    await client.query(badSql);
  } catch (e) {
    badSqlFailed = true;
  }
  if (!badSqlFailed) {
    throw new Error("expected WIRE_TXN_BAD_SQL to fail");
  }
  await client.query("ROLLBACK");

  await client.end();
}

main()
  .then(() => {
    for (const s of statuses) {
      process.stdout.write(`${s}\n`);
    }
    process.exit(0);
  })
  .catch((err) => {
    let suffix = err && err.code ? ` [SQLSTATE=${err.code}]` : "";
    process.stderr.write(`pg_txn_status: failed${suffix}: ${err}\n`);
    process.exit(1);
  });
