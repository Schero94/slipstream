"""Validate coding-run timings and speculative-decoding evidence."""

from __future__ import annotations

import math
import re
from typing import Mapping, Sequence

from bench.m0a.admission_policy import (
    MAX_PEAK_RSS_KB,
    RESPONSE_WARNING_FLOOR,
    TARGET_DECODE_TOKENS_PER_SECOND,
    PolicyError,
    evaluate_session_policy,
)

DECODE_TOKENS_PER_SECOND_TOLERANCE = 1.0
MIN_DECODE_TOKENS_PER_SECOND = RESPONSE_WARNING_FLOOR
DRAFT_LINE = re.compile(
    r"draft acceptance = [0-9.]+\s+\(\s*(\d+) accepted /\s*(\d+) generated\)"
)
DECODE_TIMING_LINE = re.compile(r"\|\s+eval time\s+=.*?/\s*(\d+) tokens")
DECODE_RATE_LINE = re.compile(
    r"\|\s+eval time\s+=.*?\(.*?ms per token,\s*(\S+)\s+tokens per second\)"
)
DECODE_SAMPLE_LINE = re.compile(
    r"\|\s+eval time\s+=.*?/\s*(\d+) tokens\s+"
    r"\(.*?ms per token,\s*(\S+)\s+tokens per second\)"
)


class TelemetryError(ValueError):
    """Raised when runtime evidence is incomplete or internally inconsistent."""


def _non_negative_int(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise TelemetryError(f"{name} must be a non-negative integer")
    return value


def _positive_number(value: object, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value <= 0:
        raise TelemetryError(f"{name} must be a positive number")
    return float(value)


def _unit_interval(value: object, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise TelemetryError(f"{name} must be a number")
    normalized = float(value)
    if not 0.0 <= normalized <= 1.0:
        raise TelemetryError(f"{name} must be in [0, 1]")
    return normalized


def parse_response_timings(response: Mapping[str, object]) -> dict[str, object]:
    """Normalize llama.cpp response timings without inventing missing counters."""

    timings = response.get("timings")
    if not isinstance(timings, Mapping):
        raise TelemetryError("response has no timings mapping")
    decoded = _non_negative_int(timings.get("predicted_n"), "predicted_n")
    decode_speed = _positive_number(
        timings.get("predicted_per_second"), "predicted_per_second"
    )

    prompt_tokens = timings.get("prompt_n")
    if prompt_tokens is not None:
        prompt_tokens = _non_negative_int(prompt_tokens, "prompt_n")
    prompt_speed = timings.get("prompt_per_second")
    if prompt_speed is not None:
        prompt_speed = _positive_number(prompt_speed, "prompt_per_second")

    generated_raw = timings.get("draft_n")
    accepted_raw = timings.get("draft_n_accepted")
    if (generated_raw is None) != (accepted_raw is None):
        raise TelemetryError("draft counters must be present together")
    generated: int | None = None
    accepted: int | None = None
    acceptance: float | None = None
    if generated_raw is not None:
        generated = _non_negative_int(generated_raw, "draft_n")
        accepted = _non_negative_int(accepted_raw, "draft_n_accepted")
        if accepted > generated:
            raise TelemetryError("accepted draft count exceeds generated count")
        acceptance = accepted / generated if generated else None

    adaptive_names = (
        "draft_adaptive_n",
        "draft_adaptive_n_next",
        "draft_acceptance",
        "draft_acceptance_ewma",
    )
    adaptive_values = tuple(timings.get(name) for name in adaptive_names)
    if any(value is not None for value in adaptive_values) and not all(
        value is not None for value in adaptive_values
    ):
        raise TelemetryError("adaptive draft fields must be present together")
    adaptive_n: int | None = None
    adaptive_n_next: int | None = None
    adaptive_acceptance: float | None = None
    adaptive_ewma: float | None = None
    if all(value is not None for value in adaptive_values):
        adaptive_n = _non_negative_int(adaptive_values[0], adaptive_names[0])
        adaptive_n_next = _non_negative_int(adaptive_values[1], adaptive_names[1])
        if not 4 <= adaptive_n <= 12 or not 4 <= adaptive_n_next <= 12:
            raise TelemetryError("adaptive draft length must be in [4, 12]")
        adaptive_acceptance = _unit_interval(adaptive_values[2], adaptive_names[2])
        adaptive_ewma = _unit_interval(adaptive_values[3], adaptive_names[3])

    return {
        "prompt_tokens": prompt_tokens,
        "prompt_tokens_per_second": prompt_speed,
        "decoded_tokens": decoded,
        "decode_tokens_per_second": decode_speed,
        "draft_generated": generated,
        "draft_accepted": accepted,
        "draft_acceptance": acceptance,
        "adaptive_draft_n": adaptive_n,
        "adaptive_draft_n_next": adaptive_n_next,
        "adaptive_acceptance": adaptive_acceptance,
        "adaptive_acceptance_ewma": adaptive_ewma,
    }


def parse_log_draft_totals(text: str) -> dict[str, object]:
    """Sum llama.cpp draft counters from a session-local server log."""

    matches = DRAFT_LINE.findall(text)
    if not matches:
        return {
            "draft_generated": None,
            "draft_accepted": None,
            "draft_acceptance": None,
        }
    accepted = sum(int(match[0]) for match in matches)
    generated = sum(int(match[1]) for match in matches)
    if accepted > generated:
        raise TelemetryError("log accepted draft count exceeds generated count")
    return {
        "draft_generated": generated,
        "draft_accepted": accepted,
        "draft_acceptance": accepted / generated if generated else None,
    }


def parse_log_decode_tokens(text: str) -> int:
    """Count emitted tokens from llama.cpp final decode-timing lines."""

    counts = [int(value) for value in DECODE_TIMING_LINE.findall(text)]
    if not counts:
        raise TelemetryError("server log has no decode timing lines")
    return sum(counts)


def parse_log_decode_rates(text: str) -> tuple[float, ...]:
    """Extract ordered rates from every final llama.cpp decode-timing line."""

    raw_rates = DECODE_RATE_LINE.findall(text)
    decode_lines = DECODE_TIMING_LINE.findall(text)
    if not raw_rates or len(raw_rates) != len(decode_lines):
        raise TelemetryError("server log has missing decode-rate evidence")
    rates: list[float] = []
    for raw_rate in raw_rates:
        try:
            rate = float(raw_rate)
        except ValueError as error:
            raise TelemetryError("server log has an invalid decode rate") from error
        if not math.isfinite(rate) or rate <= 0:
            raise TelemetryError("server log has an invalid decode rate")
        rates.append(rate)
    return tuple(rates)


def parse_log_decode_samples(text: str) -> tuple[dict[str, object], ...]:
    """Extract response token counts and rates from final decode timing lines."""

    raw_samples = DECODE_SAMPLE_LINE.findall(text)
    decode_lines = DECODE_TIMING_LINE.findall(text)
    if not raw_samples or len(raw_samples) != len(decode_lines):
        raise TelemetryError("server log has missing decode-sample evidence")
    samples: list[dict[str, object]] = []
    for raw_tokens, raw_rate in raw_samples:
        try:
            tokens = int(raw_tokens)
            rate = float(raw_rate)
        except ValueError as error:
            raise TelemetryError("server log has an invalid decode sample") from error
        if tokens < 0 or not math.isfinite(rate) or rate <= 0:
            raise TelemetryError("server log has an invalid decode sample")
        samples.append(
            {"decoded_tokens": tokens, "decode_tokens_per_second": rate}
        )
    return tuple(samples)


def enforce_coding_gate(
    points: Sequence[Mapping[str, object]],
    *,
    minimum_speed: float = MIN_DECODE_TOKENS_PER_SECOND,
    maximum_rss_kb: int = MAX_PEAK_RSS_KB,
) -> dict[str, object]:
    """Compatibility wrapper around the shared S4 session policy."""

    try:
        decision = evaluate_session_policy(
            points,
            minimum_mean=minimum_speed,
            warning_floor=minimum_speed,
            maximum_rss_kb=maximum_rss_kb,
        )
    except PolicyError as error:
        raise TelemetryError(str(error)) from error
    # Preserve the historical field while exposing the explicit S4 fields.
    decision["minimum_decode_tokens_per_second"] = minimum_speed
    return decision
