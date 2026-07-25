"""Export a native PGCT1 hot-set table from routing traces.

Turns measured per-layer expert frequencies (build_marginals over recorded M0a routing
traces) into the compact binary table that peregrine_predict.c loads. The runtime warms
each layer's hot set (pgr_runtime_prefetch) during the previous layer's compute — a
misprediction only wastes a speculative read, never changes logits.

PGCT1 format (little-endian), matching vendor/llama.cpp/src/peregrine_predict.h:
    magic "PGCT1\\0\\0\\0" | u32 version=1 | u32 layer_count
    per layer (ascending): u16 layer_id | u16 hot_count | hot_count*u16 expert_id (ranked)
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
from collections import Counter
from pathlib import Path
from typing import Mapping

from bench.m1.coupling_predictor import build_marginals, build_pair_table

U16_MAX = 0xFFFF


def build_pgct1(marginals: Mapping[int, Counter], top_n: int) -> bytes:
    """Serialize per-layer expert-frequency Counters into a PGCT1 image.

    Each layer keeps its `top_n` most-frequent experts, ranked hottest first. Layers are
    written in ascending id order (the loader requires strictly ascending, unique layers).
    """
    if top_n < 1:
        raise ValueError("top_n must be >= 1")
    layers = sorted(marginals.keys())
    out = bytearray(b"PGCT1\x00\x00\x00")
    out += struct.pack("<I", 1)
    out += struct.pack("<I", len(layers))
    for layer in layers:
        if not (0 <= layer <= U16_MAX):
            raise ValueError(f"layer id {layer} out of u16 range")
        hot = [e for e, _ in marginals[layer].most_common(top_n)]
        if any(not (0 <= e <= U16_MAX) for e in hot):
            raise ValueError("expert id out of u16 range")
        out += struct.pack("<HH", layer, len(hot))
        for e in hot:
            out += struct.pack("<H", e)
    return bytes(out)


def build_pgcc1(pair_table: Mapping[tuple[int, int], Counter], top_m: int) -> bytes:
    """Serialize a coupled pair table into a PGCC1 image.

    `pair_table` maps (source_layer, source_expert) -> Counter of experts routed at the
    next layer (build_pair_table). Each source expert keeps its `top_m` heaviest
    successors, ranked. Layers and, within a layer, source experts are written strictly
    ascending (the loader requires it). Counts are clamped to u16 - they only rank
    candidates for a speculative warm, so saturation never changes logits.
    """
    if top_m < 1:
        raise ValueError("top_m must be >= 1")
    by_layer: dict[int, dict[int, Counter]] = {}
    for (layer, expert), succ in pair_table.items():
        if not (0 <= layer <= U16_MAX):
            raise ValueError(f"layer id {layer} out of u16 range")
        if not (0 <= expert <= U16_MAX):
            raise ValueError(f"expert id {expert} out of u16 range")
        if not succ:
            continue
        by_layer.setdefault(layer, {})[expert] = succ

    layers = sorted(by_layer.keys())
    out = bytearray(b"PGCC1\x00\x00\x00")
    out += struct.pack("<I", 1)
    out += struct.pack("<I", len(layers))
    for layer in layers:
        experts = sorted(by_layer[layer].keys())
        out += struct.pack("<HH", layer, len(experts))
        for expert in experts:
            succ = by_layer[layer][expert].most_common(top_m)
            if any(not (0 <= sid <= U16_MAX) for sid, _ in succ):
                raise ValueError("successor expert id out of u16 range")
            out += struct.pack("<HH", expert, len(succ))
            for sid, cnt in succ:
                out += struct.pack("<HH", sid, min(cnt, U16_MAX))
    return bytes(out)


def _load_trace_json(path: Path) -> list[list[list[int]]]:
    """A trace JSON is a list of tokens; each token is a list (len = layer_count) of the
    routed expert-id lists per layer."""
    data = json.loads(path.read_text())
    if not isinstance(data, list):
        raise ValueError("trace JSON must be a list of tokens")
    return data


def export(*, tokens: list[list[list[int]]], layer_count: int, top_n: int, out_path: Path) -> dict:
    marginals = build_marginals(tokens, layer_count=layer_count)
    image = build_pgct1(marginals, top_n)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_bytes(image)
    return {
        "out": str(out_path),
        "bytes": len(image),
        "layers": layer_count,
        "top_n": top_n,
        "tokens": len(tokens),
        "distinct_per_layer": {l: len(c) for l, c in sorted(marginals.items())},
    }


def export_coupled(*, tokens: list[list[list[int]]], layer_count: int, top_m: int, out_path: Path) -> dict:
    pair_table = build_pair_table(tokens, layer_count=layer_count)
    image = build_pgcc1(pair_table, top_m)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_bytes(image)
    return {
        "out": str(out_path),
        "bytes": len(image),
        "format": "PGCC1",
        "layers": layer_count,
        "top_m": top_m,
        "tokens": len(tokens),
        "source_pairs": len(pair_table),
    }


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Export a PGCT1 hot-set or PGCC1 coupled table from routing traces")
    ap.add_argument("--trace", type=Path, help="trace JSON (list of tokens) to build from")
    ap.add_argument("--logs-dir", type=Path, help="M0a routing logs dir (uses the recorded traces)")
    ap.add_argument("--model-sha", help="model sha256 (required with --logs-dir)")
    ap.add_argument("--max-tokens", type=int, default=None)
    ap.add_argument("--layer-count", type=int, required=True)
    ap.add_argument("--coupled", action="store_true",
                    help="export a PGCC1 coupled table (conditions on the source layer's fired experts)")
    ap.add_argument("--top-n", type=int, default=64, help="PGCT1: hot experts kept per layer")
    ap.add_argument("--coupled-top-m", type=int, default=32, help="PGCC1: successors kept per source expert")
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args(argv)

    if args.trace:
        tokens = _load_trace_json(args.trace)
    elif args.logs_dir and args.model_sha:
        from bench.m1.measure_coupling_recall import load_tokens  # reuse the recorded-trace loader
        tokens = load_tokens(args.logs_dir, args.model_sha, max_tokens=args.max_tokens)
    else:
        ap.error("provide --trace, or --logs-dir with --model-sha")

    if args.coupled:
        stats = export_coupled(tokens=tokens, layer_count=args.layer_count,
                               top_m=args.coupled_top_m, out_path=args.out)
    else:
        stats = export(tokens=tokens, layer_count=args.layer_count, top_n=args.top_n, out_path=args.out)
    print(json.dumps({k: v for k, v in stats.items() if k != "distinct_per_layer"}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
