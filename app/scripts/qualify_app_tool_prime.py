#!/usr/bin/env python3
"""Measure a one-token prime followed by Slipstream's real local tool schema."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
from typing import Any

import qualify_openai_stream as qualify


APP_TOOLS: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "get_current_time",
            "description": "Return the current local date and time as an ISO-8601 string.",
            "parameters": {"type": "object", "properties": {}, "additionalProperties": False},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "calculator",
            "description": "Evaluate a simple arithmetic expression (numbers and + - * / parentheses).",
            "parameters": {
                "type": "object",
                "properties": {
                    "expression": {"type": "string", "description": "e.g. (2+3)*4"},
                },
                "required": ["expression"],
            },
        },
    },
]


def body(model: str, prompt: str, max_tokens: int, tool_choice: str | dict[str, Any] = "auto") -> dict[str, Any]:
    request = qualify._base_body(model)
    request["messages"] = [{"role": "user", "content": prompt}]
    request["max_tokens"] = max_tokens
    request["tools"] = APP_TOOLS
    request["tool_choice"] = tool_choice
    return request


def cached_tokens(run: dict[str, Any]) -> int:
    details = run.get("usage", {}).get("prompt_tokens_details") or {}
    value = details.get("cached_tokens", 0)
    return value if isinstance(value, int) else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--pid", type=int)
    parser.add_argument("--timeout", type=float, default=240.0)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    probe = qualify.probe_endpoints(args.base_url, min(args.timeout, 10.0))
    before = qualify.resource_snapshot(args.pid)
    runs: list[dict[str, Any]] = []
    if probe["passed"]:
        prime = qualify.run_case(
            args.base_url,
            args.model,
            "plain",
            timeout=args.timeout,
            body_override=body(args.model, "Initialize the local tool contract.", 1),
            validate_output=False,
        )
        prime["label"] = "one_token_schema_prime"
        prime["cached_tokens"] = cached_tokens(prime)
        runs.append(prime)

        calculator = qualify.run_case(
            args.base_url,
            args.model,
            "tool",
            timeout=args.timeout,
            body_override=body(args.model, "Call calculator with expression exactly 19+23.", 48),
            expected_tool_name="calculator",
            expected_tool_arguments={"expression": "19+23"},
        )
        calculator["label"] = "first_visible_calculator"
        calculator["cached_tokens"] = cached_tokens(calculator)
        runs.append(calculator)

        clock = qualify.run_case(
            args.base_url,
            args.model,
            "tool",
            timeout=args.timeout,
            body_override=body(args.model, "Call get_current_time now.", 48),
            expected_tool_name="get_current_time",
            expected_tool_arguments={},
        )
        clock["label"] = "same_schema_other_tool"
        clock["cached_tokens"] = cached_tokens(clock)
        runs.append(clock)

    after = qualify.resource_snapshot(args.pid)
    passed = probe["passed"] and len(runs) == 3 and all(run["passed"] for run in runs)
    artifact = {
        "schema": 1,
        "started": datetime.now(timezone.utc).isoformat(),
        "base_url": args.base_url,
        "model": args.model,
        "probe": probe,
        "resources_before": before,
        "resources_after": after,
        "runs": runs,
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
