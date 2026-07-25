"""Peregrine MCP server — expose Peregrine capabilities to any MCP-capable agent.

Agent-agnostic integration (Plan v3, Kilo-Code direction): rather than fork an
agent, Peregrine exposes its capabilities over the Model Context Protocol so
Kilo Code — or any MCP client — can consume them. Stdlib only (JSON-RPC 2.0 over
newline-delimited stdio), no MCP SDK dependency.

This first version exposes `peregrine_retrieve` (bounded-window repo retrieval).
The dispatch layer is pure and unit-tested; only `serve_stdio` touches streams.
Protocol shapes verified against the MCP 2025-06-18 spec.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any

from bench.m1.escalation import (
    EscalationError,
    TriggerContext,
    decide_escalation,
)
from bench.m1.retrieval import (
    DEFAULT_WINDOW_TOKENS,
    RetrievalError,
    assemble_context,
    estimate_tokens,
    retrieve_repo,
)

PROTOCOL_VERSION = "2025-06-18"
SERVER_NAME = "peregrine"
SERVER_VERSION = "0.1.0"

# JSON-RPC error codes
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

DEFAULT_RETRIEVE_BUDGET = 8000


def tool_definitions() -> list[dict[str, Any]]:
    return [
        {
            "name": "peregrine_retrieve",
            "title": "Peregrine repo retrieval",
            "description": (
                "Retrieve the most relevant repository chunks for a task into a "
                "bounded token budget using deterministic BM25 lexical search. "
                "Returns assembled file context and a selection manifest."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The task or search intent to retrieve context for.",
                    },
                    "repo": {
                        "type": "string",
                        "description": "Repository root path. Defaults to the server working directory.",
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": f"Token budget for retrieved context (default {DEFAULT_RETRIEVE_BUDGET}).",
                    },
                },
                "required": ["query"],
            },
        },
        {
            "name": "peregrine_escalate",
            "title": "Peregrine cloud-boost escalation (consent-gated)",
            "description": (
                "Request escalating a hard, locally-unresolved task to the cloud "
                "boost tier. This never contacts a provider: it returns the "
                "escalation decision under the 'ask' policy, which requires explicit "
                "per-incident human consent (E-A1). Zero provider invocations."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task contract / reason to escalate.",
                    },
                    "feedback_retries": {
                        "type": "integer",
                        "description": "How many local verifier-feedback retries were already spent.",
                    },
                    "local_output_tokens": {
                        "type": "integer",
                        "description": "Local output tokens produced so far.",
                    },
                },
                "required": ["task"],
            },
        },
    ]


def _response(request_id: Any, result: dict[str, Any]) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def _error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def _call_retrieve(arguments: dict[str, Any]) -> dict[str, Any]:
    query = arguments.get("query")
    if not isinstance(query, str) or not query.strip():
        raise ValueError("'query' is required and must be a non-empty string")
    repo = arguments.get("repo")
    root = Path(repo) if isinstance(repo, str) and repo else Path(os.getcwd())
    if not root.is_dir():
        raise ValueError(f"repo is not a directory: {root}")
    budget = arguments.get("budget_tokens", DEFAULT_RETRIEVE_BUDGET)
    if isinstance(budget, bool) or not isinstance(budget, int) or budget < 1 or budget > DEFAULT_WINDOW_TOKENS * 8:
        raise ValueError("budget_tokens must be a positive integer within a sane range")
    result = retrieve_repo(root, query, budget_tokens=budget)
    context = assemble_context(result["selected"])
    manifest = {
        "query": result["query"],
        "budget_tokens": result["budget_tokens"],
        "files_scanned": result["files_scanned"],
        "chunks_total": result["chunks_total"],
        "tokens_used": result["tokens_used"],
        "selected": [
            {
                "file": c.file,
                "start_line": c.start_line,
                "end_line": c.end_line,
                "tokens": estimate_tokens(c.text),
            }
            for c in result["selected"]
        ],
    }
    text = context if context else "(no matching repository context)"
    return {
        "content": [{"type": "text", "text": text}],
        "structuredContent": manifest,
        "isError": False,
    }


def _call_escalate(arguments: dict[str, Any]) -> dict[str, Any]:
    task = arguments.get("task")
    if not isinstance(task, str) or not task.strip():
        raise ValueError("'task' is required and must be a non-empty string")
    retries = arguments.get("feedback_retries", 1)
    output_tokens = arguments.get("local_output_tokens", 0)
    for value, label in ((retries, "feedback_retries"), (output_tokens, "local_output_tokens")):
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise ValueError(f"{label} must be a non-negative integer")
    # A manual escalate request is trigger T4; the task is locally unresolved.
    context = TriggerContext(
        task_contract=task,
        local_diff="",
        summary=task,
        verifier_passed=False,
        verifier_output="",
        verifier_command=("<manual-escalation>",),
        accessed_paths=(),
        file_pointers=(),
        feedback_retries=retries,
        local_output_tokens=output_tokens,
        manual_boost=True,
    )
    # mode "ask" is the safe default: an escalation can only await human consent.
    decision = decide_escalation(context, mode="ask", findings=())
    # Hard invariant: this path never invokes a provider.
    assert decision.provider_invoked is False
    consent_required = decision.action == "AWAITING_CONSENT"
    manifest = {
        "mode": decision.mode,
        "action": decision.action,
        "triggers": list(decision.triggers),
        "provider_state": "not_invoked",
        "provider_invocations": 0,
        "m0a_admitted_tokens": 0,
        "consent_required": consent_required,
    }
    message = (
        "Cloud boost is consent-gated. This request did NOT contact any provider. "
        "To proceed, a human must grant explicit per-incident consent (E-A1); until "
        f"then the decision stays '{decision.action}'. Triggers: "
        f"{', '.join(decision.triggers) or 'none'}."
    )
    return {
        "content": [{"type": "text", "text": message}],
        "structuredContent": manifest,
        "isError": False,
    }


_TOOL_HANDLERS = {
    "peregrine_retrieve": _call_retrieve,
    "peregrine_escalate": _call_escalate,
}


def dispatch(message: dict[str, Any]) -> dict[str, Any] | None:
    """Handle one JSON-RPC message. Returns a response dict, or None for notifications."""
    method = message.get("method")
    request_id = message.get("id")
    # Notifications have no id and get no response.
    if request_id is None and isinstance(method, str) and method.startswith("notifications/"):
        return None

    if method == "initialize":
        return _response(
            request_id,
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            },
        )
    if method == "ping":
        return _response(request_id, {})
    if method == "tools/list":
        return _response(request_id, {"tools": tool_definitions()})
    if method == "tools/call":
        params = message.get("params") or {}
        name = params.get("name")
        arguments = params.get("arguments") or {}
        handler = _TOOL_HANDLERS.get(name)
        if handler is None:
            return _error(request_id, INVALID_PARAMS, f"Unknown tool: {name}")
        try:
            return _response(request_id, handler(arguments))
        except ValueError as error:
            return _error(request_id, INVALID_PARAMS, str(error))
        except (RetrievalError, EscalationError, OSError) as error:
            # Tool-execution error surfaced as an isError result, per spec.
            return _response(
                request_id,
                {"content": [{"type": "text", "text": f"retrieval failed: {error}"}], "isError": True},
            )

    if request_id is None:
        return None  # unknown notification
    return _error(request_id, METHOD_NOT_FOUND, f"Method not found: {method}")


def kilocode_mcp_config(
    python_executable: str = ".venv/bin/python", repo_root: str | None = None
) -> dict[str, Any]:
    """Kilo Code MCP registration snippet (current schema: `mcp` key, `command` array).

    Local (STDIO) server: `{type: local, command: [...], environment, enabled, timeout}`
    (global `~/.config/kilo/kilo.jsonc`, project `.kilo/kilo.jsonc`). When `repo_root` is
    given the command is wrapped so the server always starts FROM the repository root
    (so `bench.m1.mcp_server` imports and retrieval defaults to scanning the workspace),
    and PYTHONPATH is pinned — a zero-config, machine-specific block. Without it, the
    portable relative form is emitted (run from the repo root yourself).
    """
    server: dict[str, Any] = {"type": "local", "enabled": True, "timeout": 30000}
    if repo_root:
        import shlex

        launch = f"cd {shlex.quote(repo_root)} && exec {shlex.quote(python_executable)} -m bench.m1.mcp_server"
        server["command"] = ["/bin/sh", "-lc", launch]
        server["environment"] = {"PYTHONPATH": repo_root}
    else:
        server["command"] = [python_executable, "-m", "bench.m1.mcp_server"]
        server["environment"] = {}
    return {"mcp": {"peregrine": server}}


def serve_stdio(stdin=None, stdout=None) -> int:
    """Newline-delimited JSON-RPC over stdio (MCP stdio transport)."""
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict):
            continue
        response = dispatch(message)
        if response is not None:
            stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
            stdout.flush()
    return 0


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--emit-kilocode-config",
        action="store_true",
        help="print the Kilo Code MCP registration snippet and exit (no server)",
    )
    parser.add_argument(
        "--python",
        default=".venv/bin/python",
        help="interpreter path used in the emitted Kilo Code command array",
    )
    parser.add_argument(
        "--repo-root",
        default=None,
        help="absolute repo root; emits a zero-config block that starts the server from it",
    )
    args = parser.parse_args(argv)
    if args.emit_kilocode_config:
        print(json.dumps(kilocode_mcp_config(args.python, repo_root=args.repo_root), indent=2))
        return 0
    return serve_stdio()


if __name__ == "__main__":
    raise SystemExit(main())
