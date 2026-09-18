"""`self_durability.py` の単体テスト（Issue #851。venv 不要・標準ライブラリのみ）。

`python3 -m unittest discover scripts/crossdb_bench/tests` で実行する。
"""

from __future__ import annotations

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import self_durability  # noqa: E402


class ParseDurabilityEnvTests(unittest.TestCase):
    def test_none_or_empty_returns_none(self) -> None:
        self.assertIsNone(self_durability.parse_durability_env(None))
        self.assertIsNone(self_durability.parse_durability_env(""))

    def test_accepts_known_tokens(self) -> None:
        self.assertEqual(self_durability.parse_durability_env("immediate"), "immediate")
        self.assertEqual(self_durability.parse_durability_env("none"), "none")

    def test_rejects_unknown_token(self) -> None:
        with self.assertRaises(ValueError):
            self_durability.parse_durability_env("bogus")

    def test_rejects_wrong_case(self) -> None:
        with self.assertRaises(ValueError):
            self_durability.parse_durability_env("Immediate")
        with self.assertRaises(ValueError):
            self_durability.parse_durability_env("NONE")

    def test_rejects_whitespace_variants(self) -> None:
        with self.assertRaises(ValueError):
            self_durability.parse_durability_env(" none")
        with self.assertRaises(ValueError):
            self_durability.parse_durability_env("none ")


class DurabilityExtraArgsTests(unittest.TestCase):
    def test_none_token_and_immediate_produce_no_extra_args(self) -> None:
        self.assertEqual(self_durability.durability_extra_args(None), [])
        self.assertEqual(self_durability.durability_extra_args("immediate"), [])

    def test_none_durability_adds_cli_flag(self) -> None:
        self.assertEqual(
            self_durability.durability_extra_args("none"), ["--durability", "none"]
        )


class WarningDetectionTests(unittest.TestCase):
    def test_warning_expected_only_for_none(self) -> None:
        self.assertFalse(self_durability.warning_line_expected(None))
        self.assertFalse(self_durability.warning_line_expected("immediate"))
        self.assertTrue(self_durability.warning_line_expected("none"))

    def test_has_durability_warning_detects_marker(self) -> None:
        log_with_warning = (
            "env: os=macos arch=aarch64\n"
            "wire-server: WARNING: --durability none selected; commit success "
            "responses do not guarantee data survives a process crash or power "
            "loss until a later durable commit\n"
            "listening on 127.0.0.1:15439\n"
        )
        log_without_warning = "env: os=macos arch=aarch64\nlistening on 127.0.0.1:15439\n"
        self.assertTrue(self_durability.has_durability_warning(log_with_warning))
        self.assertFalse(self_durability.has_durability_warning(log_without_warning))


class VerifyArmIdentityTests(unittest.TestCase):
    def test_matching_arms_do_not_raise(self) -> None:
        self_durability.verify_arm_identity(None, "listening on 127.0.0.1:15439\n")
        self_durability.verify_arm_identity("immediate", "listening on 127.0.0.1:15439\n")
        self_durability.verify_arm_identity(
            "none",
            "wire-server: WARNING: --durability none selected\nlistening\n",
        )

    def test_missing_expected_warning_raises(self) -> None:
        with self.assertRaises(RuntimeError):
            self_durability.verify_arm_identity("none", "listening on 127.0.0.1:15439\n")

    def test_unexpected_warning_raises(self) -> None:
        with self.assertRaises(RuntimeError):
            self_durability.verify_arm_identity(
                None,
                "wire-server: WARNING: --durability none selected\nlistening\n",
            )


class EnvTokenForMetaTests(unittest.TestCase):
    def test_none_token_marks_default(self) -> None:
        self.assertEqual(self_durability.env_token_for_meta(None), "immediate (default)")

    def test_explicit_tokens_pass_through(self) -> None:
        self.assertEqual(self_durability.env_token_for_meta("immediate"), "immediate")
        self.assertEqual(self_durability.env_token_for_meta("none"), "none")


class DurabilitySourceTests(unittest.TestCase):
    def test_reads_from_process_environment(self) -> None:
        saved = os.environ.pop(self_durability.ENV, None)
        try:
            self.assertIsNone(self_durability.durability_source())
            os.environ[self_durability.ENV] = "none"
            self.assertEqual(self_durability.durability_source(), "none")
        finally:
            if saved is None:
                os.environ.pop(self_durability.ENV, None)
            else:
                os.environ[self_durability.ENV] = saved


if __name__ == "__main__":
    unittest.main()
