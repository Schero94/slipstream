"""Coupling prefetch predictor — the FLB engine's promotion brain.

Cross-layer routing coupling (colibri #176/#204 technique): the experts routed
at layer L strongly constrain layer L+1/L+2, and that is a property of the model,
not the session. This predictor learns an offline pair table from routing traces
and, at run time, predicts the next layer's experts from the current layer's — so
the tiered cache can promote cold->warm->hot *before* the experts are demanded.

This is the CPU reference for the eventual ANE router-predictor. Its metric is
prefetch recall (clean and measurable); higher recall -> higher cache hit-rate ->
higher decode tok/s in the streaming projection. Pure and unit-tested; no engine,
no model, no disk needed.

A trace is: list of tokens; each token is a list (length = layer_count) of the
routed expert lists per layer.
"""

from __future__ import annotations

from collections import Counter
from typing import Sequence

Trace = Sequence[Sequence[Sequence[int]]]


class CouplingPredictorError(Exception):
    """Raised for invalid trace geometry or prediction requests."""


def _validate_token(token: Sequence[Sequence[int]], layer_count: int) -> None:
    if len(token) != layer_count:
        raise CouplingPredictorError(
            f"token has {len(token)} layers, expected {layer_count}"
        )


def build_pair_table(tokens: Trace, *, layer_count: int) -> dict[tuple[int, int], Counter]:
    """(layer, source_expert) -> Counter of experts routed at the next layer."""
    table: dict[tuple[int, int], Counter] = {}
    for token in tokens:
        _validate_token(token, layer_count)
        for layer in range(layer_count - 1):
            successors = token[layer + 1]
            for source in token[layer]:
                table.setdefault((layer, source), Counter()).update(successors)
    return table


def build_marginals(tokens: Trace, *, layer_count: int) -> dict[int, Counter]:
    """layer -> Counter of expert frequencies at that layer."""
    marginals: dict[int, Counter] = {layer: Counter() for layer in range(layer_count)}
    for token in tokens:
        _validate_token(token, layer_count)
        for layer in range(layer_count):
            marginals[layer].update(token[layer])
    return marginals


class PrefetchPredictor:
    def __init__(
        self,
        pair_table: dict[tuple[int, int], Counter],
        marginals: dict[int, Counter],
        *,
        layer_count: int,
        blend_beta: float = 0.3,
    ) -> None:
        self.pair_table = pair_table
        self.marginals = marginals
        self.layer_count = layer_count
        self.blend_beta = blend_beta
        self._marginal_rank = {
            layer: [expert for expert, _ in counter.most_common()]
            for layer, counter in marginals.items()
        }

    def _fill_to_budget(self, predicted: list[int], target_layer: int, budget: int) -> set[int]:
        if len(predicted) < budget:
            for expert in self._marginal_rank.get(target_layer, []):
                if expert not in predicted:
                    predicted.append(expert)
                if len(predicted) >= budget:
                    break
        return set(predicted[:budget])

    def _coupled_counter(self, src_layer: int, src_experts: Sequence[int]) -> Counter:
        counter: Counter = Counter()
        for source in src_experts:
            counter.update(self.pair_table.get((src_layer, source), {}))
        return counter

    def _coupled_step(self, src_layer: int, src_experts: Sequence[int], budget: int) -> set[int]:
        counter = self._coupled_counter(src_layer, src_experts)
        predicted = [expert for expert, _ in counter.most_common(budget)]
        return self._fill_to_budget(predicted, src_layer + 1, budget)

    def _blend_step(self, src_layer: int, src_experts: Sequence[int], budget: int, beta: float) -> set[int]:
        # Blend the coupled successor distribution with the target layer's marginal,
        # both normalized so the weight beta is scale-independent.
        coupled = self._coupled_counter(src_layer, src_experts)
        marginal = self.marginals.get(src_layer + 1, Counter())
        c_total = sum(coupled.values()) or 1
        m_total = sum(marginal.values()) or 1
        scores: dict[int, float] = {}
        for expert, count in coupled.items():
            scores[expert] = scores.get(expert, 0.0) + count / c_total
        for expert, count in marginal.items():
            scores[expert] = scores.get(expert, 0.0) + beta * (count / m_total)
        ranked = sorted(scores, key=lambda expert: (-scores[expert], expert))[:budget]
        return self._fill_to_budget(ranked, src_layer + 1, budget)

    def predict_next(
        self, *, layer: int, current_experts: Sequence[int], budget: int, mode: str = "coupled"
    ) -> set[int]:
        if budget <= 0:
            raise CouplingPredictorError("budget must be > 0")
        offset = 2 if mode == "two_step" else 1
        if layer < 0 or layer + offset >= self.layer_count:
            raise CouplingPredictorError(
                f"no target layer for mode {mode!r} at layer {layer}"
            )
        if mode == "marginal":
            ranking = self._marginal_rank.get(layer + 1, [])
            return set(ranking[:budget])
        if mode == "coupled":
            return self._coupled_step(layer, current_experts, budget)
        if mode == "blend":
            return self._blend_step(layer, current_experts, budget, self.blend_beta)
        if mode == "two_step":
            # predict L+1, then use that prediction to predict L+2
            intermediate = self._coupled_step(layer, current_experts, max(budget, 4))
            return self._coupled_step(layer + 1, sorted(intermediate), budget)
        raise CouplingPredictorError(f"unknown mode {mode!r}")


def evaluate_recall(predictor: PrefetchPredictor, tokens: Trace, *, budget: int, mode: str = "coupled") -> float:
    """Mean fraction of the target layer's actual experts that were predicted."""
    offset = 2 if mode == "two_step" else 1
    total = 0.0
    events = 0
    for token in tokens:
        _validate_token(token, predictor.layer_count)
        for layer in range(predictor.layer_count - offset):
            predicted = predictor.predict_next(
                layer=layer, current_experts=token[layer], budget=budget, mode=mode
            )
            actual = set(token[layer + offset])
            if not actual:
                continue
            total += len(predicted & actual) / len(actual)
            events += 1
    return total / events if events else 0.0


def evaluate_recall_ema(
    predictor: PrefetchPredictor,
    tokens: Trace,
    *,
    budget: int,
    alpha: float = 1.0,
    decay: float = 0.9,
) -> float:
    """Coupled prediction blended with an online EMA of recent per-layer routing.

    Routing in a decode stream is temporally correlated (same task/topic), so an
    exponential moving average of recently-used experts is a strong prior. The
    EMA is updated online after each token, so held-out evaluation stays leakage
    free (it only ever uses the past). This mirrors colibri's routing-EMA idea.
    """
    if budget <= 0:
        raise CouplingPredictorError("budget must be > 0")
    ema: dict[int, dict[int, float]] = {layer: {} for layer in range(predictor.layer_count)}
    total = 0.0
    events = 0
    for token in tokens:
        _validate_token(token, predictor.layer_count)
        for layer in range(predictor.layer_count - 1):
            target = layer + 1
            coupled = predictor._coupled_counter(layer, token[layer])
            c_total = sum(coupled.values()) or 1
            scores: dict[int, float] = {}
            for expert, count in coupled.items():
                scores[expert] = count / c_total
            for expert, score in ema[target].items():
                scores[expert] = scores.get(expert, 0.0) + alpha * score
            ranked = sorted(scores, key=lambda expert: (-scores[expert], expert))[:budget]
            predicted = predictor._fill_to_budget(list(ranked), target, budget)
            actual = set(token[target])
            if actual:
                total += len(predicted & actual) / len(actual)
                events += 1
        # online EMA update (uses only this token, then moves on)
        for layer in range(predictor.layer_count):
            decayed = {expert: value * decay for expert, value in ema[layer].items()}
            for expert in token[layer]:
                decayed[expert] = decayed.get(expert, 0.0) + (1.0 - decay)
            ema[layer] = decayed
    return total / events if events else 0.0
