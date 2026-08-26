#!/usr/bin/env node
// node `pg`（無改造）で wire-server へ簡易クエリを 1 文送るクライアント。
//
// `crates/wire-server/tests/three_client_e2e.rs`（TASK-73・WIRE-1、`#[ignore]`）から
// 子プロセスとして起動される。接続情報・SQL は環境変数で受け取り（コマンドライン
// 引数・ソース中にダミー資格情報以外の秘密情報を書かない。security.md P0）、
// `client.query(text)`（values を渡さない = 簡易クエリプロトコルで送信される）を
// 使う。
//
// 環境変数: WIRE_HOST / WIRE_PORT / WIRE_USER / WIRE_PASSWORD / WIRE_SQL。
// 成功時は結果セットの各行を `|` 区切りで結合した文字列を改行区切りで stdout へ
// 出力し終了コード 0（複数列を返す SQL でも列構成・型変換を検証できるよう全列を
// 出力する。`crates/wire-server/tests/three_client_e2e.rs` の `run_psql`／
// `run_psycopg` と同じ区切り規約）。
// 失敗時はエラーを stderr へ出力し終了コード 1（silent skip はしない）。

const host = process.env.WIRE_HOST;
const port = process.env.WIRE_PORT;
const user = process.env.WIRE_USER;
const password = process.env.WIRE_PASSWORD;
const sql = process.env.WIRE_SQL;

if (!host || !port || !user || password === undefined || !sql) {
  process.stderr.write("pg_client: missing required WIRE_* environment variables\n");
  process.exit(1);
}

let pg;
try {
  pg = require("pg");
} catch (e) {
  process.stderr.write(`pg_client: pg module is not installed: ${e}\n`);
  process.exit(1);
}

const client = new pg.Client({
  host,
  port: Number(port),
  user,
  password,
  database: "irrelevant-db-name",
  connectionTimeoutMillis: 5000,
});

client
  .connect()
  .then(() => client.query(sql))
  .then((result) => {
    for (const row of result.rows) {
      process.stdout.write(`${Object.values(row).join("|")}\n`);
    }
    return client.end();
  })
  .then(() => process.exit(0))
  .catch((err) => {
    process.stderr.write(`pg_client: query failed: ${err}\n`);
    process.exit(1);
  });
