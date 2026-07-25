"""Single executable speed and memory policy for Peregrine evidence."""

from __future__ import annotations

import math
from typing import Mapping, Sequence


TARGET_DECODE_TOKENS_PER_SECOND = 25.0
SESSION_MEAN_FLOOR = 24.0
SESSION_P10_FLOOR = 18.0
RESPONSE_WARNING_FLOOR = 24.0
MIN_SCORED_RESPONSE_TOKENS = 8
MAX_PEAK_RSS_KB = 31_000_000
QUALIFICATION_CONTEXTS = (4_000, 32_000, 64_000)
PROFILE_BUCKET_FLOORS = {64_000: 20.0}


class PolicyError(ValueError):
    pass


def percentile(values: Sequence[float], percentile_value: float) -> float:
    if not values:
        raise PolicyError("percentile requires evidence")
    ordered = sorted(values)
    position = percentile_value * (len(ordered) - 1)
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def evaluate_session_policy(
    points: Sequence[Mapping[str, object]],
    *,
    minimum_mean: float = SESSION_MEAN_FLOOR,
    minimum_p10: float = SESSION_P10_FLOOR,
    warning_floor: float = RESPONSE_WARNING_FLOOR,
    minimum_tokens: int = MIN_SCORED_RESPONSE_TOKENS,
    maximum_rss_kb: int = MAX_PEAK_RSS_KB,
) -> dict[str, object]:
    if not points:
        raise PolicyError("session policy requires at least one measurement")
    reasons: list[str] = []
    warnings: list[str] = []
    speeds: list[float] = []
    excluded = 0
    for index, point in enumerate(points):
        try:
            tokens = int(point["decoded_tokens"])
            speed = float(point["decode_tokens_per_second"])
            rss = int(point["peak_rss_kb"])
        except (KeyError, TypeError, ValueError) as error:
            raise PolicyError(f"invalid session point at index {index}") from error
        if tokens < 0 or not math.isfinite(speed) or speed <= 0 or rss <= 0:
            raise PolicyError(f"invalid session point at index {index}")
        if rss > maximum_rss_kb:
            reasons.append(f"rss@{index}")
        if tokens < minimum_tokens:
            excluded += 1
            continue
        speeds.append(speed)
        if speed < warning_floor:
            warnings.append(f"throughput@{index}")
    mean_speed = math.fsum(speeds) / len(speeds) if speeds else None
    p10_speed = percentile(speeds, 0.10) if speeds else None
    if not speeds:
        reasons.insert(0, "no-scored-responses")
    else:
        if mean_speed is not None and mean_speed < minimum_mean:
            reasons.insert(0, "mean-throughput")
        if p10_speed is not None and p10_speed < minimum_p10:
            reasons.insert(0, "p10-throughput")
    return {
        "passed": not reasons,
        "reasons": reasons,
        "warnings": warnings,
        "mean_decode_tokens_per_second": mean_speed,
        "p10_decode_tokens_per_second": p10_speed,
        "response_count": len(points),
        "scored_response_count": len(speeds),
        "excluded_short_response_count": excluded,
        "responses_below_minimum": len(warnings),
        "target_decode_tokens_per_second": TARGET_DECODE_TOKENS_PER_SECOND,
        "minimum_mean_decode_tokens_per_second": minimum_mean,
        "minimum_p10_decode_tokens_per_second": minimum_p10,
        "response_warning_floor": warning_floor,
        "minimum_scored_response_tokens": minimum_tokens,
        "maximum_peak_rss_kb": maximum_rss_kb,
    }


def evaluate_qualification_policy(
    points: Sequence[Mapping[str, object]],
    *,
    contexts: Sequence[int] = QUALIFICATION_CONTEXTS,
    decode_tokens: int = 128,
    minimum_mean: float = SESSION_MEAN_FLOOR,
    maximum_rss_kb: int = MAX_PEAK_RSS_KB,
    bucket_floors: Mapping[int, float] = PROFILE_BUCKET_FLOORS,
) -> dict[str, object]:
    by_context: dict[int, Mapping[str, object]] = {}
    reasons: list[str] = []
    warnings: list[str] = []
    speeds: list[float] = []
    for point in points:
        try:
            context = int(point["context_tokens"])
        except (KeyError, TypeError, ValueError) as error:
            raise PolicyError("qualification point has no valid context") from error
        if context in by_context:
            reasons.append(f"duplicate-context@{context}")
        by_context[context] = point
    if set(by_context) != set(contexts):
        reasons.append("context-matrix")
    for context in contexts:
        point = by_context.get(context)
        if point is None:
            continue
        try:
            decoded = int(point["decoded_tokens"])
            speed = float(point["decode_tokens_per_second"])
            rss = int(point["peak_rss_kb"])
        except (KeyError, TypeError, ValueError) as error:
            raise PolicyError(f"invalid qualification point at {context}") from error
        if decoded != decode_tokens or point.get("stop_type") != "limit":
            reasons.append(f"decode-count@{context}")
        if not math.isfinite(speed) or speed <= 0:
            reasons.append(f"invalid-throughput@{context}")
        else:
            speeds.append(speed)
            if speed < RESPONSE_WARNING_FLOOR:
                warnings.append(f"throughput@{context}")
            floor = bucket_floors.get(context)
            if floor is not None and speed < floor:
                reasons.append(f"bucket-throughput@{context}")
        if rss <= 0:
            raise PolicyError(f"invalid qualification RSS at {context}")
        if rss > maximum_rss_kb:
            reasons.append(f"rss@{context}")
    mean_speed = math.fsum(speeds) / len(speeds) if speeds else None
    if len(speeds) == len(contexts) and mean_speed is not None and mean_speed < minimum_mean:
        reasons.append("mean-throughput")
    return {
        "passed": not reasons,
        "reasons": reasons,
        "warnings": warnings,
        "mean_decode_tokens_per_second": mean_speed,
        "target_decode_tokens_per_second": TARGET_DECODE_TOKENS_PER_SECOND,
        "minimum_decode_tokens_per_second": minimum_mean,
        "maximum_peak_rss_kb": maximum_rss_kb,
        "profile_bucket_floors": {str(key): value for key, value in bucket_floors.items()},
    }
