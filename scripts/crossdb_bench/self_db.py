"""自作ベクトル DB（wire-server 経由）の機能別ベンチマーク実装。

`target/release/wire-server` を子プロセスとして起動し、psycopg（簡易クエリ
プロトコルのみ・`autocommit=True`・パラメータなし `execute(sql)`）で接続する。
クエリ形は `crates/engine/examples/feature_bench.rs` に合わせてある（比較可能性
のため独自方言を作らない）。

RLS 相当のテナント境界は wire 接続ユーザーのテナントで自動適用される
（`bench` = tenant-a、`bench_b` = tenant-b の 2 ユーザーを users.txt に登録する）。
"""

from __future__ import annotations

import atexit
import os
import subprocess
import time

import psycopg

from common import (
    DIM,
    TENANT_OTHER,
    TENANT_VISIBLE,
    build_meta,
    measure,
    sql_escape_literal,
    unsupported,
    vec_literal,
    wait_for_port,
)

BIND_HOST = "127.0.0.1"
BIND_PORT = 15432
USER_A = "bench"
USER_B = "bench_b"
PASSWORD = "bench"


def _hash_password(binary: str, password: str) -> str:
    """`wire-server hash-password` サブコマンドで phc 文字列を得る（平文はここでのみ扱う）。"""
    proc = subprocess.run(
        [binary, "hash-password"],
        input=password,
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


def _port_is_listening(host: str, port: int) -> bool:
    """`host:port` が現在 TCP 接続を受理するかを 1 回だけ確認する（待機しない）。"""
    import socket

    try:
        with socket.create_connection((host, port), timeout=0.5):
            return True
    except OSError:
        return False


class SelfServer:
    """wire-server 子プロセスのライフサイクル管理（起動・users.txt 生成・停止）。"""

    def __init__(self, db_path: str, workdir: str, binary: str | None = None):
        self.db_path = db_path
        self.workdir = workdir
        self.binary = binary or self._default_binary()
        self.proc: subprocess.Popen | None = None
        self.users_path = os.path.join(workdir, "users.txt")

    @staticmethod
    def _default_binary() -> str:
        repo_root = os.path.abspath(
            os.path.join(os.path.dirname(__file__), "..", "..")
        )
        return os.path.join(repo_root, "target", "release", "wire-server")

    def start(self) -> None:
        if not os.path.exists(self.binary):
            raise FileNotFoundError(
                f"wire-server binary not found: {self.binary}"
                "（`cargo build --release -p wire-server` を先に実行）"
            )
        phc_a = _hash_password(self.binary, PASSWORD)
        phc_b = _hash_password(self.binary, PASSWORD)
        os.makedirs(self.workdir, exist_ok=True)
        with open(self.users_path, "w", encoding="utf-8") as f:
            f.write(f"{USER_A}:{TENANT_VISIBLE}:{phc_a}\n")
            f.write(f"{USER_B}:{TENANT_OTHER}:{phc_b}\n")

        # 起動前にポートが既に LISTEN していれば別プロセス（前回の残骸等）であり、
        # そのまま進むと wait_for_port が成功して別サーバーを計測してしまうため拒否する。
        if _port_is_listening(BIND_HOST, BIND_PORT):
            raise RuntimeError(
                f"{BIND_HOST}:{BIND_PORT} is already in use; refusing to start wire-server "
                "(another process may be listening)"
            )
        self.proc = subprocess.Popen(
            [
                self.binary,
                "--users",
                self.users_path,
                "--db",
                self.db_path,
                "--bind",
                f"{BIND_HOST}:{BIND_PORT}",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        atexit.register(self.stop)
        # 子プロセスの生存を確認しながら LISTEN を待つ（bind 失敗等で子が終了した
        # 場合は出力を添えて即座に失敗させる。ポート待ちだけでは検出できない）。
        deadline = time.time() + 30.0
        while True:
            if self.proc.poll() is not None:
                out = self.proc.stdout.read() if self.proc.stdout else ""
                raise RuntimeError(
                    f"wire-server exited during startup (code {self.proc.returncode}): {out.strip()}"
                )
            if _port_is_listening(BIND_HOST, BIND_PORT):
                break
            if time.time() > deadline:
                self.stop()
                raise RuntimeError("wire-server did not start listening within timeout")
            time.sleep(0.2)
        # 起動直後の TCP accept 可能とクエリ受理可能の間には短いラグがあり得るため
        # 軽く待つ（psycopg 側の接続リトライは別途 connect() 呼び出し元が担う）。
        time.sleep(0.3)

    def stop(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.proc = None

    def connect(self, user: str = USER_A) -> psycopg.Connection:
        conn = psycopg.connect(
            host=BIND_HOST,
            port=BIND_PORT,
            user=user,
            password=PASSWORD,
            dbname="bench",
            autocommit=True,
        )
        # psycopg は既定で同一クエリ文字列が `prepare_threshold`（既定 5）回
        # 実行されるとサーバー側 PREPARE（拡張クエリプロトコル・Parse メッセージ）
        # へ自動的に切り替える。wire-server は簡易クエリプロトコルのみ対応
        # （Parse/Bind 非対応で切断される）ため、固定 SQL を 50 回超計測する
        # 集計・GROUP BY 等のフェーズで自動 PREPARE が発火して接続断になる
        # （実機確認: `FeatureNotSupported: extended query protocol is not
        # supported on this connection`）。無効化して常に簡易クエリプロトコルを使う。
        conn.prepare_threshold = None
        return conn


def _exec_ids(conn: psycopg.Connection, sql: str) -> list:
    with conn.cursor() as cur:
        cur.execute(sql)
        rows = cur.fetchall()
    # `SELECT *` は id が先頭列（CREATE TABLE の列順。feature_bench.rs も同じ前提）。
    return [r[0] for r in rows]


def run(args, queries: list[dict]) -> dict:
    """self（wire-server）の全フェーズを実行する。

    `args.rows_file` は self の場合 redb ファイルパスを指す（他 DB モジュールの
    `run(args, docs, queries)` とは引数の意味が異なる。docs jsonl は不要
    ——wire-server は既存 redb をそのまま開くため再投入しない）。
    """
    # self は既定エンジン（brute-force）のみを wire 経由で計測できる（ANN opt-in は
    # wire から選択できない）。hnsw 構成を受理して exact と同じ経路の結果を
    # self_hnsw.json として保存すると比較結果を誤認させるため拒否する。
    if args.config != "exact":
        raise ValueError(
            f"self supports only --config exact (got {args.config!r}); "
            "ANN opt-in is not selectable over the wire protocol"
        )
    workdir = args.workdir
    # 計測は redb を書き換える（ingest_single_stmt が行と operation_id 台帳を追加する）
    # ため、渡された fixture を直接開かず作業コピーに対して実行する。コピーしないと
    # 2 回目以降の実行で同一 operation_id の再送が内容不一致として拒否され
    # （TASK-101・RECOVER-10 の契約どおり）、可視行数も毎回増えて比較できなくなる。
    import shutil

    os.makedirs(workdir, exist_ok=True)
    work_db = os.path.join(workdir, "self_bench_work.redb")
    shutil.copyfile(args.rows_file, work_db)
    server = SelfServer(db_path=work_db, workdir=workdir)
    server.start()
    try:
        conn_a = server.connect(USER_A)
        phases: dict = {}

        query_vecs = [vec_literal(q["embedding"]) for q in queries]
        query_texts = [sql_escape_literal(q.get("text", "")) for q in queries]

        # --- vector_knn ---
        def knn(qv):
            sql = f"SELECT id FROM docs ORDER BY embedding <=> '{qv}' LIMIT 10"
            return _exec_ids(conn_a, sql)

        stats, _ = measure(knn, query_vecs)
        # recall 検算用に全 200 クエリ分を 1 回ずつ実行して id を集める。
        knn_ids_all = [knn(qv) for qv in query_vecs]
        phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

        # --- vector_knn_where（point_where と同形のため統合。task note 準拠） ---
        def knn_where(qv):
            sql = (
                f"SELECT id FROM docs WHERE lang = 'ja' "
                f"ORDER BY embedding <=> '{qv}' LIMIT 10"
            )
            return _exec_ids(conn_a, sql)

        stats, _ = measure(knn_where, query_vecs)
        phases["vector_knn_where"] = stats
        phases["point_where"] = {
            "note": "vector_knn_where と同一クエリ形のため統合（task 指示準拠）",
            **stats,
        }

        # --- where_compound_count ---
        def compound_count(_):
            sql = "SELECT COUNT(*) FROM docs WHERE visible() AND id > 100 AND lang = 'ja'"
            with conn_a.cursor() as cur:
                cur.execute(sql)
                return cur.fetchone()[0]

        stats, last = measure(compound_count, [None])
        phases["where_compound_count"] = {**stats, "result": last}

        # --- agg_count ---
        def agg_count(_):
            with conn_a.cursor() as cur:
                cur.execute("SELECT COUNT(*) FROM docs")
                return cur.fetchone()[0]

        stats, last = measure(agg_count, [None])
        phases["agg_count"] = {**stats, "result": last}

        # --- agg_multi ---
        def agg_multi(_):
            with conn_a.cursor() as cur:
                cur.execute("SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs")
                return cur.fetchone()

        stats, last = measure(agg_multi, [None])
        phases["agg_multi"] = {**stats, "result": list(last) if last else None}

        # --- group_by_having ---
        def group_by(_):
            sql = (
                "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang "
                "HAVING n > 1 ORDER BY n DESC LIMIT 5"
            )
            with conn_a.cursor() as cur:
                cur.execute(sql)
                return cur.fetchall()

        stats, last = measure(group_by, [None])
        phases["group_by_having"] = {**stats, "result": last}

        # --- hybrid_rrf ---
        def hybrid(i):
            qv, qt = query_vecs[i], query_texts[i]
            sql = (
                f"SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '{qv}', "
                f"body, '{qt}') LIMIT 10"
            )
            return _exec_ids(conn_a, sql)

        idxs = list(range(len(query_vecs)))
        stats, _ = measure(hybrid, idxs)
        phases["hybrid_rrf"] = stats

        # --- mode_recall / mode_precision ---
        def mode_query(mode):
            def _run(qv):
                sql = (
                    f"SELECT id FROM docs ORDER BY embedding <=> '{qv}' "
                    f"LIMIT 10 USING MODE '{mode}'"
                )
                return _exec_ids(conn_a, sql)

            return _run

        stats, _ = measure(mode_query("recall"), query_vecs)
        phases["mode_recall"] = stats
        stats, _ = measure(mode_query("precision"), query_vecs)
        phases["mode_precision"] = stats

        # --- rls_isolation（tenant-b 接続で COUNT） ---
        # 実機確認済みの現行契約: 自作 DB はどのテナントの wire セッションから
        # も `visibility = 'public'` の行のみが可視であり、private 行は所有
        # テナント自身のセッションからも不可視。そのため tenant-a（bench）・
        # tenant-b（bench_b）いずれの COUNT(*) も docs25k.jsonl の public 行数
        # （23,000）に一致するはず（他 DB モジュールの `visibility = 'public'`
        # フィルタが模している対象そのもの）。
        #
        # 接続は使用直前に開く（他フェーズの実行に数十秒かかり得るため、
        # 先に開いたまま放置すると wire-server の読み取りタイムアウト
        # `wire_server::limits::READ_TIMEOUT`〔既定 30 秒〕でサーバー側から
        # 切断される。実機確認: measure() 内の 2 回目以降の呼び出しで
        # `OperationalError: consuming input failed: server closed the
        # connection unexpectedly`）。
        conn_b = server.connect(USER_B)

        def rls_count(_):
            with conn_b.cursor() as cur:
                cur.execute("SELECT COUNT(*) FROM docs")
                return cur.fetchone()[0]

        stats, last = measure(rls_count, [None])
        phases["rls_isolation"] = {**stats, "tenant_b_count": last}

        # --- udf_call ---
        try:
            with conn_a.cursor() as cur:
                cur.execute(
                    "CREATE FUNCTION norm_scale(v, s) AS s * vec_sum(vec_div(v, vec_norm(v)))"
                )
            qv0 = query_vecs[0]
            udf_sql = (
                f"SELECT id, norm_scale(embedding, 2.0) AS score FROM docs "
                f"ORDER BY embedding <=> '{qv0}' LIMIT 10"
            )

            def udf_call(_):
                with conn_a.cursor() as cur:
                    cur.execute(udf_sql)
                    return cur.fetchall()

            stats, _ = measure(udf_call, [None])
            phases["udf_call"] = stats
        except Exception as e:  # noqa: BLE001 - wire 経由 CREATE FUNCTION 不許可の可能性を記録
            phases["udf_call"] = unsupported(
                f"CREATE FUNCTION が wire 経由で失敗: {e!r}"
            )

        # --- explain ---
        # 実機確認: `EXPLAIN` は `SELECT ... USING PLAN(...)` 文にのみ対応する
        # （`crates/engine/src/sql/allowlist.rs`
        # "EXPLAIN is only supported for SELECT ... USING PLAN(...) statements"）。
        # 素の `ORDER BY embedding <=> '...'` に対する `EXPLAIN` は許可リストで
        # 拒否される。`USING PLAN` は LLM プランナー注入（`--planner-endpoint`
        # 等）が無いと fail-closed で拒否されるため、本ベンチでは同注入を
        # 行っておらず EXPLAIN フェーズは fail-closed に unsupported とする。
        try:
            with conn_a.cursor() as cur:
                cur.execute(f"EXPLAIN SELECT id FROM docs ORDER BY embedding <=> '{query_vecs[0]}' LIMIT 10")
            phases["explain"] = unsupported("到達しないはずの分岐（EXPLAIN が成功した）")
        except Exception as e:  # noqa: BLE001
            phases["explain"] = unsupported(
                f"EXPLAIN は `USING PLAN(...)` 文にのみ対応（許可リスト拒否を実機確認）: {e!r}"
            )

        # --- ingest_bulk: wire は COPY 相当の一括投入プロトコルを持たない ---
        phases["ingest_bulk"] = unsupported(
            "wire プロトコルに COPY 相当が無く、SQL 表層は単文 INSERT のみ受理する"
            "（`EngineCore::execute_insert_sql_batch` は Rust API であり wire 未露出）"
        )

        # --- ingest_single_stmt: 行形 INSERT を 1,000 行単文で送る ---
        rng_dim = DIM

        def make_row_sql(n: int) -> str:
            import random

            rid = 10_000_000 + n
            emb = vec_literal([random.random() * 2 - 1 for _ in range(rng_dim)])
            lang = "ja" if n % 2 == 0 else "en"
            topic = f"topic-{n % 20:02d}"
            body = sql_escape_literal(f"crossdb bench ingest row {n}")
            return (
                f"INSERT INTO docs (id, embedding, lang, topic, body) VALUES "
                f"({rid}, '{emb}', '{lang}', '{topic}', '{body}') "
                f"USING OPERATION_ID 'xdb-{n}'"
            )

        n_ingest = 1000
        t0 = time.perf_counter()
        ingest_error = None
        rows_ok = 0
        try:
            with conn_a.cursor() as cur:
                for n in range(n_ingest):
                    cur.execute(make_row_sql(n))
                    rows_ok += 1
        except Exception as e:  # noqa: BLE001 - 失敗時はエラーコードを記録して N/A
            ingest_error = repr(e)
        t1 = time.perf_counter()
        if ingest_error is not None:
            phases["ingest_single_stmt"] = unsupported(
                f"{rows_ok} 行成功後に失敗: {ingest_error}"
            )
        else:
            elapsed = t1 - t0
            phases["ingest_single_stmt"] = {
                "rows": rows_ok,
                "seconds": elapsed,
                "rows_per_sec": rows_ok / elapsed if elapsed > 0 else None,
                "commit_granularity": "per_statement",
            }

        # COUNT(*) は wire 経由ではテキスト表現で返る（psycopg が INT8 として
        # デコードしない）ため int へ変換する。変換できない場合は 0 で fail-closed。
        rows_visible_raw = phases.get("agg_count", {}).get("result")
        try:
            rows_visible = int(rows_visible_raw)
        except (TypeError, ValueError):
            rows_visible = 0
        meta = build_meta(
            db="self",
            version="wire-server (workspace HEAD)",
            connection="loopback TCP (psycopg simple query protocol)",
            config=args.config,
            rows=rows_visible,
        )
        return {"meta": meta, "phases": phases}
    finally:
        server.stop()
