#!/usr/bin/env python3
"""TASK-118（回答正答率評価基盤）: LLM 採点による回答正答率の評価スクリプト。

役割・呼び出し文脈:
    vector-db（本体エンジン）の検索結果を LLM のコンテキストとして渡した際の
    回答正答率を測定するフォローアップ評価ツール。TASK-118（MS-4・基盤・工程管理）に
    対応する評価「基盤」であり、正式な測定・結果レポートの確定はオーナーによる評価設計
    （採点基準・サンプル数）承認後に行う。データ・結果本文は private spec 側
    （ポインタ表記: docs/spec/03-poc/eval-base/）にあるため、本スクリプトは
    設定ファイル経由でパスを受け取るのみで、spec 本文・評価データを本体に含めない。

外部依存: Python 標準ライブラリのみ（依存最小方針。ユーザー承認不要）。

呼び出し方法:
    python3 scripts/eval/answer_accuracy.py --config scripts/eval/config.example.json [--dry-run]

fail-closed の方針:
    - 設定の必須キー欠落・型不正・上限超過は即エラー終了する
    - 採点出力が想定ラベル（CORRECT / INCORRECT）以外の場合は「判定不能」とし、
      正答率の分子には計上しない（不正解側に倒す）
    - LLM エンドポイントはリダイレクトを追従しない（SSRF 対策）
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import random
import re
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

# 上限値。設定ファイルでこれを超える値を指定しても切り詰める（DoS・暴走課金対策）。
HARD_MAX_SAMPLE_SIZE = 200
HARD_MAX_TIMEOUT_SECONDS = 120
HARD_MAX_RETRIES = 5
HARD_MAX_RESPONSE_BYTES = 1_000_000
# リトライ間隔（秒）。連続リトライによる相手側への負荷集中を避けるための指数バックオフの初期値。
RETRY_BACKOFF_BASE_SECONDS = 0.5

REQUIRED_CONFIG_KEYS = (
    "llm_endpoint",
    "model",
    "api_key_env",
    "data_path",
    "scoring_prompt_path",
    "sample_size",
    "output_dir",
)

# 採点出力の厳格パース対象ラベル。これ以外は "UNKNOWN" として不正解側に倒す。
LABEL_CORRECT = "CORRECT"
LABEL_INCORRECT = "INCORRECT"
LABEL_UNKNOWN = "UNKNOWN"


class ConfigError(ValueError):
    """設定ファイルの検証エラー（fail-closed で即終了させるための専用例外）。"""


class DatasetError(ValueError):
    """評価データ読み込みエラー（private submodule 未取得等）。"""


@dataclass(frozen=True)
class EvalConfig:
    """検証済みの実行設定。load_config() のみが生成する（未検証値を後段へ渡さない）。"""

    llm_endpoint: str
    model: str
    api_key_env: str
    data_path: Path
    scoring_prompt_path: Path
    sample_size: int
    max_sample_size: int
    timeout_seconds: int
    max_retries: int
    max_response_bytes: int
    output_dir: Path


@dataclass
class SampleResult:
    """1 サンプルの評価結果（回答生成 + 採点）。"""

    sample_id: str
    question: str
    generated_answer: str
    label: str
    reason: str


@dataclass
class EvalReport:
    """全サンプルの集計結果。Markdown レポート生成の入力。"""

    results: list[SampleResult] = field(default_factory=list)
    dry_run: bool = False

    @property
    def total(self) -> int:
        return len(self.results)

    @property
    def correct(self) -> int:
        return sum(1 for r in self.results if r.label == LABEL_CORRECT)

    @property
    def unknown(self) -> int:
        return sum(1 for r in self.results if r.label == LABEL_UNKNOWN)

    @property
    def accuracy(self) -> float:
        if self.total == 0:
            return 0.0
        return self.correct / self.total

    @property
    def unknown_rate(self) -> float:
        if self.total == 0:
            return 0.0
        return self.unknown / self.total


def _require_type(value: Any, expected: type, key: str) -> Any:
    if not isinstance(value, expected) or isinstance(value, bool) and expected is not bool:
        raise ConfigError(f"config key '{key}' must be of type {expected.__name__}")
    return value


def load_config(config_path: Path) -> EvalConfig:
    """設定ファイルを読み込み検証する。必須キー欠落・型不正・上限超過は ConfigError で即終了させる。"""
    try:
        raw_text = config_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise ConfigError(f"failed to read config file: {config_path} ({exc})") from exc

    try:
        raw: dict[str, Any] = json.loads(raw_text)
    except json.JSONDecodeError as exc:
        raise ConfigError(f"config file is not valid JSON: {exc}") from exc

    if not isinstance(raw, dict):
        raise ConfigError("config root must be a JSON object")

    missing = [key for key in REQUIRED_CONFIG_KEYS if key not in raw]
    if missing:
        raise ConfigError(f"config is missing required keys: {', '.join(missing)}")

    llm_endpoint = _require_type(raw["llm_endpoint"], str, "llm_endpoint")
    parsed_endpoint = urlparse(llm_endpoint)
    if parsed_endpoint.scheme not in ("http", "https"):
        raise ConfigError("llm_endpoint must use http or https scheme")

    model = _require_type(raw["model"], str, "model")
    api_key_env = _require_type(raw["api_key_env"], str, "api_key_env")
    data_path = Path(_require_type(raw["data_path"], str, "data_path"))
    scoring_prompt_path = Path(_require_type(raw["scoring_prompt_path"], str, "scoring_prompt_path"))

    sample_size = _require_type(raw["sample_size"], int, "sample_size")
    if sample_size <= 0:
        raise ConfigError("sample_size must be a positive integer")

    max_sample_size = _require_type(
        raw.get("max_sample_size", HARD_MAX_SAMPLE_SIZE), int, "max_sample_size"
    )
    max_sample_size = min(max_sample_size, HARD_MAX_SAMPLE_SIZE)
    if sample_size > max_sample_size:
        raise ConfigError(
            f"sample_size ({sample_size}) exceeds max_sample_size ({max_sample_size})"
        )

    timeout_seconds = _require_type(raw.get("timeout_seconds", 30), int, "timeout_seconds")
    if timeout_seconds <= 0:
        raise ConfigError("timeout_seconds must be a positive integer")
    timeout_seconds = min(timeout_seconds, HARD_MAX_TIMEOUT_SECONDS)

    max_retries = _require_type(raw.get("max_retries", 2), int, "max_retries")
    if max_retries < 0:
        raise ConfigError("max_retries must be zero or a positive integer")
    max_retries = min(max_retries, HARD_MAX_RETRIES)

    max_response_bytes = _require_type(
        raw.get("max_response_bytes", 65536), int, "max_response_bytes"
    )
    if max_response_bytes <= 0:
        raise ConfigError("max_response_bytes must be a positive integer")
    max_response_bytes = min(max_response_bytes, HARD_MAX_RESPONSE_BYTES)

    output_dir = Path(_require_type(raw["output_dir"], str, "output_dir"))

    return EvalConfig(
        llm_endpoint=llm_endpoint,
        model=model,
        api_key_env=api_key_env,
        data_path=data_path,
        scoring_prompt_path=scoring_prompt_path,
        sample_size=sample_size,
        max_sample_size=max_sample_size,
        timeout_seconds=timeout_seconds,
        max_retries=max_retries,
        max_response_bytes=max_response_bytes,
        output_dir=output_dir,
    )


def load_dataset(data_path: Path) -> list[dict[str, str]]:
    """評価データ（JSONL）を読み込む。private submodule 未取得時は明示エラーで終了する。"""
    if not data_path.exists():
        raise DatasetError(
            f"dataset not found at {data_path}. "
            "This likely means the private spec submodule (docs/spec) is not checked out, "
            "or data_path in the config does not point to a valid file."
        )

    records: list[dict[str, str]] = []
    with data_path.open(encoding="utf-8") as f:
        for line_no, line in enumerate(f, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as exc:
                raise DatasetError(f"invalid JSON at {data_path}:{line_no}: {exc}") from exc
            if not isinstance(record, dict):
                raise DatasetError(f"record at {data_path}:{line_no} must be a JSON object")
            for key in ("id", "question", "context", "expected_answer"):
                if key not in record:
                    raise DatasetError(f"record at {data_path}:{line_no} missing key '{key}'")
                if not isinstance(record[key], str):
                    raise DatasetError(
                        f"record at {data_path}:{line_no} field '{key}' must be a string"
                    )
            records.append(record)

    if not records:
        raise DatasetError(f"dataset at {data_path} contains no records")

    return records


def sample_dataset(
    records: list[dict[str, str]], sample_size: int, seed: int = 0
) -> list[dict[str, str]]:
    """データセットから決定的にサンプリングする（再現性のため固定シードを既定とする）。"""
    if sample_size >= len(records):
        return list(records)
    rng = random.Random(seed)
    return rng.sample(records, sample_size)


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """SSRF 対策: 設定で明示指定されたエンドポイント以外へのリダイレクト追従を禁止する。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D102
        raise urllib.error.HTTPError(newurl, code, "redirect not followed (SSRF guard)", headers, fp)


def _post_json(
    endpoint: str,
    payload: dict[str, Any],
    api_key: str | None,
    timeout_seconds: int,
    max_retries: int,
    max_response_bytes: int,
) -> dict[str, Any]:
    """LLM エンドポイントへ JSON POST する。タイムアウト・リトライ上限・レスポンスサイズ上限を厳守する。"""
    parsed = urlparse(endpoint)
    if parsed.scheme not in ("http", "https"):
        raise ValueError("endpoint must use http or https scheme")

    body = json.dumps(payload).encode("utf-8")
    headers = {"Content-Type": "application/json"}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"

    opener = urllib.request.build_opener(_NoRedirectHandler)

    last_error: Exception | None = None
    for attempt in range(max_retries + 1):
        req = urllib.request.Request(endpoint, data=body, headers=headers, method="POST")
        try:
            with opener.open(req, timeout=timeout_seconds) as resp:
                raw = resp.read(max_response_bytes + 1)
                if len(raw) > max_response_bytes:
                    raise ValueError("response exceeded max_response_bytes limit")
                return json.loads(raw.decode("utf-8"))
        except (
            urllib.error.URLError,
            http.client.HTTPException,
            TimeoutError,
            ValueError,
            OSError,
        ) as exc:
            # OSError を追加で捕捉する: ConnectionResetError・ssl.SSLError 等の
            # ソケット/TLS 例外は urllib.error.URLError でラップされず素通りするため、
            # ここに含めないと一時的な接続断がリトライされず即 UNKNOWN 記録に落ちる。
            last_error = exc
            if attempt < max_retries:
                time.sleep(RETRY_BACKOFF_BASE_SECONDS * (2**attempt))
            continue

    raise RuntimeError(f"LLM request failed after {max_retries + 1} attempts: {last_error}")


def generate_answer(config: EvalConfig, api_key: str | None, question: str, context: str) -> str:
    """質問＋上位コンテキストを LLM に送って回答を生成させる（回答生成フェーズ）。"""
    payload = {
        "model": config.model,
        "messages": [
            {
                "role": "user",
                "content": f"Context:\n{context}\n\nQuestion: {question}\nAnswer concisely.",
            }
        ],
    }
    response = _post_json(
        config.llm_endpoint,
        payload,
        api_key,
        config.timeout_seconds,
        config.max_retries,
        config.max_response_bytes,
    )
    return _extract_message_text(response)


def score_answer(
    config: EvalConfig,
    api_key: str | None,
    scoring_prompt: str,
    question: str,
    expected_answer: str,
    generated_answer: str,
) -> tuple[str, str]:
    """採点プロンプトで LLM 採点する（採点フェーズ）。想定ラベル以外は判定不能として返す。"""
    payload = {
        "model": config.model,
        "messages": [
            {"role": "system", "content": scoring_prompt},
            {
                "role": "user",
                "content": (
                    f"Question: {question}\n"
                    f"Expected answer: {expected_answer}\n"
                    f"Candidate answer: {generated_answer}"
                ),
            },
        ],
    }
    response = _post_json(
        config.llm_endpoint,
        payload,
        api_key,
        config.timeout_seconds,
        config.max_retries,
        config.max_response_bytes,
    )
    text = _extract_message_text(response)
    return _parse_score_label(text)


def _extract_message_text(response: dict[str, Any]) -> str:
    """OpenAI 互換レスポンスからテキストを取り出す。想定形状でなければ空文字を返す（呼び出し側で判定不能扱い）。

    untrusted な外部（LLM API）からの応答は形状を一切信頼せず、辞書アクセス・添字アクセスで
    発生し得る例外（AttributeError/KeyError/TypeError/IndexError）をすべて捕捉して fail-closed で
    空文字に倒す（想定外形状で未処理の traceback により異常終了させない）。
    """
    try:
        choices = response.get("choices", [])
        if not isinstance(choices, list) or not choices:
            return ""
        first_choice = choices[0]
        if not isinstance(first_choice, dict):
            return ""
        message = first_choice.get("message", {})
        if not isinstance(message, dict):
            return ""
        content = message.get("content", "")
        return content if isinstance(content, str) else ""
    except (AttributeError, KeyError, TypeError, IndexError):
        return ""


_LABEL_TOKEN_RE = re.compile(r"^[A-Za-z]+")


def _parse_score_label(text: str) -> tuple[str, str]:
    """採点出力を厳格パースする。先頭行の先頭トークンが CORRECT / INCORRECT に完全一致しない場合は
    UNKNOWN（不正解側）として返す。

    'CORRECTED' や 'CORRECTNESS is uncertain' 等、CORRECT を prefix に持つが意味の異なる
    出力を誤って CORRECT 判定しないよう、先頭の英字トークンのみを取り出して等価比較する
    （前方一致 startswith ではなく完全一致。fail-closed: 一致しなければ正答率の分子に計上しない）。
    """
    if not text or not text.strip():
        return LABEL_UNKNOWN, "empty response from grader"

    first_line = text.strip().splitlines()[0].strip()
    match = _LABEL_TOKEN_RE.match(first_line)
    token = match.group(0).upper() if match else ""
    if token == LABEL_CORRECT:
        return LABEL_CORRECT, first_line
    if token == LABEL_INCORRECT:
        return LABEL_INCORRECT, first_line
    return LABEL_UNKNOWN, f"unparseable grader output: {first_line[:200]!r}"


def run_evaluation(config: EvalConfig, dry_run: bool) -> EvalReport:
    """設定検証済みの EvalConfig を受けて評価を実行する（dry_run 時は LLM を呼ばない配線検証）。"""
    records = load_dataset(config.data_path)
    sampled = sample_dataset(records, config.sample_size)

    report = EvalReport(dry_run=dry_run)

    # 採点プロンプトファイルは dry-run 経路でも読み込む（1 回だけ）。ここを省略すると
    # README が「配線検証」と説明する dry-run 経路が、本番実行時にのみ露見する
    # scoring_prompt_path の欠落・読み取り不可を検知できなくなる。読み込みをここに一本化し、
    # 後段で再度 read_text() する二重読み込み（＝ガードされない 2 回目の OSError 経路）を作らない。
    try:
        scoring_prompt = config.scoring_prompt_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise DatasetError(f"failed to read scoring prompt: {config.scoring_prompt_path} ({exc})") from exc

    if dry_run:
        for record in sampled:
            report.results.append(
                SampleResult(
                    sample_id=record["id"],
                    question=record["question"],
                    generated_answer="(dry-run: LLM was not called)",
                    label=LABEL_UNKNOWN,
                    reason="dry-run",
                )
            )
        return report

    # api_key_env は必須設定キーであり認証利用が前提の設計。実際の環境変数が
    # 欠落したまま続行すると全サンプルが認証エラーで失敗し、「判定不能率 100%」の
    # レポートだけが残って原因（キー未設定 or モデル不調）が運用者に分からなくなる。
    # fail-closed のため、LLM 呼び出し前に明示チェックして分かりやすいエラーで終了する。
    if config.api_key_env not in os.environ or not os.environ[config.api_key_env]:
        raise RuntimeError(
            f"environment variable '{config.api_key_env}' (config.api_key_env) is not set or empty. "
            "Set it before running a non-dry-run evaluation."
        )
    api_key = os.environ[config.api_key_env]

    for record in sampled:
        # サンプル単位で例外を隔離する。1 サンプルの LLM 呼び出し失敗（リトライ上限到達の
        # RuntimeError に加え、_post_json の except タプルが捕捉しない OSError 系
        # （例: レスポンス読み取り中の ConnectionResetError・ssl.SSLError）も含む）で
        # ループ全体を中断すると、それまでに完了した有料 LLM 呼び出し分の結果が
        # write_report まで届かず失われる。想定外の例外も含めすべて判定不能として記録し
        # 次サンプルへ進む（fail-closed: 失敗サンプルは正答率の分子に計上されない）。
        try:
            generated = generate_answer(config, api_key, record["question"], record["context"])
            label, reason = score_answer(
                config, api_key, scoring_prompt, record["question"], record["expected_answer"], generated
            )
        except Exception as exc:  # noqa: BLE001 - サンプル単位隔離のため意図的に広く捕捉
            generated = ""
            label, reason = LABEL_UNKNOWN, f"sample failed: {exc}"
        report.results.append(
            SampleResult(
                sample_id=record["id"],
                question=record["question"],
                generated_answer=generated,
                label=label,
                reason=reason,
            )
        )

    return report


# レポート本文の肥大化を防ぐための各セル文字数上限（UNKNOWN 時の 200 文字上限と揃える）。
REPORT_CELL_MAX_CHARS = 200


def _escape_markdown_table_cell(text: str) -> str:
    """Markdown テーブルのセル用にエスケープする。'|' はテーブル区切りと衝突し、改行はセルを崩すため置換する。"""
    escaped = text.replace("\\", "\\\\").replace("|", "\\|")
    escaped = escaped.replace("\r\n", " ").replace("\n", " ").replace("\r", " ")
    if len(escaped) > REPORT_CELL_MAX_CHARS:
        escaped = escaped[:REPORT_CELL_MAX_CHARS] + "..."
    return escaped


def render_report_markdown(report: EvalReport, config: EvalConfig) -> str:
    """集計結果を Markdown レポートへ整形する。private データ本文はレポートに含めるが出力先は git 管理外とする。"""
    timestamp = datetime.now(timezone.utc).isoformat()
    lines = [
        "# TASK-118 回答正答率評価レポート",
        "",
        f"- 実行日時 (UTC): {timestamp}",
        f"- dry-run: {report.dry_run}",
        f"- モデル: {config.model}",
        f"- サンプル数: {report.total}",
        f"- 正答率: {report.accuracy:.1%}" if not report.dry_run else "- 正答率: (dry-run のため未計測)",
        f"- 判定不能率: {report.unknown_rate:.1%}" if not report.dry_run else "- 判定不能率: (dry-run のため未計測)",
        "",
        "> 本レポートは暫定評価設計に基づく可能性があります。"
        "正式な結果としての公開・利用はオーナーの評価設計承認後としてください。",
        "",
        "## サンプル別結果",
        "",
        "| ID | ラベル | 理由 |",
        "| --- | --- | --- |",
    ]
    for result in report.results:
        sample_id = _escape_markdown_table_cell(result.sample_id)
        label = _escape_markdown_table_cell(result.label)
        reason = _escape_markdown_table_cell(result.reason)
        lines.append(f"| {sample_id} | {label} | {reason} |")
    lines.append("")
    return "\n".join(lines)


def write_report(report: EvalReport, config: EvalConfig) -> Path:
    """レポートを output_dir（既定 `_/reports/`。git 管理外）へ書き出す。"""
    config.output_dir.mkdir(parents=True, exist_ok=True)
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out_path = config.output_dir / f"answer_accuracy_{timestamp}.md"
    out_path.write_text(render_report_markdown(report, config), encoding="utf-8")
    return out_path


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="TASK-118 answer accuracy evaluation (LLM-graded).")
    parser.add_argument("--config", required=True, type=Path, help="path to evaluation config JSON")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="validate config/dataset and emit a report skeleton without calling the LLM",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])

    try:
        config = load_config(args.config)
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        return 2

    try:
        report = run_evaluation(config, dry_run=args.dry_run)
    except DatasetError as exc:
        print(f"dataset error: {exc}", file=sys.stderr)
        return 3
    except RuntimeError as exc:
        print(f"evaluation error: {exc}", file=sys.stderr)
        return 4

    # write_report は全サンプルの LLM 呼び出し完了後に走る。ここで捕捉しないと
    # output_dir の書き込み不可（権限・親パスに同名ファイル存在等）による OSError が
    # 未処理の traceback となり、run_evaluation 内で丁寧に守った「サンプル単位隔離に
    # よる結果ロス防止」の意図がレポート書き出し段で崩れる。fail-closed で終了コードを返す。
    try:
        out_path = write_report(report, config)
    except OSError as exc:
        print(f"report write error: failed to write report to {config.output_dir} ({exc})", file=sys.stderr)
        return 5
    print(f"report written to {out_path}")
    if not report.dry_run:
        print(f"accuracy: {report.accuracy:.1%} ({report.correct}/{report.total})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
