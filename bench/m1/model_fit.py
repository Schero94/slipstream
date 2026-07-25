"""Dynamic model-fit calculator (Phase 4).

Given a MoE model's geometry and a RAM tier, decide whether it fits fully resident,
must stream experts from SSD, or must be refused — and estimate the decode speed at
the resulting cache size. The memory decision reuses the exact, fail-closed
`plan_load` admission logic; the speed number is an ESTIMATE from a cost model
calibrated on real Qwen3.6-35B-A3B measurements (see CALIBRATION below).

Answers the product question: "on a 16 / 24 / 36 GiB Mac, which models fit and at
roughly what tok/s?"
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from bench.m1.memory_admission import (
    DEFAULT_MIN_HEADROOM_BYTES,
    GIB,
    LoadPlanError,
    plan_load,
)

# --- Calibration (real Qwen3.6-35B-A3B, 36 GiB M-series Mac, PGRN SSD streaming) -----
# Anchored on the measured A/B runs recorded in docs/PEREGRINE_OPTIMIZATION_PLAN.md §7.
# These are workload-dependent estimates, not guarantees.
T_COMPUTE_MS = 31.0                # per-token compute floor (~32 tok/s fully-resident
                                   # ceiling, matching the measured resident baseline)
SSD_BYTES_PER_MS = 3.25e6          # effective F_NOCACHE NVMe read (~3.25 GB/s); with the
                                   # floor above this reproduces the measured 5.5/9/13/18
                                   # tok/s at cache 2/6/10/14 GiB within a few percent
# Hit-rate H as a function of the resident expert fraction f = cache / expert_total,
# linearly interpolated between measured points (cache 2/6/10/14 GiB -> f 0.10..0.70).
HIT_CALIBRATION: list[tuple[float, float]] = [
    (0.00, 0.00),
    (0.10, 0.22),
    (0.30, 0.60),
    (0.50, 0.77),
    (0.70, 0.85),
    (1.00, 1.00),
]


def io_parallel_factor(io_threads: int) -> float:
    """Effective fetch speed-up from parallel cold reads. Measured: io=4 -> ~1.22x
    (SSD queue-depth bound, so diminishing returns). Capped conservatively."""
    if io_threads <= 1:
        return 1.0
    return min(1.0 + 0.073 * (io_threads - 1), 1.5)


def _interp(points: list[tuple[float, float]], x: float) -> float:
    x = max(0.0, min(1.0, x))
    for i in range(1, len(points)):
        x0, y0 = points[i - 1]
        x1, y1 = points[i]
        if x <= x1:
            if x1 == x0:
                return y1
            return y0 + (y1 - y0) * (x - x0) / (x1 - x0)
    return points[-1][1]


@dataclass(frozen=True)
class ModelSpec:
    name: str
    model_bytes: int          # total weight bytes (dense + experts)
    expert_total_bytes: int   # streamable routed-expert bytes
    n_layers: int             # routed (expert) layers
    n_expert: int             # experts per layer (total)
    n_expert_used: int        # top-k active experts per token
    kv_bytes: int = 0         # KV cache for the target context
    overhead_bytes: int = 2 * GIB  # runtime/compute scratch reserve

    def __post_init__(self) -> None:
        if self.model_bytes <= 0 or self.n_layers <= 0 or self.n_expert <= 0:
            raise LoadPlanError("model_bytes/n_layers/n_expert must be > 0")
        if not 0 <= self.expert_total_bytes <= self.model_bytes:
            raise LoadPlanError("expert_total_bytes must be in [0, model_bytes]")
        if not 1 <= self.n_expert_used <= self.n_expert:
            raise LoadPlanError("n_expert_used must be in [1, n_expert]")

    @property
    def experts_per_token(self) -> int:
        return self.n_layers * self.n_expert_used

    @property
    def per_expert_bytes(self) -> float:
        return self.expert_total_bytes / (self.n_layers * self.n_expert)

    @property
    def cost_per_miss_ms(self) -> float:
        return self.per_expert_bytes / SSD_BYTES_PER_MS


def predict_decode_tok_s(cache_bytes: int, spec: ModelSpec, io_threads: int = 1) -> dict[str, float]:
    """Estimate steady-state decode speed at a given resident expert-cache size."""
    if spec.expert_total_bytes == 0:
        # Dense model: no streaming misses.
        return {"hit_rate": 1.0, "misses_per_token": 0.0,
                "fetch_ms_per_token": 0.0, "decode_tok_s": 1000.0 / T_COMPUTE_MS}
    f = min(1.0, cache_bytes / spec.expert_total_bytes)
    hit = _interp(HIT_CALIBRATION, f)
    misses = spec.experts_per_token * (1.0 - hit)
    fetch_ms = misses * spec.cost_per_miss_ms / io_parallel_factor(io_threads)
    t_token = T_COMPUTE_MS + fetch_ms
    return {
        "hit_rate": hit,
        "misses_per_token": misses,
        "fetch_ms_per_token": fetch_ms,
        "decode_tok_s": 1000.0 / t_token,
    }


def headroom_for_ram(total_bytes: int) -> int:
    """RAM-scaled true-free reserve. 3 GiB is the validated default on >=24 GiB; on a
    16 GiB Mac 2 GiB keeps more room for the model while macOS stays alive."""
    if total_bytes <= 16 * GIB:
        return 2 * GIB
    return DEFAULT_MIN_HEADROOM_BYTES  # 3 GiB


def fit(spec: ModelSpec, total_bytes: int, io_threads: int = 1,
        headroom_bytes: int | None = None) -> dict[str, Any]:
    """Decide fit for one model on one RAM tier and estimate its decode speed."""
    hb = headroom_bytes if headroom_bytes is not None else headroom_for_ram(total_bytes)
    plan = plan_load(
        total_bytes=total_bytes,
        available_bytes=total_bytes,  # fresh boot assumption for the fit estimate
        model_bytes=spec.model_bytes,
        expert_total_bytes=spec.expert_total_bytes,
        kv_bytes=spec.kv_bytes,
        overhead_bytes=spec.overhead_bytes,
        min_headroom_bytes=hb,
        layers=spec.n_layers,
        expert_bytes=int(spec.per_expert_bytes) or None,
    )
    result: dict[str, Any] = {
        "model": spec.name,
        "ram_gib": round(total_bytes / GIB, 1),
        "headroom_gib": round(hb / GIB, 1),
        "mode": plan["mode"],
        "reason": plan["reason"],
    }
    if plan["mode"] == "refuse":
        result.update({"cache_gib": 0.0, "predicted_decode_tok_s": 0.0, "hit_rate": 0.0})
        return result
    cache = spec.expert_total_bytes if plan["mode"] == "resident" else plan["resident_experts_bytes"]
    speed = predict_decode_tok_s(cache, spec, io_threads)
    result.update({
        "cache_gib": round(cache / GIB, 2),
        "resident_gib": round(plan["resident_bytes"] / GIB, 2),
        "free_after_gib": round((total_bytes - plan["resident_bytes"]) / GIB, 2),
        "hit_rate": round(speed["hit_rate"], 3),
        "predicted_decode_tok_s": round(speed["decode_tok_s"], 1),
        "io_threads": io_threads,
    })
    return result


def fit_matrix(specs: list[ModelSpec], ram_tiers_bytes: list[int],
               io_threads: int = 1) -> list[dict[str, Any]]:
    return [fit(spec, ram, io_threads) for spec in specs for ram in ram_tiers_bytes]


# --- Real geometry from a .pgrn sidecar (+ sibling .gguf for total weight bytes) -----

def spec_from_pgrn(pgrn_path: Path, n_expert_used: int, gguf_bytes: int | None = None,
                   kv_bytes: int = 0) -> ModelSpec:
    """Read routed-expert geometry from a PGRN header. model_bytes comes from the
    sibling GGUF size when present, else falls back to the expert bytes alone."""
    with open(pgrn_path, "rb") as fh:
        magic = fh.read(8)
        if magic[:5] != b"PGRN1":
            raise LoadPlanError(f"{pgrn_path} is not a PGRN v1 file")
        (_version,) = struct.unpack("<I", fh.read(4))
        (json_len,) = struct.unpack("<I", fh.read(4))
        meta = json.loads(fh.read(json_len).decode("utf-8"))
    geom = meta["metadata"]["geometry"]
    n_layers = int(geom["layers_with_experts"])
    n_expert = int(geom["experts_per_layer"])
    expert_total = int(meta.get("total_expert_bytes") or 0)
    if expert_total == 0:
        expert_total = pgrn_path.stat().st_size  # conservative fallback
    gguf = gguf_bytes
    if gguf is None:
        sib = pgrn_path.with_suffix(".gguf")
        gguf = sib.stat().st_size if sib.exists() else expert_total
    model_bytes = max(gguf, expert_total)
    return ModelSpec(
        name=pgrn_path.stem,
        model_bytes=model_bytes,
        expert_total_bytes=min(expert_total, model_bytes),
        n_layers=n_layers,
        n_expert=n_expert,
        n_expert_used=n_expert_used,
        kv_bytes=kv_bytes,
    )


# The measured reference model (real geometry + sizes from the qualification artifacts).
REFERENCE_35B = ModelSpec(
    name="Qwen3.6-35B-A3B-Q4",
    model_bytes=22_853_663_008,
    expert_total_bytes=20_128_754_176,
    n_layers=41,
    n_expert=256,
    n_expert_used=8,
    kv_bytes=0,
    overhead_bytes=2 * GIB,
)


def _print_matrix(rows: list[dict[str, Any]]) -> None:
    hdr = f"{'model':26} {'RAM':>5} {'mode':10} {'cache':>7} {'free':>6} {'hit':>5} {'tok/s':>6}"
    print(hdr)
    print("-" * len(hdr))
    for r in rows:
        ram = f"{r['ram_gib']:.0f}G"
        if r["mode"] == "refuse":
            print(f"{r['model']:26} {ram:>5} {'REFUSE':10} {'-':>7} {'-':>6} {'-':>5} {'-':>6}")
        else:
            cache = f"{r['cache_gib']:.1f}G"
            free = f"{r['free_after_gib']:.1f}G"
            print(f"{r['model']:26} {ram:>5} {r['mode']:10} {cache:>7} {free:>6} "
                  f"{r['hit_rate']*100:>4.0f}% {r['predicted_decode_tok_s']:>6.1f}")


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description="Dynamic MoE model-fit + speed estimator")
    p.add_argument("--ram", default="16,24,36",
                   help="comma-separated RAM tiers in GiB (default 16,24,36)")
    p.add_argument("--io-threads", type=int, default=1)
    p.add_argument("--pgrn", type=Path, action="append", default=[],
                   help="a .pgrn to read real geometry from (repeatable)")
    p.add_argument("--n-expert-used", type=int, default=8,
                   help="top-k active experts per token for scanned PGRN models")
    p.add_argument("--json", action="store_true", help="emit JSON instead of a table")
    args = p.parse_args(argv)

    tiers = [round(float(x) * GIB) for x in args.ram.split(",") if x.strip()]
    if args.pgrn:
        specs = [spec_from_pgrn(path, args.n_expert_used) for path in args.pgrn]
    else:
        specs = [REFERENCE_35B]
    rows = fit_matrix(specs, tiers, args.io_threads)
    if args.json:
        print(json.dumps(rows, indent=2))
    else:
        _print_matrix(rows)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
