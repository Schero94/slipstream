"""Decode-speed projection for a tiered (hot/warm/cold) expert-streamed model.

Turns the user's "big model on SSD, must clear >=10 tok/s" intuition into a
falsifiable number. It models one decode token as:

    time/token = compute_ms + miss_bytes/token / SSD_bandwidth

where miss_bytes/token = (1 - hit_rate) * active_expert_bytes_per_token. Hot
(GPU) + warm (RAM) residency raises the hit rate; only cold misses hit the SSD
at the measured M0c bandwidth (5.6 GB/s). The NPU does NOT appear here: moving
weights is bandwidth-bound, so the NPU's only useful role is hosting the routing
predictor that raises the effective hit_rate (modeled as a higher hit_rate, not
faster transfer).

All numbers are labeled assumptions; this is a sensitivity tool, not a claim.
Calibrate active_bytes_per_token and compute_ms before trusting an absolute tok/s.
"""

from __future__ import annotations

import json
from dataclasses import dataclass

MEASURED_SSD_GB_S = 5.6  # M0c 2026-07-20, cold random 1.5 MB reads


class StreamProjectionError(Exception):
    """Raised for invalid projection inputs."""


@dataclass(frozen=True)
class StreamModel:
    active_bytes_per_token: float  # bytes of active expert weights read per token if ALL missed
    ssd_gb_s: float = MEASURED_SSD_GB_S
    compute_ms: float = 30.0  # GPU compute floor per token (assumption; sensitivity input)

    def __post_init__(self) -> None:
        if self.active_bytes_per_token <= 0:
            raise StreamProjectionError("active_bytes_per_token must be > 0")
        if self.ssd_gb_s <= 0:
            raise StreamProjectionError("ssd_gb_s must be > 0")
        if self.compute_ms <= 0:
            raise StreamProjectionError("compute_ms must be > 0")


def _ssd_ms(model: StreamModel, hit_rate: float) -> float:
    miss_bytes = (1.0 - hit_rate) * model.active_bytes_per_token
    return miss_bytes / (model.ssd_gb_s * 1e9) * 1000.0


def decode_tok_s(model: StreamModel, *, hit_rate: float) -> float:
    if not (0.0 <= hit_rate <= 1.0):
        raise StreamProjectionError("hit_rate must be in [0, 1]")
    total_ms = model.compute_ms + _ssd_ms(model, hit_rate)
    return 1000.0 / total_ms


def required_hit_rate(model: StreamModel, *, target_tok_s: float) -> float | None:
    """Hit rate needed to clear target_tok_s, or None if impossible even at 100% hit."""
    if target_tok_s <= 0:
        raise StreamProjectionError("target_tok_s must be > 0")
    total_ms_budget = 1000.0 / target_tok_s
    ssd_ms_budget = total_ms_budget - model.compute_ms
    if ssd_ms_budget <= 0:
        # compute alone is already too slow; even a 100% hit cannot reach the target
        if decode_tok_s(model, hit_rate=1.0) >= target_tok_s:
            return 1.0
        return None
    miss_bytes_budget = ssd_ms_budget / 1000.0 * model.ssd_gb_s * 1e9
    hit = 1.0 - miss_bytes_budget / model.active_bytes_per_token
    if hit < 0.0:
        return 0.0
    if hit > 1.0:
        return 1.0
    return hit


def project_curve(model: StreamModel, *, hit_rates: list[float]) -> list[dict[str, float]]:
    rows = []
    for h in sorted(hit_rates):
        rows.append(
            {
                "hit_rate": round(h, 4),
                "ssd_ms": round(_ssd_ms(model, h), 2),
                "tok_s": round(decode_tok_s(model, hit_rate=h), 2),
            }
        )
    return rows


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    # Default reflects the flagship geometry: 48 layers x top-10 x 1.77 MB int4
    # expert = ~0.85 GB of routed experts/token (dev/35B is ~0.57 GB).
    parser.add_argument("--active-gb", type=float, default=0.85, help="active expert bytes/token (GB) if all missed")
    parser.add_argument("--ssd-gb-s", type=float, default=MEASURED_SSD_GB_S)
    parser.add_argument("--compute-ms", type=float, default=30.0, help="per-token GPU compute floor (ms)")
    parser.add_argument("--target", type=float, default=10.0, help="required decode tok/s floor")
    args = parser.parse_args(argv)
    try:
        model = StreamModel(
            active_bytes_per_token=args.active_gb * 1e9,
            ssd_gb_s=args.ssd_gb_s,
            compute_ms=args.compute_ms,
        )
        required = required_hit_rate(model, target_tok_s=args.target)
        curve = project_curve(model, hit_rates=[0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.943, 0.97, 1.0])
    except StreamProjectionError as error:
        print(json.dumps({"error": str(error)}))
        return 2
    result = {
        "assumptions": {
            "active_gb_per_token": args.active_gb,
            "compute_ms": args.compute_ms,
            "note": "active_bytes and compute_ms are estimates; calibrate before trusting absolutes",
        },
        "ssd_gb_s": args.ssd_gb_s,
        "target_tok_s": args.target,
        "required_hit_rate_for_target": (round(required, 4) if required is not None else None),
        "target_feasible": required is not None,
        "curve": curve,
        "references": {
            "colibri_best": "94.3% hit -> 3.54 tok/s (CPU path, tiny OLMoE, PR #362)",
            "f0_ceiling": "80.48% at 10 GB coupled-prefetch (our 35B traces)",
        },
    }
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
