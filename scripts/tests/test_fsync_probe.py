"""`fsync_probe.py` の統計ヘルパ・引数検証の単体テスト（Issue #851。
実 I/O の同期原始操作自体は計時のため対象外——`common.py::measure` 系と
同じ方針。`python3 -m unittest discover scripts/tests` で実行する）。
"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import fsync_probe  # noqa: E402


class PercentileTests(unittest.TestCase):
    def test_rejects_empty_sequence(self) -> None:
        with self.assertRaises(ValueError):
            fsync_probe._percentile([], 50.0)

    def test_rejects_out_of_range_percentile(self) -> None:
        with self.assertRaises(ValueError):
            fsync_probe._percentile([1.0, 2.0], -1.0)
        with self.assertRaises(ValueError):
            fsync_probe._percentile([1.0, 2.0], 100.1)

    def test_p50_of_single_value(self) -> None:
        self.assertEqual(fsync_probe._percentile([5.0], 50.0), 5.0)

    def test_p95_selects_near_top_of_sorted_list(self) -> None:
        values = [float(v) for v in range(1, 101)]  # 1..100
        # 最近傍法: idx = round(0.95 * 99) = 94 -> values[94] = 95.0
        self.assertEqual(fsync_probe._percentile(values, 95.0), 95.0)


class SummarizeTests(unittest.TestCase):
    def test_rejects_empty_samples(self) -> None:
        with self.assertRaises(ValueError):
            fsync_probe.summarize([])

    def test_computes_min_and_percentiles(self) -> None:
        result = fsync_probe.summarize([300, 100, 200])
        self.assertEqual(result["min_ns"], 100)
        self.assertEqual(result["iters"], 3)
        self.assertIn("p50_ns", result)
        self.assertIn("p95_ns", result)
        self.assertEqual(result["samples_ns"], [300, 100, 200])


class RunProbeValidationTests(unittest.TestCase):
    def test_rejects_zero_or_negative_iters(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(ValueError):
                fsync_probe.run_probe(d, 0)
            with self.assertRaises(ValueError):
                fsync_probe.run_probe(d, -1)

    def test_rejects_nonexistent_directory(self) -> None:
        with self.assertRaises(ValueError):
            fsync_probe.run_probe("/nonexistent/path/for/fsync-probe-test", 1)

    def test_runs_fsync_probe_on_real_directory(self) -> None:
        # 実 I/O を伴う唯一のテスト。1 試行のみ・一時ディレクトリへ書く
        # （どの OS でも `os.fsync` は必ず利用可能）。
        with tempfile.TemporaryDirectory() as d:
            result = fsync_probe.run_probe(d, 1)
            self.assertEqual(result["iters"], 1)
            fsync_result = result["probes"]["fsync"]
            self.assertEqual(fsync_result["iters"], 1)
            self.assertGreaterEqual(fsync_result["min_ns"], 0)


if __name__ == "__main__":
    unittest.main()
