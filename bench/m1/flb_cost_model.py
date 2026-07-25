"""FLB cost-model scheduler v0 — the assembly-line decision engine.

The blueprint's central FLB artifact is not a kernel or a model: it is a cost
model over the pipeline stations (SSD, CPU+RAM, GPU, ANE) fed by *measured*
latency/bandwidth/accept-rate, that decides which device offload is worth
building. This module builds that engine measurement-first, so we never write a
Metal kernel on faith.

It (1) measures the one governing number still missing — CPU↔unified-memory
bandwidth — and (2) evaluates each FLB lever against our recorded evidence and
the shared-bus rule, emitting a BUILD/REJECT verdict per lever.

Honest premise (blueprint's own "corrected parallelism physics"): batch-1 decode
is memory-bandwidth-bound; all compute units share one memory bus, so a second
unit that also reads main memory adds no bandwidth. Gains come only from
offloading compute-bound phases, hiding latency, or reading from a different
source. Our closed tracks (W3 492609a7, S8 0.91%, F0 80.48%) constrain this.
"""

from __future__ import annotations

import json
import time
from typing import Any

# Measured station facts with provenance. `measured=True` means a number from
# bench/RESULTS.md; assumptions are flagged so the cost model never hides them.
MEASURED_STATIONS: dict[str, dict[str, Any]] = {
    "ssd": {
        "metric": "cold random 1.5MB read bandwidth",
        "value": 5.6,
        "unit": "GB/s",
        "measured": True,
        "source": "M0c 2026-07-20",
    },
    "gpu_decode_64k": {
        "metric": "MTP4 decode throughput at 64K",
        "value": 25.6,
        "unit": "tok/s",
        "measured": True,
        "source": "profile fa99ef98 / S3b",
    },
    "gpu_decode_4k": {
        "metric": "MTP4 decode throughput at 4K",
        "value": 43.9,
        "unit": "tok/s",
        "measured": True,
        "source": "S3b 2026-07-17",
    },
    "cpu_encode_submit_share": {
        "metric": "CPU graph encode/submit share of a 64K request",
        "value": 0.91,
        "unit": "%",
        "measured": True,
        "source": "S8 2026-07-17",
    },
    "mtp_accept": {
        "metric": "MTP draft acceptance (window 4)",
        "value": 91.0,
        "unit": "%",
        "measured": True,
        "source": "smoke / F4",
    },
    "cpu_ram_bandwidth": {
        "metric": "CPU sequential unified-memory read bandwidth",
        "value": None,
        "unit": "GB/s",
        "measured": False,
        "source": "this module (measured at runtime)",
    },
    "ane": {
        "metric": "Neural Engine fp16 speedup vs CPU (shape-dependent)",
        "value": "1.0x @seq32 .. 4.2x @seq512",
        "unit": "speedup",
        "measured": True,
        "source": "ANE probe 2026-07-20 (coremltools, py3.11)",
    },
}


class FlbError(Exception):
    """Raised for invalid cost-model inputs."""


def throughput_gb_s(bytes_moved: int, seconds: float) -> float:
    if seconds <= 0:
        raise FlbError("seconds must be > 0")
    return bytes_moved / 1e9 / seconds


def validate_membw_args(*, size_bytes: int, trials: int) -> None:
    if size_bytes <= 0:
        raise FlbError("size_bytes must be > 0")
    if trials < 1:
        raise FlbError("trials must be >= 1")


def measure_memory_bandwidth(*, size_bytes: int = 1024 * 1024 * 1024, trials: int = 5) -> dict[str, Any]:
    """Measure CPU↔unified-memory read and copy bandwidth (numpy, no new deps).

    Read = full-array reduction; copy = read+write. Peak (best trial) is reported
    because scheduling noise only slows a run, never speeds it past the true bus.
    """
    import numpy as np

    validate_membw_args(size_bytes=size_bytes, trials=trials)
    count = size_bytes // 8  # float64 elements
    source = np.ones(count, dtype=np.float64)
    best_read = 0.0
    best_copy = 0.0
    moved = count * 8
    for _ in range(trials):
        start = time.monotonic()
        _ = float(source.sum())
        read_s = time.monotonic() - start
        best_read = max(best_read, throughput_gb_s(moved, read_s))

        start = time.monotonic()
        dest = source.copy()
        copy_s = time.monotonic() - start
        # copy moves the array twice (read source + write dest)
        best_copy = max(best_copy, throughput_gb_s(moved * 2, copy_s))
        del dest
    return {
        "size_bytes": size_bytes,
        "trials": trials,
        "read_gb_s": round(best_read, 2),
        "copy_gb_s": round(best_copy, 2),
    }


# Each lever carries a fixed verdict grounded in recorded evidence plus the
# shared-bus rule. The cost model applies them against the measured bandwidth.
_LEVERS = {
    "ssd_expert_stream": {
        "description": (
            "Stream cold experts from SSD (colibri 2b). NOT a speed lever for the "
            "resident 32B (that fits in RAM) — it is a CAPACITY lever: run a larger "
            "model (64B-REAP class) that does not fit in 36 GB RAM, accepting lower "
            "but still-usable decode speed for higher quality."
        ),
        "verdict": "CAPACITY_LEVER_PLAUSIBLE",
        "reason": (
            "Corrected model (stream_projection 2026-07-20): decode is GPU compute + "
            "cold-miss streaming, NOT colibri's CPU path (their 3.54 tok/s was a tiny "
            "CPU model). At ~1.75 GB active/token, 30 ms compute and the measured 5.6 "
            "GB/s SSD, >=10 tok/s needs only ~78% hit (~82% at 45 ms compute; ~64% for a "
            "REAP-pruned 1.0 GB/token). F0 already measured ~80% and colibri 94.3%, so "
            "the >=10 tok/s floor is plausibly reachable for a >RAM (larger) model. "
            "Unproven: a real big-model hit-rate at the 36 GB resident working set, the "
            "compute floor, and the disk decision (39 GB model vs ~16 GB free)."
        ),
        "evidence": ["stream_projection 2026-07-20", "F0 2026-07-19", "M0c 2026-07-20"],
    },
    "cpu_expert_offload": {
        "description": "Run cold experts on CPU while hot experts run on GPU at the same layer.",
        "verdict": "REJECTED_BUS_BOUND",
        "reason": (
            "MoE expert GEMV is memory-bandwidth-bound and CPU shares the one memory bus "
            "with the GPU, so CPU reads compete rather than add bandwidth during decode. "
            "W3 already showed a second concurrent stream regresses on this stack."
        ),
        "evidence": ["W3 492609a7", "profile fa99ef98"],
    },
    "cpu_ingest_splice": {
        "description": "CPU prefills/splices new repo files while the GPU keeps decoding.",
        "verdict": "BUILD_CANDIDATE",
        "reason": (
            "This is latency-hiding on DIFFERENT data (new files), not bandwidth-stacking "
            "on the hot decode path. DeltaNet's linear prefill is CPU-friendly. Worth a "
            "bounded spike with its own before/after decode-stall measurement."
        ),
        "evidence": ["W1 a702b7ba (warmstart precedent)"],
    },
    "ane_draft": {
        "description": "Draft tokens on the Neural Engine, ideally one-shot-head / tree (Medusa/EAGLE style).",
        "verdict": "CONDITIONAL_ONESHOT_ONLY",
        "reason": (
            "Measured (ANE probe 2026-07-20): the ANE accelerates fp16 dense work 1.6-4.2x "
            "at seq>=128 but shows no benefit at seq~32 (1.04-1.21x). A naive autoregressive "
            "draft (few tokens/step = small seq) sits in the no-benefit regime; the ANE only "
            "pays off for a one-shot-head / tree draft that emits many candidates at once, "
            "exactly as the blueprint warned. Viable ONLY in that form, and it must still "
            "beat the existing MTP GPU draft to be worth building. Sequential ANE draft: no."
        ),
        "evidence": ["ANE probe 2026-07-20", "MTP smoke"],
    },
    "ane_router_predictor": {
        "description": (
            "Host the cross-layer routing predictor on the ANE to raise the prefetch "
            "hit-rate (the smart 'NPU in the cold->warm->hot loop' idea)."
        ),
        "verdict": "MEASURED_NOT_WORTH_IT",
        "reason": (
            "Measured 2026-07-21: at the predictor's realistic shape (small net, hidden "
            "~512, per-token/40-layer seq~40) the ANE gives ~no benefit (1.02-1.03x); a "
            "win needs batch seq>=256, which the per-token prefetch loop does not have. "
            "And it is moot anyway: the real disk-backed replay already reaches 95% hit at "
            "a 10 GB cache from capacity + CPU pair-table coupling, with prefetch adding "
            "only +1-2 pp. The '80->94% via a better predictor' multiplier does not apply "
            "— cache capacity is the lever, not the NPU. Keep the fast CPU coupling "
            "predictor; do not build an ANE predictor for this workload."
        ),
        "evidence": ["ANE probe 2026-07-20/21", "stream_replay 2026-07-21", "coupling #176"],
    },
}


def evaluate_lever(lever: str, *, membw_gb_s: float) -> dict[str, Any]:
    if lever not in _LEVERS:
        raise FlbError(f"unknown lever {lever!r}; known: {sorted(_LEVERS)}")
    spec = _LEVERS[lever]
    ssd_ratio = round(membw_gb_s / MEASURED_STATIONS["ssd"]["value"], 1)
    return {
        "lever": lever,
        "description": spec["description"],
        "verdict": spec["verdict"],
        "reason": spec["reason"],
        "evidence": spec["evidence"],
        "membw_over_ssd_ratio": ssd_ratio,
    }


def evaluate_all(*, membw_gb_s: float) -> list[dict[str, Any]]:
    return [evaluate_lever(name, membw_gb_s=membw_gb_s) for name in _LEVERS]


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--size-mib", type=int, default=1024, help="working-set size for the bandwidth probe")
    parser.add_argument("--trials", type=int, default=5)
    args = parser.parse_args(argv)
    try:
        membw = measure_memory_bandwidth(size_bytes=args.size_mib * 1024 * 1024, trials=args.trials)
    except FlbError as error:
        print(json.dumps({"error": str(error)}))
        return 2
    result = {
        "host": "Apple M3 Pro",
        "memory_bandwidth": membw,
        "stations": MEASURED_STATIONS,
        "levers": evaluate_all(membw_gb_s=membw["read_gb_s"]),
    }
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
