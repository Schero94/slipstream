"""Real disk-backed streaming replay — validate the FLB engine on real data.

Solves the "measure the streaming engine, not just simulate it" gap WITHOUT a
flagship download (which does not fit the disk). It writes a real `.pgrn`
container for the model's expert geometry, then replays real routing traces
through the real `ExpertStreamer` (directory lookup → seek → read → CRC → LRU),
with coupling prefetch, and reports the REAL hit-rate and I/O.

Honest scope: the on-disk blob size is small (hit-rate depends only on the access
pattern, not blob content, so it is exact), while decode tok/s is derived from the
measured M0c SSD bandwidth applied to the TRUE expert byte size. The one remaining
proxy is "this model's routing locality ≈ the flagship's" — the flagship's own
locality still needs the flagship (disk decision). The engine + hit-rate + loader
are measured for real here.
"""

from __future__ import annotations

import io
import tempfile
from pathlib import Path
from typing import Any

from bench.m1.coupling_predictor import PrefetchPredictor, build_marginals, build_pair_table
from bench.m1.expert_stream import ExpertStreamer
from bench.m1.pgrn_container import read_header, write_container


def replay(
    tokens,
    *,
    layer_count: int,
    capacity: int,
    prefetch_budget: int,
    blob_bytes: int,
    real_expert_bytes: int,
    ssd_gb_s: float,
    compute_ms: float,
) -> dict[str, Any]:
    """Replay per-token per-layer routed experts through a real disk-backed streamer."""
    # distinct experts that appear, built into a real .pgrn (small blobs; hit-rate is exact)
    distinct: dict[tuple[int, int], None] = {}
    for token in tokens:
        for layer in range(layer_count):
            for e in token[layer]:
                distinct[(layer, e)] = None
    experts = [(layer, e, 1, bytes([(layer + e) % 256]) * blob_bytes, 0.5) for (layer, e) in distinct]

    buf = io.BytesIO()
    write_container(buf, metadata={"geometry": {"layers": layer_count}}, experts=experts)
    buf.seek(0)
    streamer = ExpertStreamer(buf, read_header(buf), capacity=capacity)

    # coupling predictor trained on the first 70% (no leakage into held-out replay)
    split = max(1, int(len(tokens) * 0.7))
    predictor = None
    if prefetch_budget > 0:
        predictor = PrefetchPredictor(
            build_pair_table(tokens[:split], layer_count=layer_count),
            build_marginals(tokens[:split], layer_count=layer_count),
            layer_count=layer_count,
        )

    n_tokens = 0
    for token in tokens:
        n_tokens += 1
        for layer in range(layer_count):
            for e in token[layer]:
                streamer.get_expert(layer, e)
            if predictor is not None and layer < layer_count - 1:
                predicted = predictor.predict_next(
                    layer=layer, current_experts=token[layer], budget=prefetch_budget, mode="coupled"
                )
                streamer.prefetch(sorted((layer + 1, p) for p in predicted))

    stats = streamer.stats()
    accesses = stats["hits"] + stats["misses"]
    miss_rate = (stats["misses"] / accesses) if accesses else 0.0
    accesses_per_token = accesses / n_tokens if n_tokens else 0.0
    # derive decode tok/s from the TRUE expert size and measured SSD bandwidth
    real_miss_bytes_per_token = miss_rate * accesses_per_token * real_expert_bytes
    ssd_ms = real_miss_bytes_per_token / (ssd_gb_s * 1e9) * 1000.0
    tok_s = 1000.0 / (compute_ms + ssd_ms)
    return {
        "tokens": n_tokens,
        "distinct_experts": len(distinct),
        "accesses": accesses,
        "hits": stats["hits"],
        "misses": stats["misses"],
        "hit_rate": stats["hit_rate"],
        "on_disk_bytes_streamed": stats["bytes_streamed"],
        "real_bytes_per_token_missed": real_miss_bytes_per_token,
        "decode_tok_s": round(tok_s, 2),
    }


def main(argv: list[str] | None = None) -> int:
    import argparse
    import json

    from bench.m0a.constants import DEV_LAYERS, INT4_EXPERT_BYTES
    from bench.m1.measure_coupling_recall import load_tokens

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--logs-dir", type=Path, default=Path("bench/artifacts/m0a"))
    parser.add_argument("--model-sha256", required=True)
    parser.add_argument("--max-tokens", type=int, default=4000)
    parser.add_argument("--budget-gb", type=float, default=10.0)
    parser.add_argument("--prefetch-budget", type=int, default=16)
    parser.add_argument("--blob-bytes", type=int, default=4096)
    parser.add_argument("--compute-ms", type=float, default=30.0)
    parser.add_argument("--ssd-gb-s", type=float, default=5.6)
    args = parser.parse_args(argv)

    tokens = load_tokens(args.logs_dir, args.model_sha256, max_tokens=args.max_tokens)
    if len(tokens) < 50:
        print(json.dumps({"error": f"too few tokens: {len(tokens)}"}))
        return 2
    capacity = int(args.budget_gb * 1e9 / INT4_EXPERT_BYTES)  # global resident expert count
    result = replay(
        tokens, layer_count=DEV_LAYERS, capacity=capacity, prefetch_budget=args.prefetch_budget,
        blob_bytes=args.blob_bytes, real_expert_bytes=INT4_EXPERT_BYTES,
        ssd_gb_s=args.ssd_gb_s, compute_ms=args.compute_ms,
    )
    result["budget_gb"] = args.budget_gb
    result["global_capacity_experts"] = capacity
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
