"""Analyze validated M0a traces without calibration/holdout leakage."""

from __future__ import annotations

import argparse
from collections import OrderedDict
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import random
from typing import Iterable, Mapping, Sequence

from bench.m0a.cache_sim import (
    AccessEvent,
    PolicyMetrics,
    build_global_static_pins,
    build_static_pins,
    group_decode_events,
    projected_dev_capacity,
    simulate_lru,
    simulate_pin_lru,
    simulate_static,
)
from bench.m0a.constants import (
    BUDGETS_BYTES,
    DEV_EXPERTS,
    DEV_LAYERS,
    DEV_TOP_K,
    FLAGSHIP_EXPERTS,
    FLAGSHIP_LAYERS,
    GREEN_HIT_RATE,
    INT2_EXPERT_BYTES,
    INT4_EXPERT_BYTES,
    MIN_DECODE_TOKENS,
    RED_HIT_RATE,
)
from bench.m0a.coupling import analyze_coupling
from bench.m0a.report import (
    write_coupling_csv,
    write_convergence_csv,
    write_hit_rates_csv,
    write_hit_rates_svg,
)
from bench.m0a.routing_format import iter_records, read_header


class AnalysisError(ValueError):
    """Raised when evidence is incomplete or incompatible."""


TokenKey = tuple[object, int, int]


@dataclass(frozen=True)
class AnalysisResult:
    summary: dict[str, object]
    hit_rate_rows: list[dict[str, object]]
    convergence_rows: list[dict[str, object]]
    coupling_rows: list[dict[str, object]]


def _token_groups(events: Sequence[AccessEvent]) -> list[list[AccessEvent]]:
    groups: list[list[AccessEvent]] = []
    previous: TokenKey | None = None
    for event in events:
        key: TokenKey = (event.session_id, event.batch_id, event.token_pos)
        if key != previous:
            groups.append([])
            previous = key
        groups[-1].append(event)
    for group in groups:
        if len(group) != DEV_LAYERS or [event.layer for event in group] != list(
            range(DEV_LAYERS)
        ):
            raise AnalysisError("events are not complete ordered decode-token groups")
    return groups


def chronological_split(
    events: Sequence[AccessEvent],
    calibration_fraction: float = 0.70,
) -> tuple[list[AccessEvent], list[AccessEvent]]:
    if not 0.0 < calibration_fraction < 1.0:
        raise ValueError("calibration_fraction must be between zero and one")
    groups = _token_groups(events)
    if len(groups) < 2:
        raise AnalysisError("at least two complete decode tokens are required")
    split_at = max(1, min(len(groups) - 1, int(len(groups) * calibration_fraction)))
    calibration = [event for group in groups[:split_at] for event in group]
    held_out = [event for group in groups[split_at:] for event in group]
    return calibration, held_out


def _percentile(sorted_values: Sequence[float], percentile: float) -> float:
    if len(sorted_values) == 1:
        return sorted_values[0]
    position = percentile * (len(sorted_values) - 1)
    lower = int(position)
    upper = min(lower + 1, len(sorted_values) - 1)
    fraction = position - lower
    return sorted_values[lower] * (1 - fraction) + sorted_values[upper] * fraction


def bootstrap_interval(
    samples: Sequence[tuple[int, int]],
    seed: int = 42,
    draws: int = 1000,
) -> tuple[float, float]:
    if not samples or draws <= 0 or any(accesses <= 0 for _, accesses in samples):
        raise ValueError("bootstrap requires samples with positive accesses and draws")
    rng = random.Random(seed)
    rates: list[float] = []
    for _ in range(draws):
        selected = [samples[rng.randrange(len(samples))] for _ in samples]
        hits = sum(item[0] for item in selected)
        accesses = sum(item[1] for item in selected)
        rates.append(hits / accesses)
    rates.sort()
    return _percentile(rates, 0.025), _percentile(rates, 0.975)


def classify_primary(hit_rate: float) -> str:
    if hit_rate >= GREEN_HIT_RATE:
        return "green"
    if hit_rate >= RED_HIT_RATE:
        return "yellow"
    return "red"


def _warm_lru_state(
    events: Sequence[AccessEvent], capacity: int
) -> dict[int, OrderedDict[int, None]]:
    state: dict[int, OrderedDict[int, None]] = {}
    for event in events:
        cache = state.setdefault(event.layer, OrderedDict())
        for expert in event.experts:
            if expert in cache:
                cache.move_to_end(expert)
            elif capacity > 0:
                cache[expert] = None
                if len(cache) > capacity:
                    cache.popitem(last=False)
    return state


def _mixed_capacity(budget_bytes: int) -> int:
    hot_budget = min(budget_bytes, 8_000_000_000)
    cold_budget = max(0, budget_bytes - hot_budget)
    hot = (hot_budget // FLAGSHIP_LAYERS) // INT4_EXPERT_BYTES
    cold = (cold_budget // FLAGSHIP_LAYERS) // INT2_EXPERT_BYTES
    fraction = min(1.0, (hot + cold) / FLAGSHIP_EXPERTS)
    return min(DEV_EXPERTS, int(fraction * DEV_EXPERTS))


def _row(
    policy: str,
    precision: str,
    budget: int,
    capacity: int,
    metrics: PolicyMetrics,
    *,
    pin_fraction: str = "",
) -> dict[str, object]:
    return {
        "policy": policy,
        "precision": precision,
        "budget_bytes": budget,
        "split": "held_out",
        "capacity_per_layer": capacity,
        "pin_fraction": pin_fraction,
        "hits": metrics.hits,
        "accesses": metrics.accesses,
        "hit_rate": f"{metrics.hit_rate:.12f}",
        "all_hit_events": metrics.all_hit_events,
        "layer_token_events": metrics.layer_token_events,
        "all_hit_rate": f"{metrics.all_hit_rate:.12f}",
    }


def _primary_blocks(
    held_out: Sequence[AccessEvent],
    pins: Mapping[int, frozenset[int]],
) -> list[tuple[int, int]]:
    groups = _token_groups(held_out)
    samples: list[tuple[int, int]] = []
    for start in range(0, len(groups), 1024):
        block = [event for group in groups[start : start + 1024] for event in group]
        metrics = simulate_static(block, pins)
        samples.append((metrics.hits, metrics.accesses))
    return samples


def _convergence(events: Sequence[AccessEvent]) -> list[dict[str, object]]:
    groups = _token_groups(events)
    points = list(range(10_000, len(groups) + 1, 10_000))
    if not points or points[-1] != len(groups):
        points.append(len(groups))
    rows: list[dict[str, object]] = []
    capacity = projected_dev_capacity(24_000_000_000, INT4_EXPERT_BYTES)
    for point in points:
        prefix = [event for group in groups[:point] for event in group]
        calibration, held_out = chronological_split(prefix)
        pins = build_static_pins(calibration, capacity)
        metrics = simulate_static(held_out, pins)
        rows.append(
            {
                "decode_tokens": point,
                "calibration_tokens": len(_token_groups(calibration)),
                "held_out_tokens": len(_token_groups(held_out)),
                "hit_rate": f"{metrics.hit_rate:.12f}",
            }
        )
    return rows


def analyze_events(
    events: Sequence[AccessEvent],
    *,
    allow_incomplete: bool,
    inputs: Sequence[Mapping[str, object]] = (),
) -> AnalysisResult:
    decode_tokens = len(_token_groups(events))
    if decode_tokens < MIN_DECODE_TOKENS and not allow_incomplete:
        raise AnalysisError(
            f"need at least {MIN_DECODE_TOKENS} decode tokens, got {decode_tokens}"
        )
    calibration, held_out = chronological_split(events)
    coupling = analyze_coupling(
        _token_groups(calibration),
        _token_groups(held_out),
        layer_count=DEV_LAYERS,
        expert_count=DEV_EXPERTS,
    )
    rows: list[dict[str, object]] = []
    primary_metrics: PolicyMetrics | None = None
    primary_pins: Mapping[int, frozenset[int]] | None = None

    for budget in BUDGETS_BYTES:
        capacity = projected_dev_capacity(budget, INT4_EXPERT_BYTES)
        pins = build_static_pins(calibration, capacity)
        static_metrics = simulate_static(held_out, pins)
        rows.append(_row("static_equal_per_layer", "int4", budget, capacity, static_metrics))

        global_pins = build_global_static_pins(calibration, capacity * DEV_LAYERS)
        rows.append(
            _row(
                "static_global",
                "int4",
                budget,
                capacity,
                simulate_static(held_out, global_pins),
            )
        )
        rows.append(
            _row(
                "lru_cold",
                "int4",
                budget,
                capacity,
                simulate_lru(held_out, capacity, reset_on_session=True),
            )
        )
        warm_state = _warm_lru_state(calibration, capacity)
        rows.append(
            _row(
                "lru_persisted_warm",
                "int4",
                budget,
                capacity,
                simulate_lru(
                    held_out,
                    capacity,
                    initial=warm_state,
                    reset_on_session=False,
                ),
            )
        )

        candidates = []
        for pin_fraction in (0.25, 0.50, 0.75):
            pin_capacity = int(capacity * pin_fraction)
            candidate_pins = build_static_pins(calibration, pin_capacity)
            candidate_metrics = simulate_pin_lru(
                calibration,
                candidate_pins,
                capacity - pin_capacity,
                reset_on_session=True,
            )
            candidates.append(
                (candidate_metrics.hit_rate, -pin_fraction, pin_fraction, candidate_pins)
            )
        _, _, selected_fraction, selected_pins = max(candidates)
        pin_capacity = int(capacity * selected_fraction)
        rows.append(
            _row(
                "pin_lru",
                "int4",
                budget,
                capacity,
                simulate_pin_lru(
                    held_out,
                    selected_pins,
                    capacity - pin_capacity,
                    reset_on_session=True,
                ),
                pin_fraction=f"{selected_fraction:.2f}",
            )
        )

        mixed_capacity = _mixed_capacity(budget)
        mixed_pins = build_static_pins(calibration, mixed_capacity)
        rows.append(
            _row(
                "static_equal_per_layer",
                "int4_int2_tail",
                budget,
                mixed_capacity,
                simulate_static(held_out, mixed_pins),
            )
        )

        if budget == 24_000_000_000:
            primary_metrics = static_metrics
            primary_pins = pins

    assert primary_metrics is not None and primary_pins is not None
    ci_low, ci_high = bootstrap_interval(_primary_blocks(held_out, primary_pins))
    summary: dict[str, object] = {
        "format_version": 2,
        "decode_tokens": decode_tokens,
        "calibration_fraction": 0.7,
        "incomplete": decode_tokens < MIN_DECODE_TOKENS,
        "inputs": list(inputs),
        "coupling": {
            "source": "colibri-pr-176",
            "calibration_only": True,
            "dependence": coupling.dependence_rows,
            "recall": coupling.recall_rows,
        },
        "primary": {
            "policy": "static_equal_per_layer",
            "precision": "int4",
            "budget_bytes": 24_000_000_000,
            "split": "held_out",
            "hits": primary_metrics.hits,
            "accesses": primary_metrics.accesses,
            "hit_rate": primary_metrics.hit_rate,
            "ci95": [ci_low, ci_high],
            "all_hit_rate": primary_metrics.all_hit_rate,
            "band": classify_primary(primary_metrics.hit_rate),
        },
    }
    coupling_rows = [
        {"row_type": "dependence", **row} for row in coupling.dependence_rows
    ] + [{"row_type": "recall", **row} for row in coupling.recall_rows]
    return AnalysisResult(summary, rows, _convergence(events), coupling_rows)


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def eligible_routing_paths(
    logs_dir: Path,
    *,
    model_sha256: str | None = None,
) -> list[Path]:
    """Select finalized, non-rejected routing files through their sidecars."""

    sidecar_paths = sorted(logs_dir.rglob("routing-*.json"))
    if not sidecar_paths:
        return sorted(logs_dir.rglob("routing-*.bin"))
    selected: list[Path] = []
    seen: set[Path] = set()
    for sidecar_path in sidecar_paths:
        try:
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AnalysisError(f"invalid sidecar {sidecar_path}: {error}") from error
        if not isinstance(sidecar, dict):
            raise AnalysisError(f"sidecar is not a JSON object: {sidecar_path}")
        if model_sha256 is not None and sidecar.get("model_sha256") != model_sha256:
            continue
        status = sidecar.get("status")
        if status in ("running", "rejected"):
            continue
        if status not in ("complete", "interrupted"):
            raise AnalysisError(f"invalid session status in {sidecar_path}: {status}")
        routing_value = sidecar.get("routing_path")
        if not isinstance(routing_value, str):
            raise AnalysisError(f"sidecar has no routing path: {sidecar_path}")
        routing_path = Path(routing_value)
        if not routing_path.is_absolute():
            routing_path = sidecar_path.parent / routing_path
        if not routing_path.is_file():
            raise AnalysisError(f"routing file is missing: {routing_path}")
        resolved = routing_path.resolve()
        if resolved in seen:
            raise AnalysisError(f"duplicate routing path in sidecars: {routing_path}")
        seen.add(resolved)
        selected.append(routing_path)
    return selected


def analyze_paths(
    paths: Sequence[Path],
    *,
    allow_incomplete: bool,
    model_sha256: str | None = None,
) -> AnalysisResult:
    if model_sha256 is not None and (
        len(model_sha256) != 64
        or any(character not in "0123456789abcdef" for character in model_sha256)
    ):
        raise AnalysisError(
            "model_sha256 must be exactly 64 lowercase hexadecimal characters"
        )
    if not paths:
        raise AnalysisError("no routing-*.bin files found")
    records = []
    expected_identity: tuple[bytes, int, int, int] | None = None
    inputs: list[dict[str, object]] = []
    for path in paths:
        with path.open("rb") as stream:
            header = read_header(stream)
        if model_sha256 is not None and header.model_sha256.hex() != model_sha256:
            continue
        identity = (
            header.model_sha256,
            header.layer_count,
            header.expert_count,
            header.top_k,
        )
        if identity[1:] != (DEV_LAYERS, DEV_EXPERTS, DEV_TOP_K):
            raise AnalysisError(f"unexpected development geometry in {path}")
        if expected_identity is None:
            expected_identity = identity
        elif identity != expected_identity:
            raise AnalysisError("routing inputs mix model hashes or geometries")
        records.extend(iter_records(path))
        inputs.append(
            {
                "path": str(path),
                "bytes": path.stat().st_size,
                "sha256": _sha256(path),
                "session_id": str(header.session_id),
            }
        )
    if not records:
        raise AnalysisError("no routing inputs match the selected model hash")
    return analyze_events(
        group_decode_events(records),
        allow_incomplete=allow_incomplete,
        inputs=inputs,
    )


def _write_json_atomic(path: Path, value: object) -> None:
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--logs-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--allow-incomplete", action="store_true")
    parser.add_argument(
        "--model-sha256",
        help="only include routing files matching this lowercase SHA-256",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    paths = eligible_routing_paths(
        args.logs_dir,
        model_sha256=args.model_sha256,
    )
    try:
        result = analyze_paths(
            paths,
            allow_incomplete=args.allow_incomplete,
            model_sha256=args.model_sha256,
        )
        args.output_dir.mkdir(parents=True, exist_ok=False)
        _write_json_atomic(args.output_dir / "summary.json", result.summary)
        write_hit_rates_csv(args.output_dir / "hit_rates.csv", result.hit_rate_rows)
        write_convergence_csv(
            args.output_dir / "convergence.csv", result.convergence_rows
        )
        write_coupling_csv(args.output_dir / "coupling.csv", result.coupling_rows)
        write_hit_rates_svg(args.output_dir / "hit_rates.svg", result.hit_rate_rows)
    except (AnalysisError, OSError, ValueError) as error:
        print(f"analysis failed: {error}")
        return 2
    print(json.dumps(result.summary["primary"], sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
