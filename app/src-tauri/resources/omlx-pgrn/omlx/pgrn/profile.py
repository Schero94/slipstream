"""PGRN streaming profiles (quality / balanced / contract / fast).

**balanced** is the product default: prefill-sized HOT/WARM (4096/2048) +
io_width=16 on 24–36 GiB Macs (~6.8 GiB high-water). Prefer io=16 (no
cold-io=32 boost) for stable warm — see PERF_RECOVERY.md. `quality` matches
that cache; internal `contract` bounds tool/schema prompts below the 36-GiB
Mac Metal cap; `fast` fits the tightest headroom.
"""
from __future__ import annotations

import os
from dataclasses import dataclass


@dataclass(frozen=True)
class PgrnProfile:
    name: str
    capacity: int
    hot_capacity: int
    io_width: int
    clox_k: int = 2


# ~1.69 MiB / expert on Qwen3.6-35B-A3B-4bit.
# Decode touches up to ~top_k×n_layers unique experts/token (~320 on 35B).
# Capacity must cover several tokens of working set or warm≈cold (~1 tok/s).
_PROFILES = {
    # Prefill on 35B can touch 100+ unique experts/layer — size for that.
    "quality": PgrnProfile("quality", capacity=4096, hot_capacity=2048, io_width=16),
    # Prefill working-set (2026-07-29 E2E, internal NVMe, 35B A3B-4bit):
    #   cap=2048/io=8  → warm ≈1.1–1.4 tok/s (thrash)
    #   cap=4096/io=16 alone → warm 5–11 tok/s (RSS variance)
    #   + mlock/keep-hot → warm p50≈14.2 / p95≈14.9 (M3; RSS ~16.5 GiB)
    "balanced": PgrnProfile("balanced", capacity=4096, hot_capacity=2048, io_width=16),
    # Internal product profile for tools / structured output. Their chat
    # templates can expand a short request to ~280 tokens; 4096 slots reached
    # the 28.1-GiB Metal hard watermark on a 36-GiB M3 Pro. Keep io16 but bound
    # the MX working set to ~3.4 GiB instead of the balanced ~6.8 GiB.
    "contract": PgrnProfile("contract", capacity=2048, hot_capacity=1024, io_width=16),
    # Tight headroom — decode-ok, prefill will thrash more.
    "fast": PgrnProfile("fast", capacity=512, hot_capacity=256, io_width=4),
}


def resolve_profile(name: str | None = None) -> PgrnProfile:
    raw = (name or os.environ.get("SLIPSTREAM_PGRN_PROFILE", "balanced")).strip().lower()
    if raw not in _PROFILES:
        raw = "balanced"
    base = _PROFILES[raw]

    # Optional fine overrides (bench / power users).
    cap = os.environ.get("SLIPSTREAM_PGRN_CAPACITY", "").strip()
    hot = os.environ.get("SLIPSTREAM_PGRN_HOT_CAPACITY", "").strip()
    iow = os.environ.get("SLIPSTREAM_PGRN_IO_WIDTH", "").strip()
    capacity = int(cap) if cap else base.capacity
    hot_capacity = int(hot) if hot else base.hot_capacity
    io_width = int(iow) if iow else base.io_width
    if hot_capacity < 0:
        hot_capacity = 0
    if hot_capacity > capacity:
        hot_capacity = capacity
    if io_width < 1:
        io_width = 1
    if capacity < 8:
        capacity = 8
    return PgrnProfile(
        name=raw,
        capacity=capacity,
        hot_capacity=hot_capacity,
        io_width=io_width,
        clox_k=base.clox_k,
    )
