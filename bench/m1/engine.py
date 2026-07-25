"""Peregrine engine — the coherent coding-turn spine.

Composes the built pieces into one flow, the "sweet spot of logic/structure" for
an offline coding turn:

    retrieve bounded context  →  Stufe-1 generate  →  verify
                              →  one T1 retry (fed the failure)
                              →  consent-gated escalation

Retrieval (`bench.m1.retrieval`) supplies the bounded active-window context; the
reliability ladder (`bench.m1.reliability_ladder`) runs the verify/retry/escalate
logic. `generate` and `verify` are injected — in production `generate` calls the
resident gateway and `verify` runs the hidden verifier — so this spine stays pure
and testable and imposes no new dependency. It is agent-agnostic: an external
agent can drive the same flow via the MCP tools instead.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable

from bench.m1.reliability_ladder import run_ladder
from bench.m1.retrieval import assemble_context, retrieve_repo

# generate(task, context, feedback) -> output. feedback is None on the first pass.
TurnGenerateFn = Callable[[str, str, "str | None"], str]
VerifyFn = Callable[[str], "tuple[bool, str]"]
EscalateFn = Callable[[str, str], dict[str, Any]]


class EngineError(Exception):
    """Raised for invalid coding-turn inputs."""


def run_coding_turn(
    task: str,
    repo_root: Path,
    *,
    generate: TurnGenerateFn,
    verify: VerifyFn,
    budget_tokens: int = 8000,
    max_retries: int = 1,
    escalate: EscalateFn | None = None,
) -> dict[str, Any]:
    if not task or not task.strip():
        raise EngineError("task must be non-empty")
    if not repo_root.is_dir():
        raise EngineError(f"repo_root is not a directory: {repo_root}")

    # 1. bounded active-window context via deterministic retrieval
    retrieval = retrieve_repo(repo_root, task, budget_tokens=budget_tokens)
    context = assemble_context(retrieval["selected"])

    # 2-4. ladder: generate (with context) -> verify -> one T1 retry -> escalate
    def gen(task_text: str, feedback: str | None) -> str:
        return generate(task_text, context, feedback)

    result = run_ladder(task, generate=gen, verify=verify, max_retries=max_retries, escalate=escalate)

    result["retrieval"] = {
        "files_scanned": retrieval["files_scanned"],
        "chunks_total": retrieval["chunks_total"],
        "tokens_used": retrieval["tokens_used"],
        "context_files": [c.file for c in retrieval["selected"]],
    }
    return result
