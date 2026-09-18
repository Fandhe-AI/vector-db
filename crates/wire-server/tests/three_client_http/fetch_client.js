#!/usr/bin/env node
// Node.js 組み込み `fetch`（無改造。Node >= 18 のグローバル。`require` も
// 外部パッケージも使わない）で NoSQL 表層（`wire-server --surface nosql`）
// へ 1 要求を送るクライアント。
//
// `crates/wire-server/tests/three_client_http_e2e.rs`（Issue #776・#777。
// `#[ignore]`）から子プロセスとして起動される。SQL 表層側の
// `tests/three_client/pg_client.js` と同じ配置・入出力規約（接続情報・
// 要求本文はすべて環境変数経由で渡し、コマンドライン引数・ソース中に
// ダミー値以外の秘密情報を書かない。security.md P0）を HTTP へ踏襲する。
//
// 環境変数:
// - HTTP_HOST / HTTP_PORT / HTTP_TARGET: 接続先
//   （http://<HOST>:<PORT><TARGET>）。
// - HTTP_BODY: 要求本文（JSON 文字列。Content-Type: application/json で
//   送る）。
// - HTTP_BEARER（任意）: 指定時のみ Authorization: Bearer <値> を付与する。
//
// 成功時は 1 行目に HTTP ステータスコード（10 進数字のみ）、2 行目以降に
// 応答本文をそのまま stdout へ出力し、終了コード 0 で終える（fetch は
// 4xx/5xx で例外を投げないため res.status をそのまま「応答を受信できた」
// として扱う。失効後トークン再送で 401 を確認するテストステップに必要な
// 契約）。リダイレクトはサーバーが 3xx を返さない契約のため
// redirect: "error" で fail-closed に倒す。
//
// 転送路・プロトコル障害（接続不能・タイムアウト・応答本文の上限超過・
// UTF-8 デコード不正・必須環境変数の欠落・fetch 非搭載）はいずれも終了
// コード 1 とし、stderr には障害種別のみを書く（要求本文・bearer 値・
// env の値は一切 echo しない）。

const host = process.env.HTTP_HOST;
const port = process.env.HTTP_PORT;
const target = process.env.HTTP_TARGET;
const body = process.env.HTTP_BODY;
const bearer = process.env.HTTP_BEARER;

// 応答本文の上限（three_client_http_e2e.rs::MAX_RESPONSE_BODY_BYTES と
// 同値。untrusted な外部プロセス応答を無制限に読み込まない）。
const MAX_RESPONSE_BODY_BYTES = 2 * 1024 * 1024;
const TIMEOUT_MS = 10000;

async function main() {
  if (!host || !port || !target || body === undefined) {
    process.stderr.write("fetch_client: missing required HTTP_* environment variables\n");
    return 1;
  }

  if (typeof fetch !== "function") {
    process.stderr.write("fetch_client: global fetch is unavailable; Node.js >= 18 is required\n");
    return 1;
  }

  const url = `http://${host}:${port}${target}`;
  const headers = { "Content-Type": "application/json" };
  if (bearer) {
    headers["Authorization"] = `Bearer ${bearer}`;
  }

  let res;
  try {
    res = await fetch(url, {
      method: "POST",
      headers,
      body,
      redirect: "error",
      signal: AbortSignal.timeout(TIMEOUT_MS),
    });
  } catch (e) {
    process.stderr.write(`fetch_client: request failed: ${e && e.name ? e.name : "Error"}\n`);
    return 1;
  }

  let buf;
  try {
    buf = await res.arrayBuffer();
  } catch (e) {
    process.stderr.write(`fetch_client: failed to read response body: ${e && e.name ? e.name : "Error"}\n`);
    return 1;
  }
  if (buf.byteLength > MAX_RESPONSE_BODY_BYTES) {
    process.stderr.write("fetch_client: response body exceeds size limit\n");
    return 1;
  }

  let decoded;
  try {
    decoded = new TextDecoder("utf-8", { fatal: true }).decode(buf);
  } catch (e) {
    process.stderr.write("fetch_client: response body is not valid utf-8\n");
    return 1;
  }

  process.stdout.write(`${res.status}\n${decoded}`);
  return 0;
}

main().then((code) => process.exit(code));
