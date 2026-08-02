#!/usr/bin/env python3
"""Common live OpenAI/SSE qualification for llama.cpp/PGRN and oMLX/PGRN."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import time
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


HARNESS_SCHEMA = 1


def _url(base_url: str, path: str) -> str:
    return f"{base_url.rstrip('/')}{path}"


def _request_json(base_url: str, path: str, timeout: float) -> dict[str, Any]:
    request = Request(_url(base_url, path), headers={"Accept": "application/json"})
    with urlopen(request, timeout=timeout) as response:
        return json.loads(response.read())


def probe_endpoints(base_url: str, timeout: float) -> dict[str, Any]:
    started = time.perf_counter()
    try:
        health = _request_json(base_url, "/health", timeout)
        models = _request_json(base_url, "/v1/models", timeout)
        ids = [str(item.get("id")) for item in models.get("data", []) if item.get("id")]
        return {
            "passed": bool(ids),
            "health": health,
            "model_ids": ids,
            "wall_seconds": round(time.perf_counter() - started, 6),
            "error": None if ids else "models endpoint returned no model ids",
        }
    except (HTTPError, URLError, TimeoutError, json.JSONDecodeError) as error:
        return {
            "passed": False,
            "health": None,
            "model_ids": [],
            "wall_seconds": round(time.perf_counter() - started, 6),
            "error": str(error),
        }


def _base_body(model: str) -> dict[str, Any]:
    return {
        "model": model,
        "messages": [],
        "max_tokens": 48,
        "temperature": 0,
        "stream": True,
        "stream_options": {"include_usage": True},
        "chat_template_kwargs": {"enable_thinking": False},
    }


def build_case_body(model: str, case: str) -> dict[str, Any]:
    body = _base_body(model)
    if case == "plain":
        body["messages"] = [{"role": "user", "content": "Reply with exactly 42 and nothing else."}]
    elif case == "json":
        body["messages"] = [{"role": "user", "content": "Return the requested JSON object."}]
        body["response_format"] = {
            "type": "json_schema",
            "json_schema": {
                "name": "slipstream_gate",
                "strict": True,
                "schema": {
                    "type": "object",
                    "properties": {
                        "answer": {"type": "integer", "const": 42},
                        "label": {"type": "string", "const": "ok"},
                    },
                    "required": ["answer", "label"],
                    "additionalProperties": False,
                },
            },
        }
    elif case == "tool":
        body["messages"] = [{"role": "user", "content": "Use the add tool to calculate 19 + 23."}]
        body["tools"] = [
            {
                "type": "function",
                "function": {
                    "name": "add",
                    "description": "Add two integers.",
                    "parameters": {
                        "type": "object",
                        "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                        "required": ["a", "b"],
                        "additionalProperties": False,
                    },
                },
            }
        ]
        body["tool_choice"] = {"type": "function", "function": {"name": "add"}}
    else:
        raise ValueError(f"unsupported qualification case: {case}")
    return body


def _percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round((len(ordered) - 1) * fraction)))
    return ordered[index]


def _resolve_decode_metrics(
    completion_tokens: int,
    measured_seconds: float,
    usage: dict[str, Any],
) -> tuple[float, float]:
    """Prefer engine timing when an API buffers structured output until completion."""

    def positive_number(value: Any) -> float | None:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return None
        number = float(value)
        return number if math.isfinite(number) and number > 0 else None

    engine_seconds = positive_number(usage.get("generation_duration"))
    engine_rate = positive_number(usage.get("generation_tokens_per_second"))
    if engine_seconds is not None and engine_rate is not None:
        return engine_seconds, engine_rate
    if engine_seconds is not None:
        rate = completion_tokens / engine_seconds if completion_tokens else 0.0
        return engine_seconds, rate
    if engine_rate is not None:
        seconds = completion_tokens / engine_rate if completion_tokens else 0.0
        return seconds, engine_rate

    seconds = max(0.0, measured_seconds)
    rate = completion_tokens / seconds if completion_tokens and seconds > 0 else 0.0
    return seconds, rate


def _canonical_output(content: str, tool_calls: list[dict[str, Any]]) -> str:
    if tool_calls:
        payload: Any = []
        for call in tool_calls:
            function = call.get("function") or {}
            raw_arguments = function.get("arguments") or ""
            try:
                arguments: Any = json.loads(raw_arguments)
            except (json.JSONDecodeError, TypeError):
                arguments = raw_arguments
            payload.append(
                {
                    "type": call.get("type", "function"),
                    "function": {"name": function.get("name", ""), "arguments": arguments},
                }
            )
    else:
        payload = content
    return json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _validate_case(case: str, content: str, tool_calls: list[dict[str, Any]]) -> list[str]:
    errors: list[str] = []
    if case == "plain":
        if content.strip() != "42":
            errors.append(f"plain output mismatch: {content!r}")
    elif case == "json":
        try:
            parsed = json.loads(content)
        except json.JSONDecodeError as error:
            errors.append(f"JSON output parse failed: {error}")
        else:
            if parsed != {"answer": 42, "label": "ok"}:
                errors.append(f"JSON output mismatch: {parsed!r}")
    elif case == "tool":
        if len(tool_calls) != 1:
            errors.append(f"expected one tool call, got {len(tool_calls)}")
        else:
            function = tool_calls[0].get("function") or {}
            if function.get("name") != "add":
                errors.append(f"tool name mismatch: {function.get('name')!r}")
            try:
                arguments = json.loads(function.get("arguments") or "")
            except json.JSONDecodeError as error:
                errors.append(f"tool arguments parse failed: {error}")
            else:
                if arguments != {"a": 19, "b": 23}:
                    errors.append(f"tool arguments mismatch: {arguments!r}")
    return errors


def run_case(
    base_url: str,
    model: str,
    case: str,
    timeout: float,
    max_ttft: float = 180.0,
    max_chunk_gap: float = 5.0,
) -> dict[str, Any]:
    body = build_case_body(model, case)
    raw_body = json.dumps(body, separators=(",", ":")).encode()
    request = Request(
        _url(base_url, "/v1/chat/completions"),
        data=raw_body,
        method="POST",
        headers={"Content-Type": "application/json", "Accept": "text/event-stream"},
    )
    started = time.perf_counter()
    first_signal: float | None = None
    signal_times: list[float] = []
    content_parts: list[str] = []
    calls: dict[int, dict[str, Any]] = {}
    usage: dict[str, Any] = {}
    finish_reason: str | None = None
    event_count = 0
    done = False
    errors: list[str] = []

    try:
        with urlopen(request, timeout=timeout) as response:
            content_type = response.headers.get("Content-Type", "")
            if "text/event-stream" not in content_type:
                errors.append(f"unexpected content type: {content_type!r}")
            while True:
                raw_line = response.readline()
                if not raw_line:
                    break
                line = raw_line.decode("utf-8", errors="replace").strip()
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if data == "[DONE]":
                    done = True
                    break
                try:
                    event = json.loads(data)
                except json.JSONDecodeError as error:
                    errors.append(f"invalid SSE JSON: {error}")
                    continue
                event_count += 1
                if isinstance(event.get("usage"), dict):
                    usage = event["usage"]
                for choice in event.get("choices") or []:
                    if choice.get("finish_reason") is not None:
                        finish_reason = str(choice["finish_reason"])
                    delta = choice.get("delta") or {}
                    meaningful = False
                    content = delta.get("content")
                    if isinstance(content, str) and content:
                        content_parts.append(content)
                        meaningful = True
                    for tool_delta in delta.get("tool_calls") or []:
                        index = int(tool_delta.get("index", 0))
                        call = calls.setdefault(
                            index,
                            {"id": "", "type": "function", "function": {"name": "", "arguments": ""}},
                        )
                        if tool_delta.get("id"):
                            call["id"] += str(tool_delta["id"])
                        if tool_delta.get("type"):
                            call["type"] = str(tool_delta["type"])
                        function = tool_delta.get("function") or {}
                        if function.get("name"):
                            call["function"]["name"] += str(function["name"])
                        if function.get("arguments"):
                            call["function"]["arguments"] += str(function["arguments"])
                        meaningful = True
                    if meaningful:
                        now = time.perf_counter()
                        first_signal = first_signal or now
                        signal_times.append(now)
    except (HTTPError, URLError, TimeoutError) as error:
        errors.append(str(error))

    ended = time.perf_counter()
    content = "".join(content_parts)
    tool_calls = [calls[index] for index in sorted(calls)]
    errors.extend(_validate_case(case, content, tool_calls))
    if not done:
        errors.append("SSE stream ended without [DONE]")
    if not usage or not isinstance(usage.get("completion_tokens"), int):
        errors.append("stream usage/completion_tokens missing")
    if first_signal is None:
        errors.append("stream produced no content or tool-call delta")

    ttft = (first_signal - started) if first_signal is not None else None
    gaps = [b - a for a, b in zip(signal_times, signal_times[1:])]
    completion_tokens = int(usage.get("completion_tokens") or 0)
    measured_decode_seconds = max(0.0, ended - first_signal) if first_signal is not None else 0.0
    decode_seconds, decode_tps = _resolve_decode_metrics(
        completion_tokens,
        measured_decode_seconds,
        usage,
    )
    if ttft is not None and ttft > max_ttft:
        errors.append(f"TTFT {ttft:.3f}s exceeds {max_ttft:.3f}s")
    if gaps and max(gaps) > max_chunk_gap:
        errors.append(f"chunk gap {max(gaps):.3f}s exceeds {max_chunk_gap:.3f}s")

    canonical = _canonical_output(content, tool_calls)
    return {
        "case": case,
        "passed": not errors,
        "errors": errors,
        "done": done,
        "finish_reason": finish_reason,
        "content": content,
        "tool_calls": tool_calls,
        "output_sha256": hashlib.sha256(canonical.encode()).hexdigest(),
        "usage": usage,
        "events": event_count,
        "content_chunks": len(signal_times),
        "ttft_seconds": round(ttft, 6) if ttft is not None else None,
        "wall_seconds": round(ended - started, 6),
        "decode_seconds": round(decode_seconds, 6),
        "decode_tokens_per_second": round(decode_tps, 6),
        "chunk_gap_p50_seconds": round(statistics.median(gaps), 6) if gaps else 0.0,
        "chunk_gap_p95_seconds": round(_percentile(gaps, 0.95), 6),
        "chunk_gap_max_seconds": round(max(gaps), 6) if gaps else 0.0,
    }


def resource_snapshot(pid: int | None = None) -> dict[str, Any]:
    snapshot: dict[str, Any] = {"platform": platform.system().lower()}
    if platform.system() == "Darwin":
        try:
            swap = subprocess.check_output(["/usr/sbin/sysctl", "-n", "vm.swapusage"], text=True)
            match = re.search(r"used = ([0-9.]+)M", swap)
            snapshot["swap_used_mib"] = float(match.group(1)) if match else None
            vm = subprocess.check_output(["/usr/bin/vm_stat"], text=True)
            page_match = re.search(r"page size of (\d+)", vm)
            page = int(page_match.group(1)) if page_match else 16384
            pages = {
                key: int(match.group(1)) if (match := re.search(rf"Pages {key}:\s+(\d+)", vm)) else 0
                for key in ("free", "inactive")
            }
            snapshot["free_inactive_bytes"] = (pages["free"] + pages["inactive"]) * page
        except (OSError, subprocess.SubprocessError, ValueError):
            snapshot["probe_error"] = "macOS memory probe failed"
    elif Path("/proc/meminfo").is_file():
        values: dict[str, int] = {}
        for line in Path("/proc/meminfo").read_text().splitlines():
            key, _, rest = line.partition(":")
            if rest.strip().split():
                values[key] = int(rest.strip().split()[0]) * 1024
        snapshot["memory_available_bytes"] = values.get("MemAvailable")
        snapshot["swap_used_bytes"] = max(0, values.get("SwapTotal", 0) - values.get("SwapFree", 0))
    if pid:
        try:
            rss_kib = int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True).strip())
            snapshot["server_rss_bytes"] = rss_kib * 1024
        except (OSError, subprocess.SubprocessError, ValueError):
            snapshot["server_rss_bytes"] = None
    return snapshot


def summarize(runs: list[dict[str, Any]], repetitions: int) -> dict[str, Any]:
    by_case: dict[str, list[dict[str, Any]]] = {}
    for run in runs:
        by_case.setdefault(run["case"], []).append(run)
    cases: dict[str, Any] = {}
    for case, values in by_case.items():
        hashes = {value["output_sha256"] for value in values}
        ttfts = [value["ttft_seconds"] for value in values if value["ttft_seconds"] is not None]
        rates = [value["decode_tokens_per_second"] for value in values]
        cases[case] = {
            "passed": len(values) == repetitions and all(value["passed"] for value in values) and len(hashes) == 1,
            "deterministic": len(hashes) == 1,
            "output_sha256": next(iter(hashes)) if len(hashes) == 1 else sorted(hashes),
            "ttft_median_seconds": round(statistics.median(ttfts), 6) if ttfts else None,
            "decode_tps_mean": round(statistics.mean(rates), 6) if rates else 0.0,
            "chunk_gap_max_seconds": max((value["chunk_gap_max_seconds"] for value in values), default=0.0),
        }
    return {"passed": bool(cases) and all(value["passed"] for value in cases.values()), "cases": cases}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--engine", required=True, help="llama.cpp-pgrn or omlx-pgrn")
    parser.add_argument("--model", required=True)
    parser.add_argument("--case", choices=("plain", "json", "tool", "all"), default="all")
    parser.add_argument("--repetitions", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=240.0)
    parser.add_argument("--max-ttft", type=float, default=180.0)
    parser.add_argument("--max-chunk-gap", type=float, default=5.0)
    parser.add_argument("--pid", type=int)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("--repetitions must be >= 1")

    started = datetime.now(timezone.utc).isoformat()
    probe = probe_endpoints(args.base_url, min(args.timeout, 10.0))
    cases = ("plain", "json", "tool") if args.case == "all" else (args.case,)
    before = resource_snapshot(args.pid)
    runs = [
        run_case(
            args.base_url,
            args.model,
            case,
            timeout=args.timeout,
            max_ttft=args.max_ttft,
            max_chunk_gap=args.max_chunk_gap,
        )
        for _ in range(args.repetitions)
        for case in cases
    ] if probe["passed"] else []
    after = resource_snapshot(args.pid)
    summary = summarize(runs, args.repetitions)
    passed = probe["passed"] and summary["passed"]
    artifact = {
        "schema": HARNESS_SCHEMA,
        "started": started,
        "finished": datetime.now(timezone.utc).isoformat(),
        "engine": args.engine,
        "base_url": args.base_url,
        "model": args.model,
        "repetitions": args.repetitions,
        "thresholds": {"max_ttft_seconds": args.max_ttft, "max_chunk_gap_seconds": args.max_chunk_gap},
        "probe": probe,
        "resources_before": before,
        "resources_after": after,
        "runs": runs,
        "summary": summary,
        "passed": passed,
    }
    raw = json.dumps(artifact, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(raw)
    print(raw, end="")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
