"""Run one bounded local coding-agent episode against an OpenAI endpoint."""

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
import time
from typing import Callable, Mapping
from uuid import UUID

from bench.m0a.agentic_episode import Episode, load_episodes
from bench.m0a.agentic_tools import AgenticSandbox, PATCH_TARGET, ToolError
from bench.m0a.coding_telemetry import (
    TelemetryError,
    enforce_coding_gate,
    parse_response_timings,
)
from bench.m0a.run_coding_workload import (
    CodingRunError,
    _monitor_pid_rss,
    _read_rss_kb,
    _response_content,
)
from bench.m0a.smoke_server import SmokeError, _json_request
from bench.m0a.start_session import _write_json_atomic


JsonRequest = Callable[[str, dict[str, object], float], dict[str, object]]
TOOLS = {"list_files", "read_file", "apply_patch", "write_file", "run_tests", "finish"}
REQUIRED_TOOL_FLOW = {"read_file", "run_tests", "finish"}
DEFAULT_MANIFEST = Path(__file__).parent / "episodes" / "core.json"
SYSTEM_PROMPT = """You are a local coding agent in an isolated repository.
Reply with exactly one JSON object at a time and no Markdown:
{"tool":"list_files|read_file|apply_patch|write_file|run_tests|finish","arguments":{...}}
Begin every episode with exactly {"tool":"list_files","arguments":{}}.
Never batch multiple tool calls in one response. Wait for each tool result before the next call.
Use list_files with optional directory and read_file with path. After reading a complete file under 64 KB, prefer write_file with path and the complete file in content. For a small localized change, apply_patch requires one unified diff beginning with --- a/path and +++ b/path. Never repeat a diff or put raw source code in apply_patch. Use run_tests with no arguments, and finish only when the task is complete.
You cannot access hidden tests or a shell. Work carefully within the available steps."""


class AgenticRunError(RuntimeError):
    """Raised when an episode violates its execution contract."""


def _default_request(url: str, body: dict[str, object], timeout: float) -> dict[str, object]:
    return _json_request(url, body, timeout)


def _parse_call(response: dict[str, object]) -> tuple[str, dict[str, object]]:
    content = _response_content(response)
    try:
        value = json.loads(content)
    except json.JSONDecodeError as error:
        # Some local chat templates concatenate a second JSON tool call despite
        # the one-call protocol. Execute only the first complete object and feed
        # its result back; the unexecuted suffix has no authority. Arbitrary
        # prose suffixes still fail closed.
        try:
            first, first_end = json.JSONDecoder().raw_decode(content)
        except json.JSONDecodeError:
            first = None
            first_end = 0
        suffix = content[first_end:].lstrip()
        if first_end > 0 and suffix.startswith("{"):
            value = first
        else:
            try:
                value = json.loads(content + "}")
            except json.JSONDecodeError:
                tool_objects = list(re.finditer(r'\{\s*"tool"\s*:', content))
                start = tool_objects[-1].start() if tool_objects else -1
                if start <= 0:
                    raise AgenticRunError(f"tool call is not valid JSON: {error}") from error
                try:
                    value, end = json.JSONDecoder().raw_decode(content, start)
                except json.JSONDecodeError as nested_error:
                    raise AgenticRunError(
                        f"tool call is not valid JSON: {nested_error}"
                    ) from nested_error
                if content[end:].strip():
                    raise AgenticRunError("tool call has content after its JSON object")
    if not isinstance(value, dict) or set(value) not in (
        {"tool"},
        {"tool", "arguments"},
    ):
        raise AgenticRunError("tool call must contain only tool and arguments")
    tool = value["tool"]
    if not isinstance(tool, str) or tool not in TOOLS:
        raise AgenticRunError(f"unsupported tool: {tool}")
    if "arguments" not in value:
        if tool not in {"run_tests", "finish"}:
            raise AgenticRunError(f"{tool} requires arguments")
        value["arguments"] = {}
    arguments = value["arguments"]
    if not isinstance(arguments, dict):
        raise AgenticRunError("tool arguments must be an object")
    if tool == "finish" and set(arguments) == {"message"}:
        if not isinstance(arguments["message"], str):
            raise AgenticRunError("finish message must be a string")
        arguments = {}
    return tool, arguments


def _argument(arguments: Mapping[str, object], name: str) -> str:
    if set(arguments) != {name} or not isinstance(arguments.get(name), str):
        raise AgenticRunError(f"tool requires exactly one string argument: {name}")
    return str(arguments[name])


def _dispatch(sandbox: AgenticSandbox, tool: str, arguments: dict[str, object]) -> object:
    if tool == "list_files":
        if not set(arguments).issubset({"directory"}):
            raise AgenticRunError("list_files accepts only directory")
        directory = arguments.get("directory", ".")
        if not isinstance(directory, str):
            raise AgenticRunError("directory must be a string")
        return sandbox.list_files(directory)
    if tool == "read_file":
        return sandbox.read_file(_argument(arguments, "path"))
    if tool == "apply_patch":
        if set(arguments) == {"path", "content"}:
            path = arguments["path"]
            content = arguments["content"]
            if not isinstance(path, str) or not isinstance(content, str):
                raise AgenticRunError("apply_patch path and content must be strings")
            # Normalize a common OpenAI-compatible agent dialect. Authority is
            # unchanged: AgenticSandbox still enforces the manifest allowlist.
            if content.startswith("--- a/"):
                if path not in PATCH_TARGET.findall(content):
                    raise AgenticRunError("apply_patch path must match a patch target")
                return sandbox.apply_patch(content)
            return sandbox.write_file(path, content)
        if set(arguments) not in (
            {"patch"},
            {"diff"},
            {"path", "patch"},
            {"path", "diff"},
        ):
            raise AgenticRunError("apply_patch requires patch/diff and optional path")
        patch_text = arguments.get("patch", arguments.get("diff"))
        if not isinstance(patch_text, str):
            raise AgenticRunError("patch must be a string")
        if "path" in arguments:
            path = arguments["path"]
            if not isinstance(path, str) or path not in PATCH_TARGET.findall(patch_text):
                raise AgenticRunError("apply_patch path must match a patch target")
        return sandbox.apply_patch(patch_text)
    if tool == "write_file":
        if set(arguments) != {"path", "content"}:
            raise AgenticRunError("write_file requires path and content")
        path = arguments.get("path")
        content = arguments.get("content")
        if not isinstance(path, str) or not isinstance(content, str):
            raise AgenticRunError("write_file path and content must be strings")
        return sandbox.write_file(path, content)
    if tool == "run_tests":
        if arguments:
            raise AgenticRunError("run_tests accepts no arguments")
        return sandbox.run_tests()
    raise AgenticRunError(f"cannot dispatch tool: {tool}")


def _hidden_verifier(
    episode: Episode, root: Path, *, timeout: float
) -> dict[str, object]:
    try:
        completed = subprocess.run(
            list(episode.hidden_verifier),
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
            timeout=min(60, timeout),
        )
    except subprocess.TimeoutExpired as error:
        raise AgenticRunError("hidden verifier timeout") from error
    return {
        "exit_code": completed.returncode,
        "stdout": completed.stdout[:16_000],
        "stderr": completed.stderr[:16_000],
    }


def run_episode(
    episode: Episode,
    output_dir: Path,
    *,
    endpoint: str,
    model: str,
    request_json: JsonRequest = _default_request,
    request_timeout: float = 900,
    require_self_review: bool = False,
    verifier_retry: bool = False,
) -> dict[str, object]:
    """Execute an episode without mutating its source fixture."""

    output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    result: dict[str, object] = {
        "episode_id": episode.episode_id,
        "task_sha256": episode.task_sha256,
        "passed": False,
        "error": None,
        "steps": 0,
        "decoded_tokens": 0,
        "draft_generated": 0,
        "draft_accepted": 0,
        "telemetry": [],
        "tools": [],
        "hidden_verifier_exit_code": None,
        "hidden_verifier_stdout": "",
        "hidden_verifier_stderr": "",
        "self_review_required": require_self_review,
        "self_review_completed": False,
        "verifier_retry_enabled": verifier_retry,
        "feedback_retry_used": False,
        "verifier_attempts": [],
    }
    messages: list[dict[str, str]] = [
        {"role": "system", "content": SYSTEM_PROMPT},
        {"role": "user", "content": episode.task},
    ]
    output_token_budget = episode.max_output_tokens + (
        2_048 if require_self_review else 0
    ) + (episode.max_output_tokens if verifier_retry else 0)
    step_budget = episode.max_steps + (6 if verifier_retry else 0)
    result["output_token_budget"] = output_token_budget
    result["step_budget"] = step_budget
    started = time.monotonic()
    review_start_index: int | None = None
    retry_start_index: int | None = None
    try:
        with TemporaryDirectory(prefix="workspace-", dir=output_dir) as tmp:
            workspace = Path(tmp)
            shutil.copytree(episode.fixture_dir, workspace, dirs_exist_ok=True)
            sandbox = AgenticSandbox(episode, workspace)
            for step in range(1, step_budget + 1):
                elapsed = time.monotonic() - started
                remaining_wall = episode.wall_timeout_seconds - elapsed
                remaining_tokens = output_token_budget - int(result["decoded_tokens"])
                if remaining_wall <= 0:
                    raise AgenticRunError("episode wall-time limit exceeded")
                if remaining_tokens <= 0:
                    raise AgenticRunError("episode output-token limit exceeded")
                response = request_json(
                    endpoint,
                    {
                        "model": model,
                        "messages": list(messages),
                        "temperature": 0,
                        "seed": 42,
                        "max_tokens": min(2048, remaining_tokens),
                        "stream": False,
                        "timings_per_token": False,
                        "chat_template_kwargs": {"enable_thinking": False},
                    },
                    min(request_timeout, remaining_wall),
                )
                telemetry = parse_response_timings(response)
                decoded = int(telemetry["decoded_tokens"])
                if decoded > remaining_tokens:
                    raise AgenticRunError("episode output-token limit exceeded")
                result["steps"] = step
                result["decoded_tokens"] = int(result["decoded_tokens"]) + decoded
                generated = telemetry["draft_generated"]
                accepted = telemetry["draft_accepted"]
                if generated is not None:
                    result["draft_generated"] = int(result["draft_generated"]) + int(
                        generated
                    )
                    result["draft_accepted"] = int(result["draft_accepted"]) + int(
                        accepted
                    )
                result["telemetry"].append(telemetry)
                _write_json_atomic(
                    output_dir / f"step-{step:03d}.json",
                    {"response": response},
                )
                tool, arguments = _parse_call(response)
                result["tools"].append(tool)
                messages.append(
                    {
                        "role": "assistant",
                        "content": json.dumps(
                            {"tool": tool, "arguments": arguments}
                        ),
                    }
                )
                if tool == "finish":
                    if arguments:
                        raise AgenticRunError("finish accepts no arguments")
                    if require_self_review:
                        reviewed_tools = (
                            result["tools"][review_start_index:]
                            if review_start_index is not None
                            else []
                        )
                        missing_review_tools = {"read_file", "run_tests"} - set(
                            reviewed_tools
                        )
                        if review_start_index is None or missing_review_tools:
                            if review_start_index is None:
                                review_start_index = len(result["tools"])
                            review_message = (
                                "Finish deferred for an independent reliability review. "
                                "Re-read the task and the final implementation, look for "
                                "edge cases not covered by visible tests, correct any issue, "
                                "and run the visible tests again before finishing."
                            )
                            _write_json_atomic(
                                output_dir / f"step-{step:03d}.json",
                                {
                                    "response": response,
                                    "tool": tool,
                                    "arguments": arguments,
                                    "tool_result": {
                                        "ok": False,
                                        "review_required": True,
                                        "message": review_message,
                                    },
                                },
                            )
                            messages.append(
                                {"role": "user", "content": review_message}
                            )
                            continue
                        result["self_review_completed"] = True
                    if retry_start_index is not None:
                        retry_tools = set(result["tools"][retry_start_index:])
                        missing_retry_tools = {"read_file", "run_tests"} - retry_tools
                        if missing_retry_tools:
                            retry_message = (
                                "Verifier feedback repair is incomplete. Re-read the "
                                "affected implementation and run visible tests before "
                                "finishing again."
                            )
                            _write_json_atomic(
                                output_dir / f"step-{step:03d}.json",
                                {
                                    "response": response,
                                    "tool": tool,
                                    "arguments": arguments,
                                    "tool_result": {
                                        "ok": False,
                                        "retry_review_required": True,
                                        "message": retry_message,
                                    },
                                },
                            )
                            messages.append({"role": "user", "content": retry_message})
                            continue
                    _write_json_atomic(
                        output_dir / f"step-{step:03d}.json",
                        {"response": response, "tool": tool, "arguments": arguments},
                    )
                    remaining_wall = episode.wall_timeout_seconds - (
                        time.monotonic() - started
                    )
                    if remaining_wall <= 0:
                        raise AgenticRunError("episode wall-time limit exceeded")
                    verifier = _hidden_verifier(
                        episode, workspace, timeout=remaining_wall
                    )
                    result["hidden_verifier_exit_code"] = verifier["exit_code"]
                    result["hidden_verifier_stdout"] = verifier["stdout"]
                    result["hidden_verifier_stderr"] = verifier["stderr"]
                    result["verifier_attempts"].append(verifier)
                    if verifier["exit_code"] != 0:
                        if verifier_retry and not result["feedback_retry_used"]:
                            result["feedback_retry_used"] = True
                            retry_start_index = len(result["tools"])
                            if require_self_review:
                                review_start_index = retry_start_index
                            feedback = (
                                "The local verifier failed. You have exactly one repair "
                                "attempt. Use this failure output as evidence, inspect the "
                                "implementation, fix the general cause, and run visible "
                                "tests before finishing again.\n\n"
                                + str(verifier["stderr"])[-12_000:]
                            )
                            _write_json_atomic(
                                output_dir / f"step-{step:03d}.json",
                                {
                                    "response": response,
                                    "tool": tool,
                                    "arguments": arguments,
                                    "hidden_verifier": verifier,
                                    "feedback_retry": True,
                                },
                            )
                            messages.append({"role": "user", "content": feedback})
                            continue
                        raise AgenticRunError("hidden verifier failed")
                    _write_json_atomic(
                        output_dir / f"step-{step:03d}.json",
                        {
                            "response": response,
                            "tool": tool,
                            "arguments": arguments,
                            "hidden_verifier": verifier,
                        },
                    )
                    result["passed"] = True
                    result["passed_first_attempt"] = (
                        len(result["verifier_attempts"]) == 1
                    )
                    break
                try:
                    tool_result: object = {
                        "ok": True,
                        "result": _dispatch(sandbox, tool, arguments),
                    }
                except ToolError as error:
                    tool_result = {"ok": False, "error": str(error)}
                _write_json_atomic(
                    output_dir / f"step-{step:03d}.json",
                    {
                        "response": response,
                        "tool": tool,
                        "arguments": arguments,
                        "tool_result": tool_result,
                    },
                )
                messages.append(
                    {
                        "role": "user",
                        "content": "Tool result:\n" + json.dumps(tool_result, sort_keys=True),
                    }
                )
            else:
                raise AgenticRunError("episode step limit exceeded without finish")
    except (
        AgenticRunError,
        CodingRunError,
        SmokeError,
        TelemetryError,
        ToolError,
        OSError,
        ValueError,
    ) as error:
        result["error"] = str(error)
    finally:
        result["elapsed_seconds"] = time.monotonic() - started
        _write_json_atomic(output_dir / "result.json", result)
    return result


def evaluate_episode_gate(
    results: list[dict[str, object]], peak_rss_kb: int
) -> dict[str, object]:
    """Apply quality, tool-flow, per-step speed, and memory gates."""

    reasons = [
        f"episode@{result['episode_id']}"
        for result in results
        if not result.get("passed")
    ]
    has_complete_flow = any(
        REQUIRED_TOOL_FLOW.issubset(set(result.get("tools", [])))
        and bool({"apply_patch", "write_file"} & set(result.get("tools", [])))
        for result in results
    )
    if not has_complete_flow:
        reasons.append("required-tool-flow")
    points = [
        {
            "decode_tokens_per_second": telemetry["decode_tokens_per_second"],
            "decoded_tokens": telemetry["decoded_tokens"],
            "peak_rss_kb": peak_rss_kb,
        }
        for result in results
        for telemetry in result.get("telemetry", [])
        if isinstance(telemetry, Mapping)
    ]
    try:
        performance = enforce_coding_gate(points)
    except TelemetryError:
        performance = {"passed": False, "reasons": ["telemetry"]}
    reasons.extend(str(reason) for reason in performance["reasons"])
    return {
        "passed": not reasons,
        "reasons": reasons,
        "warnings": list(performance.get("warnings", [])),
        "mean_decode_tokens_per_second": performance.get(
            "mean_decode_tokens_per_second"
        ),
        "episode_passes": sum(bool(result.get("passed")) for result in results),
        "episode_count": len(results),
        "step_count": len(points),
        "complete_tool_flow": has_complete_flow,
    }


def run_episodes(
    manifest: Path,
    output_dir: Path,
    *,
    base_url: str,
    model: str,
    session_id: UUID,
    server_pid: int,
    request_json: JsonRequest = _default_request,
    request_timeout: float = 900,
    require_self_review: bool = False,
    verifier_retry: bool = False,
) -> dict[str, object]:
    """Run a manifest in one measured server lifecycle and persist its report."""

    episodes = load_episodes(manifest)
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
    started_at = datetime.now(timezone.utc).isoformat()
    try:
        results = [
            run_episode(
                episode,
                output_dir / episode.episode_id,
                endpoint=endpoint,
                model=model,
                request_json=request_json,
                request_timeout=request_timeout,
                require_self_review=require_self_review,
                verifier_retry=verifier_retry,
            )
            for episode in episodes
        ]
    finally:
        stop_monitor.set()
        monitor.join(timeout=2)
    decision = evaluate_episode_gate(results, peak_rss[0])
    report: dict[str, object] = {
        "schema": 1,
        "session_id": str(session_id),
        "model": model,
        "base_url": base_url,
        "manifest": str(manifest.resolve()),
        "started_at": started_at,
        "ended_at": datetime.now(timezone.utc).isoformat(),
        "server_pid": server_pid,
        "self_review_required": require_self_review,
        "verifier_retry_enabled": verifier_retry,
        "peak_rss_kb": peak_rss[0],
        "decoded_tokens": sum(int(result["decoded_tokens"]) for result in results),
        "draft_generated": sum(int(result["draft_generated"]) for result in results),
        "draft_accepted": sum(int(result["draft_accepted"]) for result in results),
        "episodes": results,
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
    parser.add_argument("--require-self-review", action="store_true")
    parser.add_argument("--verifier-retry", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = run_episodes(
            args.manifest,
            args.output_dir,
            base_url=args.base_url,
            model=args.model,
            session_id=args.session_id,
            server_pid=args.server_pid,
            require_self_review=args.require_self_review,
            verifier_retry=args.verifier_retry,
        )
    except (AgenticRunError, OSError, ValueError) as error:
        print(f"agentic episode run failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["decision"]["passed"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
