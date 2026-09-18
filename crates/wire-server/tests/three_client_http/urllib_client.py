#!/usr/bin/env python3
"""Python 標準ライブラリ `urllib.request`（無改造）で NoSQL 表層
（`wire-server --surface nosql`）へ 1 要求を送るクライアント。

`crates/wire-server/tests/three_client_http_e2e.rs`（Issue #776・#777。
`#[ignore]`）から子プロセスとして起動される。SQL 表層側の
`tests/three_client/psycopg_client.py` と同じ配置・入出力規約
（接続情報・要求本文はすべて環境変数経由で渡し、コマンドライン引数・
ソース中にダミー値以外の秘密情報を書かない。security.md P0）を HTTP へ
踏襲する。外部パッケージ（pip）には一切依存しない
（`.claude/rules/dependency-policy.md`）。

環境変数:
- HTTP_HOST / HTTP_PORT / HTTP_TARGET: 接続先（`http://<HOST>:<PORT><TARGET>`）。
- HTTP_BODY: 要求本文（JSON 文字列。`Content-Type: application/json` で送る）。
- HTTP_BEARER（任意）: 指定時のみ `Authorization: Bearer <値>` を付与する。

成功時は 1 行目に HTTP ステータスコード（10 進数字のみ）、2 行目以降に
応答本文をそのまま stdout へ出力し、終了コード 0 で終える（ステータスが
4xx／5xx でも「応答を受信できた」として扱う。`urllib.error.HTTPError` を
捕捉し `e.code`／`e.read()` を採用する。失効後トークン再送で `401` を
確認するテストステップに必要な契約）。

転送路・プロトコル障害（接続不能・タイムアウト・応答本文の上限超過・
UTF-8 デコード不正・必須環境変数の欠落）はいずれも終了コード 1 とし、
stderr には障害種別のみを書く（要求本文・bearer 値・env の値は一切
echo しない。値を含む例外メッセージがあっても種別・reason に限定する）。
"""

import os
import sys
import urllib.error
import urllib.request

# 応答本文の上限（untrusted な外部プロセス応答を無制限に読み込まない
# ための固定上限。`three_client_http_e2e.rs::MAX_RESPONSE_BODY_BYTES` と
# 同値。超過は転送路障害として終了コード 1 にする）。
MAX_RESPONSE_BODY_BYTES = 2 * 1024 * 1024

# 接続・応答待ちのタイムアウト（秒）。ハングしたテストプロセスを残さない
# ための fail-closed な上限。
TIMEOUT_SECONDS = 10


def main() -> int:
    host = os.environ.get("HTTP_HOST")
    port = os.environ.get("HTTP_PORT")
    target = os.environ.get("HTTP_TARGET")
    body = os.environ.get("HTTP_BODY")
    bearer = os.environ.get("HTTP_BEARER")

    if not host or not port or not target or body is None:
        print("urllib_client: missing required HTTP_* environment variables", file=sys.stderr)
        return 1

    url = f"http://{host}:{port}{target}"
    headers = {"Content-Type": "application/json"}
    if bearer:
        headers["Authorization"] = f"Bearer {bearer}"

    req = urllib.request.Request(
        url,
        data=body.encode("utf-8"),
        method="POST",
        headers=headers,
    )

    try:
        try:
            resp = urllib.request.urlopen(req, timeout=TIMEOUT_SECONDS)
        except urllib.error.HTTPError as e:
            # 4xx／5xx は「応答あり」として扱う（curl -w '%{http_code}' と
            # 同じ契約。失効後再送で 401 を検証するステップに必要）。
            resp = e
        status = resp.getcode()
        raw = resp.read(MAX_RESPONSE_BODY_BYTES + 1)
        if len(raw) > MAX_RESPONSE_BODY_BYTES:
            print("urllib_client: response body exceeds size limit", file=sys.stderr)
            return 1
        try:
            decoded = raw.decode("utf-8", errors="strict")
        except UnicodeDecodeError as e:
            print(f"urllib_client: response body is not valid utf-8: {e}", file=sys.stderr)
            return 1
    except urllib.error.URLError as e:
        print(f"urllib_client: request failed: {e.reason}", file=sys.stderr)
        return 1
    except (TimeoutError, OSError) as e:
        print(f"urllib_client: request failed: {type(e).__name__}", file=sys.stderr)
        return 1

    print(status)
    sys.stdout.write(decoded)
    return 0


if __name__ == "__main__":
    sys.exit(main())
