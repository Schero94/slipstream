"""Run verified coding tasks against one local OpenAI-compatible model endpoint."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
from tempfile import TemporaryDirectory
import threading
from typing import Callable
from uuid import UUID

from bench.m0a.coding_telemetry import (
    TelemetryError,
    enforce_coding_gate,
    parse_response_timings,
)
from bench.m0a.coding_workload import WorkloadTask, load_workload
from bench.m0a.smoke_server import SmokeError, _json_request
from bench.m0a.start_session import _write_json_atomic


DEFAULT_MANIFEST = Path(__file__).parent / "workloads" / "smoke.json"
JsonRequest = Callable[[str, dict[str, object], float], dict[str, object]]
SINGLE_FENCE = re.compile(
    r"\s*```(?:python|json)?[ \t]*\n(?P<body>.*?)\n```\s*",
    re.DOTALL,
)


class CodingRunError(RuntimeError):
    """Raised when a coding workload cannot be executed safely."""


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _default_request(url: str, body: dict[str, object], timeout: float) -> dict[str, object]:
    return _json_request(url, body, timeout)


def _read_rss_kb(pid: int) -> int:
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        capture_output=True,
        text=True,
        check=False,
    )
    try:
        return int(result.stdout.strip())
    except ValueError:
        return 0


def _monitor_pid_rss(pid: int, stop: threading.Event, maximum: list[int]) -> None:
    while not stop.wait(0.1):
        maximum[0] = max(maximum[0], _read_rss_kb(pid))


def _response_content(response: dict[str, object]) -> str:
    choices = response.get("choices")
    if not isinstance(choices, list) or len(choices) != 1:
        raise CodingRunError("chat response must contain exactly one choice")
    choice = choices[0]
    if not isinstance(choice, dict) or not isinstance(choice.get("message"), dict):
        raise CodingRunError("chat choice has no message")
    content = choice["message"].get("content")
    if not isinstance(content, str) or not content.strip():
        raise CodingRunError("chat message has no content")
    fenced = SINGLE_FENCE.fullmatch(content)
    if fenced is not None:
        content = fenced.group("body")
    elif "```" in content:
        raise CodingRunError("model output contains mixed Markdown and code")
    return content.strip() + "\n"


def _write_model_output(task: WorkloadTask, workdir: Path, content: str) -> None:
    destination = workdir / task.output_path
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.resolve().is_relative_to(workdir.resolve()):
        raise CodingRunError("model output path escapes task directory")
    if task.response_kind == "json":
        try:
            parsed = json.loads(content)
        except json.JSONDecodeError as error:
            raise CodingRunError(f"model output is not valid JSON: {error}") from error
        if not isinstance(parsed, dict):
            raise CodingRunError("JSON model output must be an object")
        destination.write_text(
            json.dumps(parsed, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
    else:
        destination.write_text(content, encoding="utf-8")


def _run_task(
    task: WorkloadTask,
    *,
    endpoint: str,
    model: str,
    output_dir: Path,
    request_json: JsonRequest,
    request_timeout: float,
    verifier_timeout: float,
) -> dict[str, object]:
    result: dict[str, object] = {
        "task_id": task.task_id,
        "passed": False,
        "error": None,
        "telemetry": None,
        "verifier_exit_code": None,
        "verifier_stdout": "",
        "verifier_stderr": "",
    }
    response: dict[str, object] | None = None
    try:
        response = request_json(
            endpoint,
            {
                "model": model,
                "messages": [
                    {
                        "role": "system",
                        "content": "You are a precise coding engine. Follow the output contract exactly.",
                    },
                    {"role": "user", "content": task.prompt},
                ],
                "temperature": 0,
                "seed": 42,
                "max_tokens": 512,
                "stream": False,
                "timings_per_token": False,
                "chat_template_kwargs": {"enable_thinking": False},
            },
            request_timeout,
        )
        _write_json_atomic(output_dir / f"response-{task.task_id}.json", response)
        telemetry = parse_response_timings(response)
        content = _response_content(response)
        with TemporaryDirectory(prefix=f"task-{task.task_id}-", dir=output_dir) as tmp:
            workdir = Path(tmp)
            shutil.copytree(task.fixture_dir, workdir, dirs_exist_ok=True)
            _write_model_output(task, workdir, content)
            try:
                verifier = subprocess.run(
                    list(task.verifier),
                    cwd=workdir,
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=verifier_timeout,
                )
            except subprocess.TimeoutExpired:
                result["error"] = "verifier timeout"
            else:
                result["verifier_exit_code"] = verifier.returncode
                result["verifier_stdout"] = verifier.stdout
                result["verifier_stderr"] = verifier.stderr
                if verifier.returncode == 0:
                    result["passed"] = True
                else:
                    result["error"] = "verifier failed"
        result["telemetry"] = telemetry
    except (CodingRunError, TelemetryError, SmokeError, OSError, ValueError) as error:
        result["error"] = str(error)
    finally:
        _write_json_atomic(output_dir / f"result-{task.task_id}.json", result)
    return result


def run_workload(
    manifest: Path,
    output_dir: Path,
    *,
    base_url: str,
    model: str,
    session_id: UUID,
    server_pid: int,
    request_json: JsonRequest = _default_request,
    request_timeout: float = 900,
    verifier_timeout: float = 60,
) -> dict[str, object]:
    """Run all declared tasks and atomically persist a fail-closed report."""

    tasks = load_workload(manifest)
    output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    os.chmod(output_dir, 0o700)
    endpoint = base_url.rstrip("/") + "/chat/completions"
    peak_rss = [_read_rss_kb(server_pid)]
    stop_monitor = threading.Event()
    monitor = threading.Thread(
        target=_monitor_pid_rss,
        args=(server_pid, stop_monitor, peak_rss),
        daemon=True,
    )
    monitor.start()
    started_at = _utc_now()
    try:
        results = [
            _run_task(
                task,
                endpoint=endpoint,
                model=model,
                output_dir=output_dir,
                request_json=request_json,
                request_timeout=request_timeout,
                verifier_timeout=verifier_timeout,
            )
            for task in tasks
        ]
    finally:
        stop_monitor.set()
        monitor.join(timeout=2)

    reasons = [f"task@{result['task_id']}" for result in results if not result["passed"]]
    points = []
    for result in results:
        telemetry = result.get("telemetry")
        if isinstance(telemetry, dict):
            points.append(
                {
                    "decode_tokens_per_second": telemetry["decode_tokens_per_second"],
                    "decoded_tokens": telemetry["decoded_tokens"],
                    "peak_rss_kb": peak_rss[0],
                }
            )
    try:
        performance = enforce_coding_gate(points)
    except TelemetryError:
        performance = {"passed": False, "reasons": ["telemetry"]}
    reasons.extend(str(reason) for reason in performance["reasons"])
    decision = {
        "passed": not reasons,
        "reasons": reasons,
        "warnings": list(performance.get("warnings", [])),
        "mean_decode_tokens_per_second": performance.get(
            "mean_decode_tokens_per_second"
        ),
        "task_passes": sum(bool(result["passed"]) for result in results),
        "task_count": len(results),
    }
    report: dict[str, object] = {
        "schema": 1,
        "session_id": str(session_id),
        "model": model,
        "base_url": base_url,
        "manifest": str(manifest.resolve()),
        "started_at": started_at,
        "ended_at": _utc_now(),
        "server_pid": server_pid,
        "peak_rss_kb": peak_rss[0],
        "tasks": results,
        "decision": decision,
    }
    _write_json_atomic(output_dir / "report.json", report)
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--base-url", default="http://127.0.0.1:8080/v1")
    parser.add_argument("--model", default="peregrine-m0")
    parser.add_argument("--session-id", required=True, type=UUID)
    parser.add_argument("--server-pid", required=True, type=int)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = run_workload(
            args.manifest,
            args.output_dir,
            base_url=args.base_url,
            model=args.model,
            session_id=args.session_id,
            server_pid=args.server_pid,
        )
    except (CodingRunError, OSError, ValueError) as error:
        print(f"coding workload failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["decision"]["passed"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
