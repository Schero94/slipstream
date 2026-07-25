"""Dependency-free cache-policy simulation for M0a routing traces."""

from __future__ import annotations

from collections import Counter, OrderedDict
from dataclasses import dataclass
from typing import Iterable, Mapping, Sequence
from uuid import UUID

from bench.m0a.constants import (
    DEV_EXPERTS,
    DEV_LAYERS,
    FLAGSHIP_EXPERTS,
    FLAGSHIP_LAYERS,
)
from bench.m0a.routing_format import PHASE_DECODE, RoutingRecord


class TraceValidationError(ValueError):
    """Raised when records do not form complete chronological decode tokens."""


@dataclass(frozen=True)
class AccessEvent:
    session_id: UUID
    batch_id: int
    token_pos: int
    token_id: int
    layer: int
    experts: tuple[int, ...]


@dataclass(frozen=True)
class PolicyMetrics:
    accesses: int
    hits: int
    layer_token_events: int
    all_hit_events: int

    @property
    def hit_rate(self) -> float:
        return self.hits / self.accesses if self.accesses else 0.0

    @property
    def all_hit_rate(self) -> float:
        return self.all_hit_events / self.layer_token_events if self.layer_token_events else 0.0


def group_decode_events(records: Iterable[RoutingRecord]) -> list[AccessEvent]:
    groups: OrderedDict[
        tuple[UUID, int, int],
        dict[int, RoutingRecord],
    ] = OrderedDict()

    for record in records:
        if record.phase != PHASE_DECODE:
            continue
        key = (record.session_id, record.batch_id, record.token_pos)
        layers = groups.setdefault(key, {})
        if record.layer in layers:
            raise TraceValidationError(
                f"duplicate layer {record.layer} for decode token {key}"
            )
        layers[record.layer] = record

    events: list[AccessEvent] = []
    expected_layers = set(range(DEV_LAYERS))
    for key, layers in groups.items():
        if set(layers) != expected_layers:
            missing = sorted(expected_layers - set(layers))
            extra = sorted(set(layers) - expected_layers)
            raise TraceValidationError(
                f"incomplete decode token {key}: missing={missing} extra={extra}"
            )
        token_ids = {record.token_id for record in layers.values()}
        sequence_ids = {record.sequence_id for record in layers.values()}
        if len(token_ids) != 1 or len(sequence_ids) != 1:
            raise TraceValidationError(
                f"inconsistent token or sequence metadata for decode token {key}"
            )
        for layer in range(DEV_LAYERS):
            record = layers[layer]
            events.append(
                AccessEvent(
                    session_id=record.session_id,
                    batch_id=record.batch_id,
                    token_pos=record.token_pos,
                    token_id=record.token_id,
                    layer=record.layer,
                    experts=record.experts,
                )
            )
    return events


def projected_dev_capacity(budget_bytes: int, record_bytes: int) -> int:
    if budget_bytes < 0 or record_bytes <= 0:
        raise ValueError("budget_bytes must be non-negative and record_bytes positive")
    flagship_capacity = (budget_bytes // FLAGSHIP_LAYERS) // record_bytes
    resident_fraction = min(1.0, flagship_capacity / FLAGSHIP_EXPERTS)
    return min(DEV_EXPERTS, int(resident_fraction * DEV_EXPERTS))


def build_static_pins(
    events: Sequence[AccessEvent],
    capacity_per_layer: int,
) -> dict[int, frozenset[int]]:
    if capacity_per_layer < 0:
        raise ValueError("capacity_per_layer must be non-negative")
    counts: dict[int, Counter[int]] = {}
    for event in events:
        counts.setdefault(event.layer, Counter()).update(event.experts)
    return {
        layer: frozenset(
            expert
            for expert, _ in sorted(
                layer_counts.items(), key=lambda item: (-item[1], item[0])
            )[:capacity_per_layer]
        )
        for layer, layer_counts in sorted(counts.items())
    }


def build_global_static_pins(
    events: Sequence[AccessEvent],
    total_capacity: int,
) -> dict[int, frozenset[int]]:
    if total_capacity < 0:
        raise ValueError("total_capacity must be non-negative")
    counts: Counter[tuple[int, int]] = Counter()
    for event in events:
        counts.update((event.layer, expert) for expert in event.experts)
    selected = sorted(
        counts.items(),
        key=lambda item: (-item[1], item[0][0], item[0][1]),
    )[:total_capacity]
    mutable: dict[int, set[int]] = {}
    for (layer, expert), _ in selected:
        mutable.setdefault(layer, set()).add(expert)
    return {layer: frozenset(experts) for layer, experts in sorted(mutable.items())}


def _metrics(hit_vectors: Iterable[tuple[bool, ...]]) -> PolicyMetrics:
    accesses = hits = layer_events = all_hit_events = 0
    for vector in hit_vectors:
        accesses += len(vector)
        hits += sum(vector)
        layer_events += 1
        all_hit_events += int(all(vector))
    return PolicyMetrics(accesses, hits, layer_events, all_hit_events)


def simulate_static(
    events: Sequence[AccessEvent],
    pins: Mapping[int, frozenset[int]],
) -> PolicyMetrics:
    return _metrics(
        tuple(expert in pins.get(event.layer, frozenset()) for expert in event.experts)
        for event in events
    )


def _copy_initial(
    initial: Mapping[int, Iterable[int]] | None,
    capacity_per_layer: int,
) -> dict[int, OrderedDict[int, None]]:
    state: dict[int, OrderedDict[int, None]] = {}
    if initial is None:
        return state
    for layer, experts in initial.items():
        cache = OrderedDict((expert, None) for expert in experts)
        while len(cache) > capacity_per_layer:
            cache.popitem(last=False)
        state[layer] = cache
    return state


def _update_lru(
    cache: OrderedDict[int, None],
    experts: Iterable[int],
    capacity: int,
) -> None:
    for expert in experts:
        if expert in cache:
            cache.move_to_end(expert)
        elif capacity > 0:
            cache[expert] = None
            if len(cache) > capacity:
                cache.popitem(last=False)


def simulate_lru(
    events: Sequence[AccessEvent],
    capacity_per_layer: int,
    initial: Mapping[int, Iterable[int]] | None = None,
    *,
    reset_on_session: bool = True,
) -> PolicyMetrics:
    if capacity_per_layer < 0:
        raise ValueError("capacity_per_layer must be non-negative")
    state = _copy_initial(initial, capacity_per_layer)
    hit_vectors: list[tuple[bool, ...]] = []
    current_session: UUID | None = None
    for event in events:
        if current_session is None:
            current_session = event.session_id
        elif event.session_id != current_session:
            current_session = event.session_id
            if reset_on_session:
                state = _copy_initial(initial, capacity_per_layer)
        cache = state.setdefault(event.layer, OrderedDict())
        hit_vectors.append(tuple(expert in cache for expert in event.experts))
        _update_lru(cache, event.experts, capacity_per_layer)
    return _metrics(hit_vectors)


def simulate_pin_lru(
    events: Sequence[AccessEvent],
    pins: Mapping[int, frozenset[int]],
    lru_capacity_per_layer: int,
    initial: Mapping[int, Iterable[int]] | None = None,
    *,
    reset_on_session: bool = True,
) -> PolicyMetrics:
    if lru_capacity_per_layer < 0:
        raise ValueError("lru_capacity_per_layer must be non-negative")
    state = _copy_initial(initial, lru_capacity_per_layer)
    hit_vectors: list[tuple[bool, ...]] = []
    current_session: UUID | None = None
    for event in events:
        if current_session is None:
            current_session = event.session_id
        elif event.session_id != current_session:
            current_session = event.session_id
            if reset_on_session:
                state = _copy_initial(initial, lru_capacity_per_layer)
        layer_pins = pins.get(event.layer, frozenset())
        cache = state.setdefault(event.layer, OrderedDict())
        hit_vectors.append(
            tuple(expert in layer_pins or expert in cache for expert in event.experts)
        )
        _update_lru(
            cache,
            (expert for expert in event.experts if expert not in layer_pins),
            lru_capacity_per_layer,
        )
    return _metrics(hit_vectors)
