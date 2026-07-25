"""Reliability ladder — the quality core of the offline coding engine.

Encodes Plan v3's tiered ladder as reusable, tested logic:

1. Stufe 1 (fast local): generate a solution, run the hidden verifier.
2. If it fails, feed the exact verifier failure back ONCE (T1 — proven to lift
   3/6 -> 4/6) and re-verify. Bounded to a single evidence-based retry; no blind
   retries.
3. If it still fails, mark `escalation_required` — a consent-gated Stufe 3 cloud
   boost. The escalate hook is NEVER allowed to auto-invoke a provider; it may
   only report `AWAITING_CONSENT` with zero provider invocations.

Pure and dependency-free: `generate`, `verify`, and `escalate` are injected, so
this composes with the gateway, the verifier corpus, and the escalation MCP tool
without importing any of them.
"""

from __future__ import annotations

from typing import Any, Callable

# generate(task, feedback) -> output text; feedback is None on the first pass,
# else the exact verifier failure to repair against.
GenerateFn = Callable[[str, str | None], str]
# verify(output) -> (passed: bool, failure_detail: str)
VerifyFn = Callable[[str], "tuple[bool, str]"]
# escalate(task, last_output) -> dict; MUST be consent-gated (no provider call).
EscalateFn = Callable[[str, str], dict[str, Any]]


class LadderError(Exception):
    """Raised for invalid ladder configuration."""


def _default_escalation() -> dict[str, Any]:
    return {"action": "AWAITING_CONSENT", "consent": "required", "provider_invocations": 0}


def run_ladder(
    task: str,
    *,
    generate: GenerateFn,
    verify: VerifyFn,
    max_retries: int = 1,
    escalate: EscalateFn | None = None,
) -> dict[str, Any]:
    if not task:
        raise LadderError("task must be non-empty")
    if max_retries not in (0, 1):
        raise LadderError("max_retries must be 0 or 1 (exactly one evidence-based retry)")

    attempts = 0
    # --- Stufe 1: first pass ---
    output = generate(task, None)
    attempts += 1
    passed, detail = verify(output)
    if passed:
        return {
            "outcome": "solved_first_pass",
            "attempts": attempts,
            "retry_used": False,
            "first_pass": True,
        }

    # --- T1: exactly one evidence-based retry, fed the verifier failure ---
    if max_retries >= 1:
        output = generate(task, detail)
        attempts += 1
        passed, detail = verify(output)
        if passed:
            return {
                "outcome": "solved_after_retry",
                "attempts": attempts,
                "retry_used": True,
                "first_pass": False,
            }

    # --- Stufe 3: consent-gated escalation (never auto-invokes a provider) ---
    escalation = _default_escalation()
    if escalate is not None:
        reported = escalate(task, output)
        # enforce the hard invariant regardless of what the hook returned
        if isinstance(reported, dict) and int(reported.get("provider_invocations", 0)) != 0:
            raise LadderError("escalate hook reported a provider invocation; ladder forbids it")
        if isinstance(reported, dict):
            escalation = {**escalation, **reported, "provider_invocations": 0}
    return {
        "outcome": "escalation_required",
        "attempts": attempts,
        "retry_used": max_retries >= 1,
        "first_pass": False,
        "last_failure": detail,
        "escalation": escalation,
    }
