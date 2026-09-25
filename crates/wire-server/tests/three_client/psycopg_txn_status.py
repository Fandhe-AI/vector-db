#!/usr/bin/env python3
"""psycopg（無改造）で明示トランザクションの状態遷移（WIRE-19・Issue #943）を
観測するクライアント。

`crates/wire-server/tests/three_client_e2e.rs`（`#[ignore]`）から子プロセスと
して起動される。`psycopg_client.py` と異なり、本スクリプトは意図的に
`autocommit=False` で接続する ―― psycopg 3 自身が（受信した `ReadyForQuery`
の状態バイトから）接続の transaction status を追跡し、`IDLE` のときに限り
最初の文の送信前に暗黙の `BEGIN` を自動送出する（libpq に autocommit の
概念はなく、この判断・送出は psycopg 自身が行う。
`docs/design/three-client-e2e-harness.md`「トランザクション状態遷移（Issue #943・
WIRE-19）」節参照）。もし wire-server が `ReadyForQuery` の状態バイトを常に
`'I'` のまま返す不具合があれば、2 文目の前にも `BEGIN` が再送されて
`engine::sql::transaction` の「入れ子の BEGIN」（`25001`）が観測されるはずであり、
これは本スクリプトが検出する不具合クラスの 1 つである。

各段の後、psycopg 3 の `conn.info.transaction_status`
（`psycopg.pq.TransactionStatus` の `IDLE`/`INTRANS`/`INERROR`。
値そのものは接続情報の一部でありクライアント内部状態を持たない）を
1 行ずつ stdout へ出力する（`IDLE`／`INTRANS`／`INERROR` の名前のみ。
SQL 本文・資格情報は出力しない。security.md P0）。

環境変数（すべて必須。ダミー資格情報以外の秘密情報を書かない）:
- WIRE_HOST / WIRE_PORT / WIRE_USER / WIRE_PASSWORD: 接続情報。
- WIRE_TXN_INSERT_SQL: `BEGIN` 直後に実行する単一行 `INSERT`
  （`USING OPERATION_ID` を含む一意な文字列。呼び出し側が一意性を保証する）。
- WIRE_TXN_SELECT_SQL: `INSERT` と同一トランザクション内で実行する
  **別の・未書き込みの** テーブルへの `SELECT`（engine 側の制約により、
  同一トランザクション内で直前に書き込んだテーブル自身は読めない
  ―― `docs/design/explicit-transaction.md` 参照。この制約検証は層 A が
  既に担うため、本スクリプトでは単に別テーブルを使う）。
- WIRE_TXN_VERIFY_SQL: `COMMIT` 後に `INSERT` した行が読み戻せる
  （read-your-writes）ことを確認する `SELECT`（`INSERT` と同じテーブル）。
- WIRE_TXN_BAD_SQL: 2 周目で `INERROR` へ遷移させるための構文エラー文
  （`WIRE_TXN_SELECT_SQL` と同じ未書き込みテーブルを参照する想定）。

成功時は終了コード 0、失敗時は理由を stderr へ出力し終了コード 1（silent skip
はしない）。
"""

import os
import sys


def main() -> int:
    host = os.environ.get("WIRE_HOST")
    port = os.environ.get("WIRE_PORT")
    user = os.environ.get("WIRE_USER")
    password = os.environ.get("WIRE_PASSWORD")
    insert_sql = os.environ.get("WIRE_TXN_INSERT_SQL")
    select_sql = os.environ.get("WIRE_TXN_SELECT_SQL")
    verify_sql = os.environ.get("WIRE_TXN_VERIFY_SQL")
    bad_sql = os.environ.get("WIRE_TXN_BAD_SQL")
    if not all(
        [
            host,
            port,
            user,
            password is not None,
            insert_sql,
            select_sql,
            verify_sql,
            bad_sql,
        ]
    ):
        print(
            "psycopg_txn_status: missing required WIRE_* environment variables",
            file=sys.stderr,
        )
        return 1

    try:
        import psycopg
        from psycopg.pq import TransactionStatus
    except ImportError as e:
        print(f"psycopg_txn_status: psycopg is not installed: {e}", file=sys.stderr)
        return 1

    def status_name(status: int) -> str:
        return {
            TransactionStatus.IDLE: "IDLE",
            TransactionStatus.INTRANS: "INTRANS",
            TransactionStatus.INERROR: "INERROR",
        }.get(status, f"OTHER({status})")

    try:
        # `autocommit=False`（既定）: psycopg 自身が transaction status を
        # 追跡し、`IDLE` のときだけ次の文の前に暗黙の `BEGIN` を送る。
        with psycopg.connect(
            host=host,
            port=int(port),
            user=user,
            password=password,
            dbname="irrelevant-db-name",
            connect_timeout=5,
        ) as conn:
            with psycopg.ClientCursor(conn) as cur:
                print(status_name(conn.info.transaction_status))

                # 暗黙の BEGIN + INSERT。
                cur.execute(insert_sql)
                print(status_name(conn.info.transaction_status))

                # 同一トランザクション内で未書き込みテーブルの SELECT。
                cur.execute(select_sql)
                cur.fetchall()
                print(status_name(conn.info.transaction_status))

                conn.commit()
                print(status_name(conn.info.transaction_status))

                # commit 済みの行が読み戻せる（read-your-writes）ことも
                # あわせて確認する。この確認自体が新たな暗黙トランザクション
                # を開くため、確認後は commit() で閉じて `IDLE` へ戻す
                # （状態遷移の出力列には含めない副検証）。
                cur.execute(verify_sql)
                rows_after_commit = cur.fetchall()
                if not rows_after_commit:
                    print(
                        "psycopg_txn_status: expected at least one row after commit",
                        file=sys.stderr,
                    )
                    return 1
                conn.commit()

                # 2 周目: 暗黙 BEGIN → 構文エラー → INERROR → ROLLBACK → IDLE。
                try:
                    cur.execute(bad_sql)
                except psycopg.Error:
                    print(status_name(conn.info.transaction_status))
                else:
                    print(
                        "psycopg_txn_status: expected WIRE_TXN_BAD_SQL to fail",
                        file=sys.stderr,
                    )
                    return 1
                conn.rollback()
                print(status_name(conn.info.transaction_status))
        return 0
    except Exception as e:  # noqa: BLE001 — ハーネスへ理由を伝える最終防波堤
        sqlstate = getattr(e, "sqlstate", None)
        suffix = f" [SQLSTATE={sqlstate}]" if sqlstate else ""
        print(f"psycopg_txn_status: failed{suffix}: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
