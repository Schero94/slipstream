"""Measure the coupling predictor's real prefetch recall on routing traces.

Loads the recorded M0a routing traces, converts them to the predictor's trace
format, splits chronologically (no leakage), and reports prefetch recall for the
marginal / coupled / two-step policies across budgets. This is the honest,
data-grounded metric for the FLB prefetch brain — higher recall raises the cache
hit-rate that the streaming projection turns into decode tok/s.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from bench.m0a.analyze_routing import _token_groups, eligible_routing_paths
from bench.m0a.cache_sim import group_decode_events
from bench.m0a.constants import DEV_EXPERTS, DEV_LAYERS, DEV_TOP_K
from bench.m0a.routing_format import iter_records, read_header
from bench.m1.coupling_predictor import (
    PrefetchPredictor,
    build_marginals,
    build_pair_table,
    evaluate_recall,
    evaluate_recall_ema,
)


def load_tokens(logs_dir: Path, model_sha256: str, *, max_tokens: int | None) -> list[list[list[int]]]:
    paths = eligible_routing_paths(logs_dir, model_sha256=model_sha256)
    records = []
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
            continue
        records.extend(iter_records(path))
    groups = _token_groups(group_decode_events(records))
    tokens = []
    for group in groups:
        if len(group) != DEV_LAYERS:
            continue
        ordered = sorted(group, key=lambda event: event.layer)
        tokens.append([list(event.experts) for event in ordered])
        if max_tokens is not None and len(tokens) >= max_tokens:
            break
    return tokens


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--logs-dir", type=Path, default=Path("bench/artifacts/m0a"))
    parser.add_argument("--model-sha256", required=True)
    parser.add_argument("--max-tokens", type=int, default=8000)
    parser.add_argument("--budgets", type=int, nargs="+", default=[8, 16, 32])
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
    rows = []
    for budget in args.budgets:
        rows.append(
            {
                "budget": budget,
                "marginal": round(evaluate_recall(predictor, held, budget=budget, mode="marginal"), 4),
                "coupled": round(evaluate_recall(predictor, held, budget=budget, mode="coupled"), 4),
                "blend": round(evaluate_recall(predictor, held, budget=budget, mode="blend"), 4),
                "ema": round(evaluate_recall_ema(predictor, held, budget=budget, alpha=1.0, decay=0.9), 4),
                "two_step": round(evaluate_recall(predictor, held, budget=budget, mode="two_step"), 4),
            }
        )
    print(json.dumps({"tokens": len(tokens), "train": len(train), "held": len(held), "recall": rows}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
