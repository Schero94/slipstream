"""Evaluate the bounded 8-10 GB expert-streaming gate on held-out traces."""

from __future__ import annotations

import argparse
from collections import Counter, OrderedDict, defaultdict
import hashlib
import json
import os
from pathlib import Path
from typing import Sequence

from bench.m0a.analyze_routing import (
    AnalysisError,
    _token_groups,
    chronological_split,
    eligible_routing_paths,
)
from bench.m0a.cache_sim import (
    AccessEvent,
    PolicyMetrics,
    build_static_pins,
    group_decode_events,
    projected_dev_capacity,
    simulate_static,
)
from bench.m0a.constants import (
    DEV_EXPERTS,
    DEV_LAYERS,
    DEV_TOP_K,
    INT4_EXPERT_BYTES,
    MIN_DECODE_TOKENS,
)
from bench.m0a.routing_format import iter_records, read_header


F0_BUDGETS_BYTES = (8_000_000_000, 9_000_000_000, 10_000_000_000)


def projected_capacities() -> dict[int, int]:
    return {
        budget: projected_dev_capacity(budget, INT4_EXPERT_BYTES)
        for budget in F0_BUDGETS_BYTES
    }


def classify_f0(hit_rate: float, *, incomplete: bool) -> str:
    if not 0.0 <= hit_rate <= 1.0:
        raise ValueError("hit_rate must be between zero and one")
    if hit_rate >= 0.90:
        band = "ELIGIBLE"
    elif hit_rate >= 0.85:
        band = "CONDITIONAL"
    else:
        band = "REJECTED"
    prefix = "F0_PROVISIONAL_" if incomplete else "F0_"
    return prefix + band


def _rankings(
    calibration_groups: Sequence[Sequence[AccessEvent]],
    *,
    layer_count: int,
    expert_count: int,
) -> tuple[dict[int, list[int]], dict[tuple[int, int], Counter[int]]]:
    marginals = {layer: Counter() for layer in range(layer_count)}
    pairs: dict[tuple[int, int], Counter[int]] = defaultdict(Counter)
    for group in calibration_groups:
        if len(group) != layer_count:
            raise ValueError("calibration group has invalid layer geometry")
        for event in group:
            if event.layer < 0 or event.layer >= layer_count or any(
                expert < 0 or expert >= expert_count for expert in event.experts
            ):
                raise ValueError("calibration group has invalid expert geometry")
            marginals[event.layer].update(event.experts)
        for layer in range(layer_count - 1):
            for source in group[layer].experts:
                pairs[(layer, source)].update(group[layer + 1].experts)
    rankings = {
        layer: sorted(
            range(expert_count),
            key=lambda expert: (-marginals[layer][expert], expert),
        )
        for layer in range(layer_count)
    }
    return rankings, pairs


def _initial_state(
    rankings: dict[int, list[int]], capacity_per_layer: int
) -> dict[int, OrderedDict[int, None]]:
    # The most frequent expert is inserted last so it starts as the MRU entry.
    return {
        layer: OrderedDict(
            (expert, None) for expert in reversed(ranking[:capacity_per_layer])
        )
        for layer, ranking in rankings.items()
    }


def _touch(cache: OrderedDict[int, None], experts: Sequence[int], capacity: int) -> None:
    for expert in experts:
        if expert in cache:
            cache.move_to_end(expert)
        elif capacity > 0:
            cache[expert] = None
            if len(cache) > capacity:
                cache.popitem(last=False)


def simulate_coupled_prefetch(
    calibration_groups: Sequence[Sequence[AccessEvent]],
    held_out_groups: Sequence[Sequence[AccessEvent]],
    *,
    layer_count: int,
    expert_count: int,
    capacity_per_layer: int,
    prefetch_width: int,
) -> PolicyMetrics:
    """Simulate an optimistic completed-before-use coupling prefetch ceiling."""

    if capacity_per_layer < 0 or prefetch_width <= 0:
        raise ValueError("capacity must be non-negative and prefetch width positive")
    rankings, pairs = _rankings(
        calibration_groups,
        layer_count=layer_count,
        expert_count=expert_count,
    )
    ranking_positions = {
        layer: {expert: position for position, expert in enumerate(ranking)}
        for layer, ranking in rankings.items()
    }
    state = _initial_state(rankings, capacity_per_layer)
    current_session = None
    hits = accesses = all_hit_events = layer_token_events = 0

    for group in held_out_groups:
        if len(group) != layer_count or [event.layer for event in group] != list(
            range(layer_count)
        ):
            raise ValueError("held-out group has invalid layer geometry")
        session = group[0].session_id
        if current_session is None:
            current_session = session
        elif session != current_session:
            current_session = session
            state = _initial_state(rankings, capacity_per_layer)

        for layer, target in enumerate(group):
            cache = state[layer]
            if layer > 0:
                scores: Counter[int] = Counter()
                for source in group[layer - 1].experts:
                    scores.update(pairs.get((layer - 1, source), Counter()))
                prediction = sorted(
                    scores,
                    key=lambda expert: (
                        -scores[expert],
                        ranking_positions[layer][expert],
                    ),
                )
                predicted = set(prediction)
                for expert in rankings[layer]:
                    if len(prediction) >= prefetch_width:
                        break
                    if expert not in predicted:
                        prediction.append(expert)
                        predicted.add(expert)
                prediction = prediction[:prefetch_width]
                _touch(cache, prediction, capacity_per_layer)

            vector = tuple(expert in cache for expert in target.experts)
            hits += sum(vector)
            accesses += len(vector)
            layer_token_events += 1
            all_hit_events += int(all(vector))
            _touch(cache, target.experts, capacity_per_layer)

    return PolicyMetrics(accesses, hits, layer_token_events, all_hit_events)


def evaluate_f0_events(
    events: Sequence[AccessEvent], *, allow_incomplete: bool
) -> dict[str, object]:
    groups = _token_groups(events)
    decode_tokens = len(groups)
    incomplete = decode_tokens < MIN_DECODE_TOKENS
    if incomplete and not allow_incomplete:
        raise AnalysisError(
            f"need at least {MIN_DECODE_TOKENS} decode tokens, got {decode_tokens}"
        )
    calibration, held_out = chronological_split(events)
    calibration_groups = _token_groups(calibration)
    held_out_groups = _token_groups(held_out)
    rows: list[dict[str, object]] = []
    for budget, capacity in projected_capacities().items():
        pins = build_static_pins(calibration, capacity)
        static = simulate_static(held_out, pins)
        coupled = simulate_coupled_prefetch(
            calibration_groups,
            held_out_groups,
            layer_count=DEV_LAYERS,
            expert_count=DEV_EXPERTS,
            capacity_per_layer=capacity,
            prefetch_width=DEV_TOP_K,
        )
        rows.append(
            {
                "budget_bytes": budget,
                "capacity_per_layer": capacity,
                "split": "held_out",
                "static_hit_rate": static.hit_rate,
                "coupled_prefetch_hit_rate": coupled.hit_rate,
                "coupled_prefetch_assumption": "completed_before_next_layer",
                "accesses": coupled.accesses,
            }
        )
    decision = classify_f0(
        float(rows[-1]["coupled_prefetch_hit_rate"]), incomplete=incomplete
    )
    return {
        "format_version": 1,
        "decode_tokens": decode_tokens,
        "calibration_fraction": 0.7,
        "incomplete": incomplete,
        "decision": decision,
        "decision_budget_bytes": F0_BUDGETS_BYTES[-1],
        "thresholds": {"reject_below": 0.85, "eligible_at": 0.90},
        "budgets": rows,
    }


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def evaluate_f0_paths(
    paths: Sequence[Path], *, model_sha256: str, allow_incomplete: bool
) -> dict[str, object]:
    if len(model_sha256) != 64 or any(c not in "0123456789abcdef" for c in model_sha256):
        raise AnalysisError("model_sha256 must be lowercase hexadecimal SHA-256")
    records = []
    inputs = []
    for path in paths:
        with path.open("rb") as stream:
            header = read_header(stream)
        if header.model_sha256.hex() != model_sha256:
            continue
        if (header.layer_count, header.expert_count, header.top_k) != (
            DEV_LAYERS,
            DEV_EXPERTS,
            DEV_TOP_K,
        ):
            raise AnalysisError(f"unexpected trace geometry in {path}")
        records.extend(iter_records(path))
        inputs.append({"path": str(path), "sha256": _sha256(path)})
    if not records:
        raise AnalysisError("no routing inputs match the selected model hash")
    result = evaluate_f0_events(
        group_decode_events(records), allow_incomplete=allow_incomplete
    )
    result["model_sha256"] = model_sha256
    result["inputs"] = inputs
    return result


def _write_json_atomic(path: Path, value: object) -> None:
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--logs-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--model-sha256", required=True)
    parser.add_argument("--allow-incomplete", action="store_true")
    args = parser.parse_args()
    try:
        paths = eligible_routing_paths(
            args.logs_dir, model_sha256=args.model_sha256
        )
        result = evaluate_f0_paths(
            paths,
            model_sha256=args.model_sha256,
            allow_incomplete=args.allow_incomplete,
        )
        args.output_dir.mkdir(parents=True, exist_ok=False)
        _write_json_atomic(args.output_dir / "f0-streaming-gate.json", result)
    except (AnalysisError, OSError, ValueError) as error:
        print(f"F0 failed: {error}")
        return 2
    print(json.dumps({"decision": result["decision"], "decode_tokens": result["decode_tokens"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
