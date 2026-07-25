"""M0d streaming replay — drive a real routing trace through the cache with REAL reads.

Wires the existing chain (LRU tiered cache + coupling predictor) to the real
`StreamingStore`: every cache miss (and every prefetch of a not-resident expert)
costs a real `preadv` cold read from the SSD. Reports the held-out hit-rate — the
one M0d number — plus the real measured SSD read cost and a `stream_projection`
cross-check tok/s.

The predictor is trained on the first 70% of the trace; hit-rate is counted only on
the held-out last 30% (the cache is already warm), matching the coupling-analysis
split so the number is not warm-up-biased.
"""

from __future__ import annotations

from collections import OrderedDict
from typing import Any

from bench.m1.coupling_predictor import PrefetchPredictor, build_marginals, build_pair_table
from bench.m0d.cloxcache import CloxCache


class _LruAdapter:
    """LRU with the same access()->hit interface as CloxCache (get-or-insert + evict)."""

    def __init__(self, capacity: int):
        self.cap = capacity
        self._c: OrderedDict = OrderedDict()

    def __contains__(self, key) -> bool:
        return key in self._c

    def access(self, key) -> bool:
        if key in self._c:
            self._c.move_to_end(key)
            return True
        self._c[key] = None
        if len(self._c) > self.cap:
            self._c.popitem(last=False)
        return False


def replay_streaming(
    trace,
    *,
    layer_count: int,
    store,
    capacity: int,
    prefetch_budget: int,
    compute_ms: float = 30.0,
    mode: str = "coupled",
    holdout_frac: float = 0.30,
    phases=None,
    policy: str = "lru",
    clox_k: int = 4,
) -> dict[str, Any]:
    """Replay per-token per-layer routed experts through a global LRU + real SSD store.

    `phases` (optional): a per-token label list aligned to `trace` (e.g. "think"/"answer")
    for a reasoning model. When given, the held-out hit-rate is also reported split by
    phase — the routing locality DURING thinking is measured separately from the answer,
    since thinking dominates a reasoning model's decode.
    """
    if capacity < 1:
        raise ValueError("capacity must be >= 1")
    if phases is not None and len(phases) != len(trace):
        raise ValueError("phases must align 1:1 with trace")
    n = len(trace)
    split = max(1, int(n * (1.0 - holdout_frac)))  # predictor train / count boundary

    predictor = None
    if prefetch_budget > 0:
        predictor = PrefetchPredictor(
            build_pair_table(trace[:split], layer_count=layer_count),
            build_marginals(trace[:split], layer_count=layer_count),
            layer_count=layer_count,
        )

    if policy == "clox":
        cache = CloxCache(capacity, k=clox_k)
    elif policy == "lru":
        cache = _LruAdapter(capacity)
    else:
        raise ValueError(f"unknown policy {policy!r}; use 'lru' or 'clox'")
    by_phase: dict[str, dict[str, int]] = {}

    def touch(key, *, is_access, phase=None):
        nonlocal hits, accesses
        hit = cache.access(key)  # get-or-insert + evict, uniform across policies
        if not hit:
            # miss -> real cold SSD read
            store.read_expert(key[0], key[1])
        if is_access:
            accesses += 1
            if hit:
                hits += 1
            if phase is not None:
                b = by_phase.setdefault(phase, {"accesses": 0, "hits": 0})
                b["accesses"] += 1
                if hit:
                    b["hits"] += 1

    hits = 0
    accesses = 0
    counted_tokens = 0
    ho_reads0 = ho_bytes0 = 0
    ho_seconds0 = 0.0
    for i, token in enumerate(trace):
        counting = i >= split
        cur_phase = phases[i] if (phases is not None) else None
        if counting:
            if counted_tokens == 0:
                # snapshot store counters at the split: attribute SSD cost to held-out only
                base = store.stats()
                ho_reads0, ho_bytes0, ho_seconds0 = base["reads"], base["bytes_read"], base["seconds"]
            counted_tokens += 1
        for layer in range(layer_count):
            for e in token[layer]:
                touch((layer, e), is_access=counting, phase=cur_phase if counting else None)
            if predictor is not None and layer < layer_count - 1:
                predicted = predictor.predict_next(
                    layer=layer, current_experts=token[layer], budget=prefetch_budget, mode=mode
                )
                for p in predicted:
                    touch((layer + 1, p), is_access=False)

    hit_rate = (hits / accesses) if accesses else 0.0
    s = store.stats()
    # held-out-only SSD cost (reads during the counted phase, warm cache)
    ho_reads = s["reads"] - ho_reads0
    ho_bytes = s["bytes_read"] - ho_bytes0
    ho_seconds = s["seconds"] - ho_seconds0
    ssd_ms_per_token = (ho_seconds * 1000.0 / counted_tokens) if counted_tokens else 0.0
    ho_gb_s = (ho_bytes / 1e9 / ho_seconds) if ho_seconds > 0 else 0.0
    tok_s = 1000.0 / (compute_ms + ssd_ms_per_token) if (compute_ms + ssd_ms_per_token) > 0 else 0.0
    phase_report = {
        label: {
            "accesses": b["accesses"],
            "hits": b["hits"],
            "hit_rate": round(b["hits"] / b["accesses"], 6) if b["accesses"] else 0.0,
        }
        for label, b in sorted(by_phase.items())
    }
    return {
        "held_out_tokens": counted_tokens,
        "accesses": accesses,
        "hits": hits,
        "misses": accesses - hits,
        "hit_rate": round(hit_rate, 6),
        "by_phase": phase_report,
        "ssd_reads_total": s["reads"],
        "ssd_reads_heldout": ho_reads,
        "ssd_bytes_heldout": ho_bytes,
        "ssd_seconds_heldout": round(ho_seconds, 6),
        "ssd_gb_s_heldout": round(ho_gb_s, 4),
        "ssd_ms_per_token": round(ssd_ms_per_token, 4),
        "projected_tok_s": round(tok_s, 2),
        "capacity_experts": capacity,
        "prefetch_budget": prefetch_budget,
        "policy": policy if policy == "lru" else f"clox(k={clox_k})",
    }
