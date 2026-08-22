"""answer_accuracy.py のユニットテスト。

呼び出し文脈:
    `make test-eval` / `python3 -m unittest discover scripts/eval/tests` から実行される。
    ネットワーク・実 LLM・private spec データを一切使わず、本ファイル配下のダミー
    fixture（新規作成の架空データ）のみで完結させる（AGENTS.md: spec 配下をテストが
    読む構造の禁止に対応）。fail-closed 経路（設定不正・採点不能パース）を重点的に検証する。
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

# scripts/eval/ を import path に追加する（パッケージ化していないスクリプトのため）。
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import answer_accuracy as aa  # noqa: E402


def _valid_config_dict(tmp_dir: Path) -> dict:
    data_path = tmp_dir / "data.jsonl"
    data_path.write_text(
        "\n".join(
            json.dumps(r)
            for r in [
                {"id": "s1", "question": "Q1", "context": "C1", "expected_answer": "A1"},
                {"id": "s2", "question": "Q2", "context": "C2", "expected_answer": "A2"},
            ]
        ),
        encoding="utf-8",
    )
    prompt_path = tmp_dir / "prompt.txt"
    prompt_path.write_text("grade strictly", encoding="utf-8")
    return {
        "llm_endpoint": "http://127.0.0.1:9/v1/chat/completions",
        "model": "dummy",
        "api_key_env": "EVAL_TEST_API_KEY",
        "data_path": str(data_path),
        "scoring_prompt_path": str(prompt_path),
        "sample_size": 2,
        "output_dir": str(tmp_dir / "out"),
    }


class LoadConfigTest(unittest.TestCase):
    def test_valid_config_loads(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(_valid_config_dict(tmp_dir)), encoding="utf-8")

            config = aa.load_config(config_path)

            self.assertEqual(config.sample_size, 2)
            self.assertEqual(config.model, "dummy")

    def test_missing_required_key_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            raw = _valid_config_dict(tmp_dir)
            del raw["llm_endpoint"]
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(raw), encoding="utf-8")

            with self.assertRaises(aa.ConfigError):
                aa.load_config(config_path)

    def test_invalid_scheme_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            raw = _valid_config_dict(tmp_dir)
            raw["llm_endpoint"] = "ftp://example.invalid/x"
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(raw), encoding="utf-8")

            with self.assertRaises(aa.ConfigError):
                aa.load_config(config_path)

    def test_sample_size_exceeding_max_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            raw = _valid_config_dict(tmp_dir)
            raw["sample_size"] = 10
            raw["max_sample_size"] = 5
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(raw), encoding="utf-8")

            with self.assertRaises(aa.ConfigError):
                aa.load_config(config_path)

    def test_hard_cap_overrides_generous_max_sample_size(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            raw = _valid_config_dict(tmp_dir)
            raw["max_sample_size"] = 999999
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(raw), encoding="utf-8")

            config = aa.load_config(config_path)

            self.assertEqual(config.max_sample_size, aa.HARD_MAX_SAMPLE_SIZE)

    def test_not_json_object_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            config_path = tmp_dir / "config.json"
            config_path.write_text("[1, 2, 3]", encoding="utf-8")

            with self.assertRaises(aa.ConfigError):
                aa.load_config(config_path)


class LoadDatasetTest(unittest.TestCase):
    def test_missing_path_raises_dataset_error(self):
        with self.assertRaises(aa.DatasetError):
            aa.load_dataset(Path("/nonexistent/does-not-exist.jsonl"))

    def test_valid_dataset_parses(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_path = Path(tmp) / "data.jsonl"
            data_path.write_text(
                json.dumps({"id": "a", "question": "q", "context": "c", "expected_answer": "e"}),
                encoding="utf-8",
            )
            records = aa.load_dataset(data_path)
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0]["id"], "a")

    def test_missing_field_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_path = Path(tmp) / "data.jsonl"
            data_path.write_text(json.dumps({"id": "a", "question": "q"}), encoding="utf-8")
            with self.assertRaises(aa.DatasetError):
                aa.load_dataset(data_path)

    def test_empty_dataset_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_path = Path(tmp) / "data.jsonl"
            data_path.write_text("", encoding="utf-8")
            with self.assertRaises(aa.DatasetError):
                aa.load_dataset(data_path)


class SampleDatasetTest(unittest.TestCase):
    def test_sample_smaller_than_population(self):
        records = [{"id": str(i)} for i in range(10)]
        sampled = aa.sample_dataset(records, 3, seed=1)
        self.assertEqual(len(sampled), 3)

    def test_sample_size_ge_population_returns_all(self):
        records = [{"id": str(i)} for i in range(3)]
        sampled = aa.sample_dataset(records, 10, seed=1)
        self.assertEqual(len(sampled), 3)

    def test_deterministic_with_fixed_seed(self):
        records = [{"id": str(i)} for i in range(10)]
        first = aa.sample_dataset(records, 4, seed=42)
        second = aa.sample_dataset(records, 4, seed=42)
        self.assertEqual(first, second)


class ParseScoreLabelTest(unittest.TestCase):
    def test_correct_label_parsed(self):
        label, _ = aa._parse_score_label("CORRECT: matches expected answer")
        self.assertEqual(label, aa.LABEL_CORRECT)

    def test_incorrect_label_parsed(self):
        label, _ = aa._parse_score_label("INCORRECT: does not match")
        self.assertEqual(label, aa.LABEL_INCORRECT)

    def test_lowercase_label_parsed_case_insensitively(self):
        label, _ = aa._parse_score_label("correct, good answer")
        self.assertEqual(label, aa.LABEL_CORRECT)

    def test_unrecognized_output_is_unknown_fail_closed(self):
        label, reason = aa._parse_score_label("The answer seems plausible but I am not sure.")
        self.assertEqual(label, aa.LABEL_UNKNOWN)
        self.assertIn("unparseable", reason)

    def test_empty_output_is_unknown_fail_closed(self):
        label, reason = aa._parse_score_label("")
        self.assertEqual(label, aa.LABEL_UNKNOWN)


class EvalReportAggregationTest(unittest.TestCase):
    def test_accuracy_and_unknown_rate(self):
        report = aa.EvalReport(
            results=[
                aa.SampleResult("1", "q1", "a1", aa.LABEL_CORRECT, "r"),
                aa.SampleResult("2", "q2", "a2", aa.LABEL_INCORRECT, "r"),
                aa.SampleResult("3", "q3", "a3", aa.LABEL_UNKNOWN, "r"),
                aa.SampleResult("4", "q4", "a4", aa.LABEL_CORRECT, "r"),
            ]
        )
        self.assertEqual(report.total, 4)
        self.assertEqual(report.correct, 2)
        self.assertEqual(report.unknown, 1)
        self.assertAlmostEqual(report.accuracy, 0.5)
        self.assertAlmostEqual(report.unknown_rate, 0.25)

    def test_empty_report_does_not_divide_by_zero(self):
        report = aa.EvalReport(results=[])
        self.assertEqual(report.accuracy, 0.0)
        self.assertEqual(report.unknown_rate, 0.0)


class DryRunEvaluationTest(unittest.TestCase):
    def test_dry_run_does_not_call_llm_and_produces_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(_valid_config_dict(tmp_dir)), encoding="utf-8")
            config = aa.load_config(config_path)

            report = aa.run_evaluation(config, dry_run=True)

            self.assertTrue(report.dry_run)
            self.assertEqual(report.total, 2)
            for result in report.results:
                self.assertEqual(result.label, aa.LABEL_UNKNOWN)

            out_path = aa.write_report(report, config)
            self.assertTrue(out_path.exists())
            content = out_path.read_text(encoding="utf-8")
            self.assertIn("dry-run", content)


class ExtractMessageTextTest(unittest.TestCase):
    def test_extracts_content_from_openai_shape(self):
        response = {"choices": [{"message": {"content": "hello"}}]}
        self.assertEqual(aa._extract_message_text(response), "hello")

    def test_missing_choices_returns_empty_string(self):
        self.assertEqual(aa._extract_message_text({}), "")

    def test_non_string_content_returns_empty_string(self):
        response = {"choices": [{"message": {"content": {"unexpected": "shape"}}}]}
        self.assertEqual(aa._extract_message_text(response), "")

    def test_non_dict_choice_element_returns_empty_string(self):
        # choices が非空でも要素が dict でない想定外形状（KeyError/TypeError の再現条件）。
        self.assertEqual(aa._extract_message_text({"choices": [None]}), "")
        self.assertEqual(aa._extract_message_text({"choices": [42]}), "")
        self.assertEqual(aa._extract_message_text({"choices": ["x"]}), "")

    def test_non_dict_message_returns_empty_string(self):
        self.assertEqual(aa._extract_message_text({"choices": [{"message": "not-a-dict"}]}), "")

    def test_choices_not_a_list_returns_empty_string(self):
        self.assertEqual(aa._extract_message_text({"choices": {"unexpected": "shape"}}), "")


class DryRunScoringPromptValidationTest(unittest.TestCase):
    def test_dry_run_raises_when_scoring_prompt_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            raw = _valid_config_dict(tmp_dir)
            raw["scoring_prompt_path"] = str(tmp_dir / "does-not-exist.txt")
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(raw), encoding="utf-8")
            config = aa.load_config(config_path)

            with self.assertRaises(aa.DatasetError):
                aa.run_evaluation(config, dry_run=True)


class RunEvaluationPartialFailureTest(unittest.TestCase):
    def test_sample_failure_is_isolated_and_report_still_produced(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            config_path = tmp_dir / "config.json"
            config_path.write_text(json.dumps(_valid_config_dict(tmp_dir)), encoding="utf-8")
            config = aa.load_config(config_path)

            call_count = {"n": 0}

            def fake_generate_answer(_config, _api_key, _question, _context):
                call_count["n"] += 1
                if call_count["n"] == 1:
                    raise RuntimeError("LLM request failed after 3 attempts: boom")
                return "generated"

            def fake_score_answer(*_args, **_kwargs):
                return aa.LABEL_CORRECT, "matches"

            original_generate = aa.generate_answer
            original_score = aa.score_answer
            aa.generate_answer = fake_generate_answer
            aa.score_answer = fake_score_answer
            try:
                report = aa.run_evaluation(config, dry_run=False)
            finally:
                aa.generate_answer = original_generate
                aa.score_answer = original_score

            # 1 サンプル目が失敗しても 2 サンプル目まで到達し、結果は全件（部分結果含む）記録される。
            self.assertEqual(report.total, 2)
            self.assertEqual(report.results[0].label, aa.LABEL_UNKNOWN)
            self.assertIn("sample failed", report.results[0].reason)
            self.assertEqual(report.results[1].label, aa.LABEL_CORRECT)

            out_path = aa.write_report(report, config)
            self.assertTrue(out_path.exists())


class RenderReportMarkdownEscapingTest(unittest.TestCase):
    def test_pipe_and_newline_in_reason_do_not_break_table(self):
        report = aa.EvalReport(
            results=[
                aa.SampleResult("s1", "q1", "a1", aa.LABEL_INCORRECT, "reason with | pipe\nand newline"),
            ]
        )
        rendered = aa.render_report_markdown(
            report,
            aa.EvalConfig(
                llm_endpoint="http://127.0.0.1:9/v1/chat/completions",
                model="dummy",
                api_key_env="EVAL_TEST_API_KEY",
                data_path=Path("data.jsonl"),
                scoring_prompt_path=Path("prompt.txt"),
                sample_size=1,
                max_sample_size=1,
                timeout_seconds=30,
                max_retries=2,
                max_response_bytes=65536,
                output_dir=Path("out"),
            ),
        )
        table_lines = [line for line in rendered.splitlines() if line.startswith("| s1")]
        self.assertEqual(len(table_lines), 1)
        self.assertNotIn("\n", table_lines[0])
        self.assertIn("\\|", table_lines[0])

    def test_long_reason_is_truncated(self):
        long_reason = "x" * 500
        escaped = aa._escape_markdown_table_cell(long_reason)
        self.assertLessEqual(len(escaped), aa.REPORT_CELL_MAX_CHARS + len("..."))


if __name__ == "__main__":
    unittest.main()
