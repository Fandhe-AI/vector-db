"""自作ベクトル DB（wire-server 経由）の機能別ベンチマーク実装。

`target/release/wire-server` を子プロセスとして起動し、psycopg（簡易クエリ
プロトコルのみ・`autocommit=True`・パラメータなし `execute(sql)`）で接続する。
クエリ形は `crates/engine/examples/feature_bench.rs` に合わせてある（比較可能性
のため独自方言を作らない）。

RLS 相当のテナント境界は wire 接続ユーザーのテナントで自動適用される
（`bench` = tenant-a、`bench_b` = tenant-b の 2 ユーザーを users.txt に登録する）。
users.txt はベンチ専用の一意な作業サブディレクトリ（`<workdir>/self_bench_auth_*`）
へ生成し、`--workdir`（既定: 入力 redb の親ディレクトリ）直下に既存の
`users.txt` があっても触らない（codex-review P1）。

接続先ポートは環境変数 `CROSSDB_SELF_PORT`（既定 15432）で上書きできる。
"""

from __future__ import annotations

import atexit
import os
import shutil
import subprocess
import tempfile
import time

import psycopg

from common import (
    DIM,
    TENANT_OTHER,
    TENANT_VISIBLE,
    build_meta,
    env_port,
    measure,
    sql_escape_literal,
    unsupported,
    vec_literal,
    wait_for_port,
)

BIND_HOST = "127.0.0.1"
# wire-server 子プロセスの bind ポート。self はコンテナではなく本モジュールが
# 自ら起動するため、起動側・接続側とも同じこの値を使う（`CROSSDB_SELF_PORT`）。
BIND_PORT = env_port("CROSSDB_SELF_PORT", 15432)
USER_A = "bench"
USER_B = "bench_b"
PASSWORD = "bench"


def _is_allowlist_syntax_rejection(e: Exception) -> bool:
    """SQL 表層の許可リスト構文拒否（`crates/engine/src/sql/allowlist.rs`
    `SqlSurfaceError::UnsupportedSyntax`。wire_code `42601`・メッセージ先頭
    "unsupported SQL syntax"）と判定できる例外か。接続断・タイムアウト・
    ストレージ障害等の実行障害まで unsupported へ丸めて成功扱いにしない
    ため（codex-review P1）、この判定に一致しない例外は呼び出し元で
    再送出する。"""
    sqlstate = getattr(e, "sqlstate", None)
    return sqlstate == "42601" or "unsupported SQL syntax" in str(e)


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
    """wire-server 子プロセスのライフサイクル管理（起動・users.txt 生成・停止）。

    users.txt は `start()` 時に `<workdir>/self_bench_auth_<一意名>/users.txt` として
    新規作成し、`stop()` でサブディレクトリごと削除する。`workdir` 直下の
    既存ファイル（特に `users.txt`）には一切触れない。
    """

    def __init__(self, db_path: str, workdir: str, binary: str | None = None):
        self.db_path = db_path
        self.workdir = workdir
        self.binary = binary or self._default_binary()
        self.proc: subprocess.Popen | None = None
        # 子プロセスの stdout/stderr の書き出し先（ファイル）。パイプ（`subprocess.PIPE`）
        # を使うと、誰も読み取らない間に OS のパイプバッファが満杯になって
        # wire-server の `eprintln` がブロックしデッドロックし得る（Bugbot 指摘）ため、
        # 必ずファイルへ落とし、起動失敗時の診断はファイルを読み戻す。
        self.log_path: str | None = None
        self._log_file = None
        # 認証ファイル用の作業サブディレクトリ。`start()` でポート検査を通過した後に
        # 初めて作成する（起動を拒否する経路ではファイルを一切生成しない）。
        self.auth_dir: str | None = None
        self.users_path: str | None = None

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
        # 起動前にポートが既に LISTEN していれば別プロセス（前回の残骸等）であり、
        # そのまま進むと wait_for_port が成功して別サーバーを計測してしまうため拒否する。
        # 認証ファイルの生成はこの検査の後に行う（拒否経路で何も書き残さない）。
        if _port_is_listening(BIND_HOST, BIND_PORT):
            raise RuntimeError(
                f"{BIND_HOST}:{BIND_PORT} is already in use; refusing to start wire-server "
                "(another process may be listening)"
            )
        phc_a = _hash_password(self.binary, PASSWORD)
        phc_b = _hash_password(self.binary, PASSWORD)
        os.makedirs(self.workdir, exist_ok=True)
        # ベンチ専用の一意なサブディレクトリを新規作成（既存があれば衝突せず別名になる。
        # `mkdtemp` は既存ディレクトリを再利用しない＝`exist_ok=False` 相当で、
        # パスワードハッシュを置くため権限も 0700 になる）。workdir 直下の既存
        # `users.txt` は上書きしない。
        self.auth_dir = tempfile.mkdtemp(prefix=f"self_bench_auth_{os.getpid()}_", dir=self.workdir)
        self.users_path = os.path.join(self.auth_dir, "users.txt")
        with open(self.users_path, "w", encoding="utf-8") as f:
            f.write(f"{USER_A}:{TENANT_VISIBLE}:{phc_a}\n")
            f.write(f"{USER_B}:{TENANT_OTHER}:{phc_b}\n")
        # stdout/stderr はパイプではなくファイルへ書く（パイプは読み手が居ないと
        # バッファ満杯で子プロセスがブロックする）。認証サブディレクトリと同じ
        # 一時領域に置き、`stop()` で一緒に削除する。
        self.log_path = os.path.join(self.auth_dir, "wire-server.log")
        self._log_file = open(self.log_path, "w", encoding="utf-8")
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
            stdout=self._log_file,
            stderr=subprocess.STDOUT,
        )
        atexit.register(self.stop)
        # 子プロセスの生存を確認しながら LISTEN を待つ（bind 失敗等で子が終了した
        # 場合は出力を添えて即座に失敗させる。ポート待ちだけでは検出できない）。
        deadline = time.time() + 30.0
        while True:
            if self.proc.poll() is not None:
                out = self._read_log_tail()
                # stop() が self.proc を None にするため、終了コードは先に控える
                # （診断に必要な終了コードと出力をエラーメッセージから落とさない）。
                code = self.proc.returncode
                # 起動失敗でも認証サブディレクトリを残さない。
                self.stop()
                raise RuntimeError(
                    f"wire-server exited during startup (code {code}): {out.strip()}"
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

    def _read_log_tail(self, max_bytes: int = 64 * 1024) -> str:
        """子プロセスのログファイル末尾（既定 64 KiB）を診断用に読み戻す。
        ファイルが無い・読めない場合は空文字（診断を理由に起動失敗の例外を隠さない）。"""
        if self.log_path is None:
            return ""
        try:
            if self._log_file is not None:
                self._log_file.flush()
            with open(self.log_path, "rb") as f:
                f.seek(0, os.SEEK_END)
                size = f.tell()
                f.seek(max(0, size - max_bytes))
                return f.read().decode("utf-8", errors="replace")
        except OSError:
            return ""

    def stop(self) -> None:
        """子プロセスを停止し、認証サブディレクトリ（ログファイル含む）を削除する
        （多重呼び出し可。`run()` の finally と atexit の双方から呼ばれる）。"""
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5.0)
        self.proc = None
        if self._log_file is not None:
            try:
                self._log_file.close()
            except OSError:
                pass
        self._log_file = None
        self.log_path = None
        if self.auth_dir is not None and os.path.isdir(self.auth_dir):
            shutil.rmtree(self.auth_dir, ignore_errors=True)
        self.auth_dir = None
        self.users_path = None

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
    # 作業コピーは実行ごとに一意なサブディレクトリへ置く。固定パスだと同じ
    # workdir で二重起動した 2 回目の copyfile が、1 回目の wire-server が開いて
    # いる DB を切り詰めて破壊する（ポート占有検査はコピー後の start() 内のため
    # 防げない）。計測後は作業コピーごと削除する。
    os.makedirs(workdir, exist_ok=True)
    run_dir = tempfile.mkdtemp(prefix=f"self_bench_work_{os.getpid()}_", dir=workdir)
    work_db = os.path.join(run_dir, "self_bench_work.redb")
    shutil.copyfile(args.rows_file, work_db)
    server = SelfServer(db_path=work_db, workdir=workdir)
    try:
        server.start()
        return _run_phases(args, queries, server)
    finally:
        server.stop()
        shutil.rmtree(run_dir, ignore_errors=True)


def _run_phases(args, queries: list[dict], server: SelfServer) -> dict:
    """起動済み wire-server に対して全フェーズを実行する（`run` から呼ばれる）。"""
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

        # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
        # LLM のコンテキストへ丸ごと渡す想定のため、本文（body）の送出コストを
        # 含めて計測する。LIMIT の上限は `crates/engine/src/core.rs::MAX_SEARCH_K`
        # （10,000）のため 1,000 は受理される。`rows_returned` は最終呼び出しで
        # 実際に返った行数（k と一致しなければ経路側の打ち切りを疑う）。
        def _exec_rows(sql: str) -> list:
            with conn_a.cursor() as cur:
                cur.execute(sql)
                return cur.fetchall()

        def bulk_knn(k: int):
            def _run(qv):
                return _exec_rows(
                    f"SELECT id, body FROM docs ORDER BY embedding <=> '{qv}' LIMIT {k}"
                )

            return _run

        for k in (200, 1000):
            stats, last = measure(bulk_knn(k), query_vecs)
            phases[f"bulk_knn_k{k}"] = {**stats, "k": k, "rows_returned": len(last)}

        def bulk_knn_where(qv):
            return _exec_rows(
                f"SELECT id, body FROM docs WHERE lang = 'ja' "
                f"ORDER BY embedding <=> '{qv}' LIMIT 200"
            )

        stats, last = measure(bulk_knn_where, query_vecs)
        phases["bulk_knn_where_k200"] = {**stats, "k": 200, "rows_returned": len(last)}

        def bulk_hybrid(i):
            qv, qt = query_vecs[i], query_texts[i]
            return _exec_rows(
                f"SELECT id, body FROM docs ORDER BY hybrid_rrf(embedding, '{qv}', "
                f"body, '{qt}') LIMIT 200"
            )

        stats, last = measure(bulk_hybrid, idxs)
        phases["bulk_hybrid_k200"] = {**stats, "k": 200, "rows_returned": len(last)}

        # ORDER BY なしの行取得は SQL 表層の許可リスト
        # （`crates/engine/src/sql/allowlist.rs::parse_select_shape`）が
        # `ORDER BY <distance>` か `USING PLAN` を必須とするため受理されない。
        # 推測で unsupported にせず、実際に 1 回実行して拒否を確認したうえで記録する
        # （explain フェーズと同じく、拒否応答は接続を切断しない）。
        nosort_sql = "SELECT id, body FROM docs WHERE lang = 'ja' LIMIT 500"
        try:
            _exec_rows(nosort_sql)
            accepted = True
        except Exception as e:  # noqa: BLE001 - 許可リスト拒否かどうかを下で検証する
            accepted = False
            # 許可リストの構文拒否（`42601`・"unsupported SQL syntax"）以外の例外
            # （接続断・タイムアウト等）を「想定内の拒否」として記録すると fail-open に
            # なるため、拒否以外はそのまま再送出して計測全体を失敗させる。
            if not _is_allowlist_syntax_rejection(e):
                raise
            phases["scan_where_nosort_k500"] = unsupported(
                "SQL surface requires ORDER BY <distance> or USING PLAN for "
                f"row-returning SELECT (sqlstate={getattr(e, 'sqlstate', None)}): {e!r}"
            )
        if accepted:
            # 許可リストが将来受理するようになった場合は通常フェーズとして計測する
            # （unsupported に丸めると仕様変更に気づけない）。
            stats, last = measure(lambda _: _exec_rows(nosort_sql), [None])
            phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

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
        except Exception as e:  # noqa: BLE001 - 許可リスト構文拒否のみ unsupported とする
            # 接続断・タイムアウト等の実行障害を unsupported へ丸めない
            # （codex-review P1）。許可リスト拒否（`42601`）と確認できない例外は
            # 再送出して計測全体を失敗させる。
            if not _is_allowlist_syntax_rejection(e):
                raise
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
        except Exception as e:  # noqa: BLE001 - 許可リスト構文拒否のみ unsupported とする
            # 接続断・タイムアウト等の実行障害を unsupported へ丸めない
            # （codex-review P1）。許可リスト拒否（`42601`）と確認できない例外は
            # 再送出して計測全体を失敗させる。
            if not _is_allowlist_syntax_rejection(e):
                raise
            phases["explain"] = unsupported(
                f"EXPLAIN は `USING PLAN(...)` 文にのみ対応（許可リスト拒否を実機確認）: {e!r}"
            )

        # --- ingest_bulk: wire は COPY 相当の一括投入プロトコルを持たない ---
        phases["ingest_bulk"] = unsupported(
            "wire プロトコルに COPY 相当が無く、SQL 表層は単文 INSERT のみ受理する"
            "（`EngineCore::execute_insert_sql_batch` は Rust API であり wire 未露出）"
        )

        # --- ingest_single_stmt: 行形 INSERT を 1,000 行単文で送る ---
        # 投入ベクトルの次元はクエリ fixture（`queries200.jsonl`）の embedding から
        # 取る。DIM（128）固定にすると 128 以外で seed した redb では列型
        # `Vector(dim)` と不一致になり投入が全件拒否される（codex-review P2）。
        rng_dim = len(queries[0]["embedding"]) if queries else DIM

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
        except Exception as e:  # noqa: BLE001 - 許可リスト構文拒否のみ unsupported とする
            # 接続断・タイムアウト・重複 id・ストレージ障害等の実行障害を
            # unsupported へ丸めない（codex-review P1）。許可リスト拒否
            # （`42601`）と確認できない例外は再送出して計測全体を失敗させる。
            if not _is_allowlist_syntax_rejection(e):
                raise
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
            dim=rng_dim,
        )
        return {"meta": meta, "phases": phases}
    finally:
        server.stop()
