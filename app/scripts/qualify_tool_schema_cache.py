#!/usr/bin/env python3
"""Measure cold, same-schema, and changed-schema tool TTFT on one live server."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
from typing import Any

import qualify_openai_stream as qualify


def tool_body(
    model: str,
    name: str,
    description: str,
    properties: dict[str, dict[str, str]],
    prompt: str,
) -> dict[str, Any]:
    body = qualify._base_body(model)
    body["messages"] = [{"role": "user", "content": prompt}]
    body["tools"] = [
        {
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": properties,
                    "required": list(properties),
                    "additionalProperties": False,
                },
            },
        }
    ]
    body["tool_choice"] = {"type": "function", "function": {"name": name}}
    return body


def scenarios(model: str) -> list[dict[str, Any]]:
    integers = {"a": {"type": "integer"}, "b": {"type": "integer"}}
    return [
        {
            "label": "cold_add_schema",
            "body": tool_body(model, "add", "Add two integers.", integers, "Use add to calculate 19 + 23."),
            "tool": "add",
            "arguments": {"a": 19, "b": 23},
        },
        {
            "label": "same_add_schema_new_prompt",
            "body": tool_body(model, "add", "Add two integers.", integers, "Use add to calculate 20 + 22."),
            "tool": "add",
            "arguments": {"a": 20, "b": 22},
        },
        {
            "label": "new_multiply_schema",
            "body": tool_body(model, "multiply", "Multiply two integers.", integers, "Use multiply to calculate 6 times 7."),
            "tool": "multiply",
            "arguments": {"a": 6, "b": 7},
        },
        {
            "label": "return_to_add_schema",
            "body": tool_body(model, "add", "Add two integers.", integers, "Use add to calculate 18 + 24."),
            "tool": "add",
            "arguments": {"a": 18, "b": 24},
        },
    ]


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
        for scenario in scenarios(args.model):
            run = qualify.run_case(
                args.base_url,
                args.model,
                "tool",
                timeout=args.timeout,
                body_override=scenario["body"],
                expected_tool_name=scenario["tool"],
                expected_tool_arguments=scenario["arguments"],
            )
            run["label"] = scenario["label"]
            run["cached_tokens"] = cached_tokens(run)
            runs.append(run)
    after = qualify.resource_snapshot(args.pid)
    passed = probe["passed"] and len(runs) == 4 and all(run["passed"] for run in runs)
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
