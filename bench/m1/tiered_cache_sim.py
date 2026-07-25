"""Tiered hot/warm/cold expert cache simulator — end-to-end hit-rate layer.

Consumes the coupling predictor's promotions and reports the *actual* resident
hit-rate (not just prediction recall), broken down by tier:

* hot  = GPU-resident (free on access),
* warm = RAM-resident (cheap),
* cold = SSD miss (streamed at the measured 5.6 GB/s).

Per layer, each cache is a two-tier LFRU: a demanded expert in warm is promoted
to hot; hot overflow demotes its LRU to warm; warm overflow evicts its LRU to
cold. Prefetched (predicted) experts land in warm ahead of demand. This is the
algorithm the Metal runtime will implement; here it is pure and unit-tested so we
know the policy before writing any kernel.
"""

from __future__ import annotations

from collections import OrderedDict
from pathlib import Path
from typing import Sequence

from bench.m1.coupling_predictor import PrefetchPredictor

Trace = Sequence[Sequence[Sequence[int]]]


class TieredCacheError(Exception):
    """Raised for invalid cache configuration."""


class TieredCache:
    def __init__(self, *, hot_capacity: int, warm_capacity: int) -> None:
        if hot_capacity < 1 or warm_capacity < 0:
            raise TieredCacheError("hot_capacity must be >= 1 and warm_capacity >= 0")
        self.hot_capacity = hot_capacity
        self.warm_capacity = warm_capacity
        self.hot: OrderedDict[int, None] = OrderedDict()
        self.warm: OrderedDict[int, None] = OrderedDict()

    def _evict_hot(self) -> None:
        while len(self.hot) > self.hot_capacity:
            expert, _ = self.hot.popitem(last=False)  # LRU
            self.warm[expert] = None
            self.warm.move_to_end(expert)
            self._evict_warm()

    def _evict_warm(self) -> None:
        while len(self.warm) > self.warm_capacity:
            self.warm.popitem(last=False)  # evict to cold (SSD)

    def _to_hot(self, expert: int) -> None:
        self.hot[expert] = None
        self.hot.move_to_end(expert)
        self._evict_hot()

    def access(self, expert: int) -> str:
        if expert in self.hot:
            self.hot.move_to_end(expert)
            return "hot"
        if expert in self.warm:
            del self.warm[expert]
            self._to_hot(expert)
            return "warm"
        self._to_hot(expert)
        return "miss"

    def prefetch(self, experts: Sequence[int]) -> None:
        for expert in experts:
            if expert in self.hot or expert in self.warm:
                continue
            self.warm[expert] = None
            self.warm.move_to_end(expert)
            self._evict_warm()


def simulate_tiered(
    tokens: Trace,
    predictor: PrefetchPredictor,
    *,
    layer_count: int,
    hot_capacity: int,
    warm_capacity: int,
    prefetch_budget: int,
) -> dict[str, float]:
    caches = [TieredCache(hot_capacity=hot_capacity, warm_capacity=warm_capacity) for _ in range(layer_count)]
    hot = warm = miss = 0
    for token in tokens:
        if len(token) != layer_count:
            raise TieredCacheError(f"token has {len(token)} layers, expected {layer_count}")
        for layer in range(layer_count):
            for expert in token[layer]:
                tier = caches[layer].access(expert)
                if tier == "hot":
                    hot += 1
                elif tier == "warm":
                    warm += 1
                else:
                    miss += 1
            if prefetch_budget > 0 and layer < layer_count - 1:
                predicted = predictor.predict_next(
                    layer=layer, current_experts=token[layer], budget=prefetch_budget, mode="coupled"
                )
                caches[layer + 1].prefetch(sorted(predicted))
    accesses = hot + warm + miss
    if accesses == 0:
        raise TieredCacheError("no expert accesses in trace")
    return {
        "accesses": accesses,
        "hot": hot,
        "warm": warm,
        "miss": miss,
        "hit_rate": (hot + warm) / accesses,
        "hot_rate": hot / accesses,
        "miss_rate": miss / accesses,
    }


def main(argv: list[str] | None = None) -> int:
    import argparse
    import json

    from bench.m0a.constants import DEV_LAYERS, INT4_EXPERT_BYTES
    from bench.m1.coupling_predictor import build_marginals, build_pair_table
    from bench.m1.measure_coupling_recall import load_tokens
    from bench.m1.stream_projection import MEASURED_SSD_GB_S, StreamModel, decode_tok_s

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--logs-dir", type=Path, default=Path("bench/artifacts/m0a"))
    parser.add_argument("--model-sha256", required=True)
    parser.add_argument("--max-tokens", type=int, default=8000)
    parser.add_argument("--budgets-gb", type=float, nargs="+", default=[8.0, 9.0, 10.0])
    parser.add_argument("--prefetch-budget", type=int, default=16)
    parser.add_argument("--compute-ms", type=float, default=30.0)
    args = parser.parse_args(argv)

    tokens = load_tokens(args.logs_dir, args.model_sha256, max_tokens=args.max_tokens)
    if len(tokens) < 100:
        print(json.dumps({"error": f"too few tokens: {len(tokens)}"}))
        return 2
    split = int(len(tokens) * 0.7)
    train, held = tokens[:split], tokens[split:]
    predictor = PrefetchPredictor(
        build_pair_table(train, layer_count=DEV_LAYERS),
        build_marginals(train, layer_count=DEV_LAYERS),
        layer_count=DEV_LAYERS,
    )
    accesses_per_token = sum(len(layer) for layer in held[0])
    rows = []
    for budget_gb in args.budgets_gb:
        capacity = int(budget_gb * 1e9 / DEV_LAYERS / INT4_EXPERT_BYTES)
        hot_cap = max(1, capacity // 3)
        warm_cap = max(0, capacity - hot_cap)
        row = {"budget_gb": budget_gb, "capacity_per_layer": capacity}
        for label, pf in (("no_prefetch", 0), ("coupled_prefetch", args.prefetch_budget)):
            sim = simulate_tiered(
                held, predictor, layer_count=DEV_LAYERS,
                hot_capacity=hot_cap, warm_capacity=warm_cap, prefetch_budget=pf,
            )
            miss_bytes_per_token = sim["miss_rate"] * accesses_per_token * INT4_EXPERT_BYTES
            model = StreamModel(
                active_bytes_per_token=accesses_per_token * INT4_EXPERT_BYTES,
                ssd_gb_s=MEASURED_SSD_GB_S,
                compute_ms=args.compute_ms,
            )
            row[label] = {
                "hit_rate": round(sim["hit_rate"], 4),
                "hot_rate": round(sim["hot_rate"], 4),
                "miss_rate": round(sim["miss_rate"], 4),
                "tok_s": round(decode_tok_s(model, hit_rate=sim["hit_rate"]), 2),
            }
        rows.append(row)
    print(json.dumps({"held_tokens": len(held), "accesses_per_token": accesses_per_token, "rows": rows}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
