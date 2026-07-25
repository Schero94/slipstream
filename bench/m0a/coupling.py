"""Calibration-only cross-layer routing coupling metrics."""

from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass
from typing import Sequence

from bench.m0a.cache_sim import AccessEvent


@dataclass(frozen=True)
class CouplingResult:
    dependence_rows: list[dict[str, object]]
    recall_rows: list[dict[str, object]]


def _percentile(sorted_values: Sequence[float], percentile: float) -> float:
    if len(sorted_values) == 1:
        return sorted_values[0]
    position = percentile * (len(sorted_values) - 1)
    lower = int(position)
    upper = min(lower + 1, len(sorted_values) - 1)
    fraction = position - lower
    return sorted_values[lower] * (1.0 - fraction) + sorted_values[upper] * fraction


def _validate_groups(
    groups: Sequence[Sequence[AccessEvent]],
    *,
    layer_count: int,
    expert_count: int,
) -> None:
    if not groups:
        raise ValueError("coupling analysis requires non-empty token groups")
    for group in groups:
        if len(group) != layer_count or [event.layer for event in group] != list(
            range(layer_count)
        ):
            raise ValueError("coupling token group has invalid layer geometry")
        for event in group:
            if not event.experts or any(
                expert < 0 or expert >= expert_count for expert in event.experts
            ):
                raise ValueError("coupling token group has invalid expert IDs")


def analyze_coupling(
    calibration_groups: Sequence[Sequence[AccessEvent]],
    held_out_groups: Sequence[Sequence[AccessEvent]],
    *,
    layer_count: int,
    expert_count: int,
    budgets: Sequence[int] = (8, 16, 32),
) -> CouplingResult:
    """Learn pair counts on calibration and score only held-out token groups."""

    if layer_count < 2 or expert_count <= 0:
        raise ValueError("coupling analysis requires valid model geometry")
    if not budgets or any(budget <= 0 or budget > expert_count for budget in budgets):
        raise ValueError("coupling budgets must fit the expert geometry")
    _validate_groups(
        calibration_groups,
        layer_count=layer_count,
        expert_count=expert_count,
    )
    _validate_groups(
        held_out_groups,
        layer_count=layer_count,
        expert_count=expert_count,
    )

    marginals = {layer: Counter() for layer in range(layer_count)}
    pairs: dict[tuple[int, int, int], Counter[int]] = defaultdict(Counter)
    for group in calibration_groups:
        for event in group:
            marginals[event.layer].update(event.experts)
        for depth in (1, 2):
            for layer in range(layer_count - depth):
                for source in group[layer].experts:
                    pairs[(layer, depth, source)].update(
                        group[layer + depth].experts
                    )

    dependence_rows: list[dict[str, object]] = []
    sample_count = len(calibration_groups)
    for depth in (1, 2):
        lifts: list[float] = []
        bound_ratios: list[float] = []
        for (layer, pair_depth, source), targets in pairs.items():
            if pair_depth != depth:
                continue
            source_count = marginals[layer][source]
            for target, coactivations in targets.items():
                target_count = marginals[layer + depth][target]
                lifts.append(
                    coactivations * sample_count / (source_count * target_count)
                )
                bound_ratios.append(
                    coactivations / min(source_count, target_count)
                )
        lifts.sort()
        bound_ratios.sort()
        if not lifts:
            continue
        dependence_rows.append(
            {
                "depth": depth,
                "observed_pairs": len(lifts),
                "lift_median": _percentile(lifts, 0.50),
                "lift_p90": _percentile(lifts, 0.90),
                "lift_p99": _percentile(lifts, 0.99),
                "frechet_above_50_fraction": sum(
                    value > 0.50 for value in bound_ratios
                )
                / len(bound_ratios),
                "frechet_above_90_fraction": sum(
                    value > 0.90 for value in bound_ratios
                )
                / len(bound_ratios),
            }
        )

    marginal_rankings = {
        layer: sorted(
            range(expert_count),
            key=lambda expert: (-marginals[layer][expert], expert),
        )
        for layer in range(layer_count)
    }
    recall_rows: list[dict[str, object]] = []
    for depth in (1, 2):
        if depth >= layer_count:
            continue
        unique_budgets = tuple(dict.fromkeys(budgets))
        max_budget = max(unique_budgets)
        marginal_hits = {budget: 0 for budget in unique_budgets}
        coupled_hits = {budget: 0 for budget in unique_budgets}
        accesses = 0

        # Pair scoring and ranking do not depend on the recall budget. Build each
        # ranking once, then score every requested budget from its prefix.
        for group in held_out_groups:
            for layer in range(layer_count - depth):
                target_layer = layer + depth
                actual = set(group[target_layer].experts)
                marginal_ranking = marginal_rankings[target_layer]

                scores: Counter[int] = Counter()
                for source in group[layer].experts:
                    source_scores = pairs.get((layer, depth, source))
                    if source_scores is not None:
                        scores.update(source_scores)
                coupled_ranking = sorted(
                    scores,
                    key=lambda expert: (-scores[expert], expert),
                )
                if len(coupled_ranking) < max_budget:
                    coupled_set = set(coupled_ranking)
                    for expert in marginal_ranking:
                        if expert not in coupled_set:
                            coupled_ranking.append(expert)
                            coupled_set.add(expert)
                        if len(coupled_ranking) == max_budget:
                            break

                for budget in unique_budgets:
                    marginal_hits[budget] += len(
                        actual.intersection(marginal_ranking[:budget])
                    )
                    coupled_hits[budget] += len(
                        actual.intersection(coupled_ranking[:budget])
                    )
                accesses += len(actual)

        for budget in budgets:
            recall_rows.append(
                {
                    "depth": depth,
                    "budget": budget,
                    "marginal_hits": marginal_hits[budget],
                    "coupled_hits": coupled_hits[budget],
                    "accesses": accesses,
                    "marginal_recall": marginal_hits[budget] / accesses,
                    "coupled_recall": coupled_hits[budget] / accesses,
                    "coupled_gain_pp": (coupled_hits[budget] - marginal_hits[budget])
                    / accesses
                    * 100.0,
                }
            )
    return CouplingResult(dependence_rows, recall_rows)
