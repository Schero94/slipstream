"""Gateway client — connects the engine spine to the real resident gateway.

Thin OpenAI-compatible client for the Peregrine gateway (`/v1/chat/completions`)
that returns the generated text plus the measured decode tok/s. `make_generate`
adapts it into the `generate(task, context, feedback)` callable the engine spine
expects, building a coding prompt from the retrieved context and (on a retry) the
verifier failure. The HTTP call is injectable (`request_fn`) so this is tested
without a live server.
"""

from __future__ import annotations

import json
import urllib.request
from typing import Any, Callable

DEFAULT_GATEWAY_URL = "http://127.0.0.1:8080"
DEFAULT_MODEL = "peregrine-qualification"

RequestFn = Callable[[str, bytes, dict], bytes]


class GatewayClientError(Exception):
    """Raised when a gateway completion cannot be produced."""


def _urllib_request(url: str, data: bytes, headers: dict) -> bytes:
    req = urllib.request.Request(url, data=data, headers=headers)
    with urllib.request.urlopen(req, timeout=180) as resp:
        return resp.read()


def complete(
    prompt: str,
    *,
    gateway_url: str = DEFAULT_GATEWAY_URL,
    model: str = DEFAULT_MODEL,
    max_tokens: int = 512,
    request_fn: RequestFn | None = None,
) -> dict[str, Any]:
    if not prompt or not prompt.strip():
        raise GatewayClientError("prompt must be non-empty")
    request_fn = request_fn or _urllib_request
    body = json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens,
            "temperature": 0,
        }
    ).encode("utf-8")
    headers = {"Content-Type": "application/json", "Authorization": "Bearer local"}
    try:
        raw = request_fn(f"{gateway_url}/v1/chat/completions", body, headers)
        data = json.loads(raw.decode("utf-8", "replace"), strict=False)
        message = data["choices"][0]["message"]
    except (OSError, ValueError, KeyError, IndexError) as error:
        raise GatewayClientError(f"gateway completion failed: {error}") from error
    content = message.get("content") or message.get("reasoning_content", "")
    timings = data.get("timings", {})
    return {
        "content": content,
        "decode_tok_s": round(float(timings.get("predicted_per_second", 0.0)), 2),
        "tokens": data.get("usage", {}).get("completion_tokens"),
    }


_SYSTEM = (
    "You are a precise local coding assistant. Use the repository context below. "
    "Prefer minimal unified-diff edits over rewriting whole files."
)


def _build_prompt(task: str, context: str, feedback: str | None) -> str:
    parts = [_SYSTEM, "", "## Repository context", context or "(none retrieved)", "", "## Task", task]
    if feedback:
        parts += ["", "## Previous attempt failed the verifier — fix exactly this", feedback]
    return "\n".join(parts)


def make_generate(
    *,
    gateway_url: str = DEFAULT_GATEWAY_URL,
    model: str = DEFAULT_MODEL,
    max_tokens: int = 512,
    request_fn: RequestFn | None = None,
) -> Callable[[str, str, "str | None"], str]:
    """Return a `generate(task, context, feedback)` backed by the gateway."""

    def generate(task: str, context: str, feedback: str | None) -> str:
        prompt = _build_prompt(task, context, feedback)
        return complete(
            prompt, gateway_url=gateway_url, model=model, max_tokens=max_tokens, request_fn=request_fn
        )["content"]

    return generate
