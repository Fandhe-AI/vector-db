#!/usr/bin/env python3
"""fsync 系原始操作（`fsync`/`fdatasync`/macOS `F_FULLFSYNC`/`F_BARRIERFSYNC`）の
直接計測プローブ（Issue #851）。標準ライブラリのみ・依存追加なし。

`ingest_single_stmt`（`docs/design/crossdb-bench.md`）の self（wire-server 経由）
が APFS で遅い一因として推定されている「redb `Durability::Immediate` が 1 commit
ごとに std `sync_data` → macOS では `F_FULLFSYNC` を発行する」という仮説を、
redb・engine を経由しない最小構成で裏付けるための参考値を出す。

1 試行 = 一時ファイルへ 4 KiB 書き込み → 対象の同期原始操作 1 回。ファイルは
`--dir` に作り、実行後に削除する。`--dir` は計測対象（例: crossdb フィクスチャ
と同じボリューム）を指定する。

macOS 固有の `F_FULLFSYNC`/`F_BARRIERFSYNC` は `platform.system() == "Darwin"`
の場合のみ計測し、失敗した原始操作は `unsupported` として記録したうえで他の
原始操作の計測は続行する（1 つの失敗で全体を失敗させない）。
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import platform
import subprocess
import sys
import time

# Python の `fcntl` モジュールは `F_FULLFSYNC` を公開している（Darwin 限定）が、
# `F_BARRIERFSYNC` は公開していないため `sys/fcntl.h`（macOS SDK）由来の数値を
# 直接定義する（Darwin 以外では未使用）。
F_BARRIERFSYNC = 85

_PAYLOAD = b"\xab" * 4096


def _percentile(sorted_values: list[float], pct: float) -> float:
    """`sorted_values`（昇順ソート済み）の百分位数を最近傍法で返す。
    `scripts/crossdb_bench/tests` 系と同じく統計計算は依存追加なしで自作する。"""
    if not sorted_values:
        raise ValueError("cannot compute a percentile of an empty sequence")
    if not 0.0 <= pct <= 100.0:
        raise ValueError(f"percentile out of range [0, 100]: {pct}")
    idx = round((pct / 100.0) * (len(sorted_values) - 1))
    idx = max(0, min(len(sorted_values) - 1, idx))
    return sorted_values[idx]


def summarize(samples_ns: list[int]) -> dict:
    """1 原始操作の生サンプル（ナノ秒）から min/p50/p95 を計算する。"""
    if not samples_ns:
        raise ValueError("cannot summarize an empty sample set")
    ordered = sorted(samples_ns)
    return {
        "samples_ns": samples_ns,
        "min_ns": ordered[0],
        "p50_ns": _percentile([float(v) for v in ordered], 50.0),
        "p95_ns": _percentile([float(v) for v in ordered], 95.0),
        "iters": len(samples_ns),
    }


def _time_op(op) -> int:
    t0 = time.perf_counter_ns()
    op()
    return time.perf_counter_ns() - t0


def probe_fsync(directory: str, iters: int) -> dict:
    """`os.fsync`（全 OS 共通）を計測する。"""
    samples: list[int] = []
    for _ in range(iters):
        path = os.path.join(directory, f".fsync_probe_fsync_{os.getpid()}_{len(samples)}")
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            os.write(fd, _PAYLOAD)
            samples.append(_time_op(lambda: os.fsync(fd)))
        finally:
            os.close(fd)
            os.unlink(path)
    return summarize(samples)


def probe_fdatasync(directory: str, iters: int) -> dict | None:
    """`os.fdatasync`（Linux 等。macOS の Python は未対応のため `None` を返す）。"""
    if not hasattr(os, "fdatasync"):
        return None
    samples: list[int] = []
    for _ in range(iters):
        path = os.path.join(directory, f".fsync_probe_fdatasync_{os.getpid()}_{len(samples)}")
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            os.write(fd, _PAYLOAD)
            samples.append(_time_op(lambda: os.fdatasync(fd)))
        finally:
            os.close(fd)
            os.unlink(path)
    return summarize(samples)


def _probe_fcntl_sync(directory: str, iters: int, cmd: int, label: str) -> dict:
    """macOS 固有の `fcntl(fd, cmd)` 型同期原始操作（`F_FULLFSYNC`／
    `F_BARRIERFSYNC`）を計測する共通実装。失敗したら `unsupported` を返す
    （1 原始操作の失敗で他原始操作の計測を止めない）。"""
    samples: list[int] = []
    try:
        for _ in range(iters):
            path = os.path.join(
                directory, f".fsync_probe_{label}_{os.getpid()}_{len(samples)}"
            )
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            try:
                os.write(fd, _PAYLOAD)
                samples.append(_time_op(lambda: fcntl.fcntl(fd, cmd)))
            finally:
                os.close(fd)
                os.unlink(path)
    except OSError as exc:
        return {"unsupported": True, "reason": repr(exc)}
    return summarize(samples)


def probe_full_fsync(directory: str, iters: int) -> dict | None:
    """macOS `F_FULLFSYNC`（デバイスキャッシュまで確実に書き戻す強い同期。
    Darwin 以外では `None`）。"""
    if platform.system() != "Darwin":
        return None
    # Python の `fcntl` は Darwin ビルドで `F_FULLFSYNC` を公開している
    # （getattr フォールバックは他プラットフォームでの誤動作を避けるため）。
    cmd = getattr(fcntl, "F_FULLFSYNC", 51)
    return _probe_fcntl_sync(directory, iters, cmd, "fullfsync")


def probe_barrier_fsync(directory: str, iters: int) -> dict | None:
    """macOS `F_BARRIERFSYNC`（書き込み順序のみ保証する弱い同期。
    Darwin 以外では `None`）。"""
    if platform.system() != "Darwin":
        return None
    return _probe_fcntl_sync(directory, iters, F_BARRIERFSYNC, "barrierfsync")


def _resolve_mount_point(directory: str) -> str | None:
    """`df <directory>` の出力末尾列（`Mounted on`）からマウント点を取り出す。
    `diskutil info` は任意のディレクトリでは解決できず、実マウント点
    （`/System/Volumes/Data` 等）を渡す必要があるため（macOS のみで使う）。"""
    try:
        out = subprocess.run(
            ["df", directory], check=False, capture_output=True, text=True, timeout=10
        ).stdout
        lines = out.splitlines()
        if len(lines) < 2:
            return None
        fields = lines[1].split()
        return fields[-1] if fields else None
    except (OSError, subprocess.SubprocessError):
        return None


def detect_fs_type(directory: str) -> str:
    """`directory` が乗っているファイルシステム種別を検出する（参考情報。
    検出できなければ `"unknown"`。macOS は `diskutil info`、Linux は `df -T`
    の出力を解析する）。"""
    system = platform.system()
    try:
        if system == "Darwin":
            mount_point = _resolve_mount_point(directory)
            if mount_point is None:
                return "unknown"
            out = subprocess.run(
                ["diskutil", "info", mount_point],
                check=False,
                capture_output=True,
                text=True,
                timeout=10,
            ).stdout
            for line in out.splitlines():
                if "File System Personality" in line:
                    return line.split(":", 1)[1].strip()
            return "unknown"
        if system == "Linux":
            out = subprocess.run(
                ["df", "-T", directory],
                check=False,
                capture_output=True,
                text=True,
                timeout=10,
            ).stdout
            lines = out.splitlines()
            if len(lines) >= 2:
                fields = lines[1].split()
                if len(fields) >= 2:
                    return fields[1]
            return "unknown"
    except (OSError, subprocess.SubprocessError):
        return "unknown"
    return "unknown"


def detect_volume_name(directory: str) -> str:
    """検出できるボリューム名・マウント点（参考情報。検出できなければ空文字）。"""
    system = platform.system()
    try:
        if system == "Darwin":
            mount_point = _resolve_mount_point(directory)
            if mount_point is None:
                return ""
            out = subprocess.run(
                ["diskutil", "info", mount_point],
                check=False,
                capture_output=True,
                text=True,
                timeout=10,
            ).stdout
            for line in out.splitlines():
                if line.strip().startswith("Volume Name"):
                    return line.split(":", 1)[1].strip()
            return ""
        out = subprocess.run(
            ["df", directory], check=False, capture_output=True, text=True, timeout=10
        ).stdout
        lines = out.splitlines()
        if len(lines) >= 2:
            fields = lines[1].split()
            if fields:
                return fields[-1]
        return ""
    except (OSError, subprocess.SubprocessError):
        return ""


def run_probe(directory: str, iters: int) -> dict:
    if iters < 1:
        raise ValueError(f"--iters must be >= 1 (got {iters})")
    if not os.path.isdir(directory):
        raise ValueError(f"--dir does not exist or is not a directory: {directory}")
    return {
        "platform": platform.system(),
        "machine": platform.machine(),
        "dir": os.path.abspath(directory),
        "fs_type": detect_fs_type(directory),
        "volume": detect_volume_name(directory),
        "iters": iters,
        "payload_bytes": len(_PAYLOAD),
        "probes": {
            "fsync": probe_fsync(directory, iters),
            "fdatasync": probe_fdatasync(directory, iters),
            "fullfsync": probe_full_fsync(directory, iters),
            "barrierfsync": probe_barrier_fsync(directory, iters),
        },
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--dir",
        required=True,
        help="計測対象ボリューム上の書き込み可能ディレクトリ（一時ファイルを作成・削除する）",
    )
    parser.add_argument("--iters", type=int, default=100, help="原始操作あたりの試行回数（既定 100）")
    parser.add_argument("--out", required=True, help="結果 JSON の出力先パス")
    args = parser.parse_args(argv)

    result = run_probe(args.dir, args.iters)
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"fsync_probe: wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
