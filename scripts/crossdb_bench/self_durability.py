"""crossdb ベンチの self（wire-server）が起動時に選ぶ書き込み durability の
opt-in ヘルパー（Issue #851）。

`--search-engine hnsw` opt-in（`self_hnsw.py`）と同型の「crossdb ベンチ専用の
環境変数から wire-server の閉じた語彙 CLI（`--durability immediate|none`。
`crates/wire-server/src/durability_opt.rs`。Issue #850）へ到達する唯一の入口」
を本モジュールに置く。`ingest_single_stmt` フェーズが APFS で `Immediate`
既定の fsync 相当コストを負っている事実（`docs/design/crossdb-bench.md`「単文
INSERT の durability 既定値」節）の A/B 計測に使う。

`immediate` 明示指定・未指定はいずれも既定経路（起動引数を増やさない・
`--durability` 自体を付けない）のため `self_db.py` の起動コマンドはビット
同一のまま不変。`none` 指定時のみ `--durability none` を追加する。
"""

from __future__ import annotations

import os

# crossdb ベンチが `--durability` opt-in を注入するための環境変数名。
ENV = "CROSSDB_SELF_DURABILITY"

# `durability_opt.rs::TOKENS` と同じ語彙（厳密一致のみ受理。trim・大文字小文字の
# 読み替えはしない）。
TOKENS = ("immediate", "none")

# `main.rs::run_server` が `--durability none` 選択時にのみ出す起動時警告行の
# 部分文字列（存在検出用。全文の転記はしない）。
_WARNING_MARKER = "wire-server: WARNING: --durability"


def parse_durability_env(raw: str | None) -> str | None:
    """`CROSSDB_SELF_DURABILITY` を解釈する（fail-closed）。

    未設定・空文字は `None`（起動引数を追加しない＝既定 `immediate` のまま）。
    `TOKENS` の厳密一致のみ受理し、それ以外（typo・大文字小文字違いを含む）は
    `ValueError` で拒否する（`self_hnsw.parse_hnsw_args_env` と同じ「黙って
    既定へフォールバックしない」方針。coding-rust.md 相当の untrusted 入力
    方針を Python 側でも踏襲する）。
    """
    if raw is None or raw == "":
        return None
    if raw not in TOKENS:
        raise ValueError(
            f"{ENV}={raw!r} is not a recognized durability token (expected one of {TOKENS!r})"
        )
    return raw


def warning_line_expected(token: str | None) -> bool:
    """指定した durability トークン（`None` は未指定＝既定 `immediate`）で
    wire-server が起動時警告を出すはずかどうか。`none` のみ `True`。"""
    return token == "none"


def has_durability_warning(log_text: str) -> bool:
    """wire-server の起動ログ（`SelfServer._read_log_tail()` 等）に `--durability`
    警告行が含まれるかを検出する。"""
    return _WARNING_MARKER in log_text


def durability_extra_args(token: str | None) -> list[str]:
    """`token`（`parse_durability_env` の戻り値）から wire-server 起動時の
    追加引数を組み立てる。`None`・`"immediate"` はいずれも空リスト（既定経路
    のまま起動コマンドをビット同一に保つ）。"""
    if token is None or token == "immediate":
        return []
    return ["--durability", token]


def verify_arm_identity(token: str | None, log_text: str) -> None:
    """起動ログの警告行の有無が選択した arm と一致することを確認する
    （arm 取り違え防止。A/B 計測で `immediate` のつもりが `none` のまま、
    あるいはその逆で走ってしまう事故を fail-closed に検出する）。

    一致しない場合は `RuntimeError`（呼び出し元の A/B ドライバがその run を
    破棄・報告できるようにする）。
    """
    expected = warning_line_expected(token)
    observed = has_durability_warning(log_text)
    if expected != observed:
        raise RuntimeError(
            "durability arm mismatch: "
            f"token={token!r} expected_warning={expected} observed_warning={observed} "
            "(wire-server startup log does not match the requested --durability arm)"
        )


def env_token_for_meta(token: str | None) -> str:
    """結果 JSON の `meta.durability` に書く表現（未指定は既定であることを
    明示する）。"""
    if token is None:
        return "immediate (default)"
    return token


def durability_source() -> str | None:
    """現在のプロセス環境から `CROSSDB_SELF_DURABILITY` を読み解釈する。
    `self_db.py::run` から呼ばれる薄いラッパー（`os.environ` 直接参照を
    1 箇所にまとめ、テストからは `parse_durability_env` を直接呼べるように
    分離する）。"""
    return parse_durability_env(os.environ.get(ENV))
