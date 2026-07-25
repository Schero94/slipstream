"""ANE (Apple Neural Engine) capability probe for the FLB `ane_draft` lever.

Answers "can the Neural Engine serve a useful speculative draft?" with numbers,
before any engine build. Method needs no sudo: build one small draft-shaped
fp16 CoreML model, then run it twice — `CPU_ONLY` vs `CPU_AND_NE`. If CPU_AND_NE
is meaningfully faster, the ANE is demonstrably doing work (CoreML would only
route to it when it helps). Absolute per-forward latency says whether an ANE
draft could keep up with the GPU verify budget.

The verdict logic is pure and unit-tested. The CoreML build/run is the runner and
is skipped when coremltools / the CoreML runtime is unavailable (environment
errors skip, they are not assertion failures).
"""

from __future__ import annotations

import json
import time
from typing import Any

# Speedup at/above which we treat the ANE as genuinely contributing.
ANE_SPEEDUP_THRESHOLD = 1.25


class AneProbeError(Exception):
    """Raised for invalid probe configuration."""


class AneUnavailable(Exception):
    """Raised when coremltools / CoreML runtime is not usable (skip, don't fail)."""


def validate_probe_args(*, hidden: int, layers: int, seq: int, iters: int) -> None:
    if hidden < 1 or hidden % 8 != 0:
        raise AneProbeError("hidden must be a positive multiple of 8 (ANE fp16 alignment)")
    for value, label in ((layers, "layers"), (seq, "seq"), (iters, "iters")):
        if value < 1:
            raise AneProbeError(f"{label} must be >= 1")


def decide_verdict(cpu_ms: float, ane_ms: float) -> dict[str, Any]:
    if cpu_ms <= 0 or ane_ms <= 0:
        raise AneProbeError("latencies must be > 0")
    speedup = cpu_ms / ane_ms
    if speedup >= ANE_SPEEDUP_THRESHOLD:
        verdict = "ANE_ACCELERATES"
    else:
        verdict = "NO_ANE_BENEFIT"
    return {
        "cpu_only_ms": round(cpu_ms, 3),
        "cpu_and_ne_ms": round(ane_ms, 3),
        "speedup": round(speedup, 3),
        "verdict": verdict,
    }


def _build_model(hidden: int, layers: int, seq: int):
    try:
        import coremltools as ct
        from coremltools.converters.mil import Builder as mb
    except ImportError as error:  # pragma: no cover - env dependent
        raise AneUnavailable(f"coremltools unavailable: {error}") from error

    import numpy as np

    @mb.program(input_specs=[mb.TensorSpec(shape=(1, seq, hidden))])
    def prog(x):  # a small stack of dense+activation, draft-block-shaped
        y = x
        for _ in range(layers):
            # Build in fp32 (MIL requires matching input dtypes); compute_precision
            # FLOAT16 below makes the runtime execute in ANE-friendly fp16.
            w = np.random.rand(hidden, hidden).astype(np.float32)
            y = mb.matmul(x=y, y=w)
            y = mb.gelu(x=y)
        return y

    try:
        model = ct.convert(
            prog,
            compute_precision=ct.precision.FLOAT16,
            minimum_deployment_target=ct.target.macOS13,
        )
    except Exception as error:  # pragma: no cover - env dependent
        raise AneUnavailable(f"CoreML conversion failed: {error}") from error
    return model


def _time_model(package_path: str, compute_unit, sample, iters: int) -> float:
    import coremltools as ct

    # ML-program models must be loaded from a saved package (spec alone lacks weights).
    model = ct.models.MLModel(package_path, compute_units=compute_unit)
    input_name = model.get_spec().description.input[0].name
    # warmup (first call compiles / loads onto the target)
    model.predict({input_name: sample})
    best = float("inf")
    for _ in range(iters):
        start = time.monotonic()
        model.predict({input_name: sample})
        best = min(best, (time.monotonic() - start) * 1000.0)
    return best


def probe_ane(*, hidden: int = 2048, layers: int = 8, seq: int = 16, iters: int = 20) -> dict[str, Any]:
    validate_probe_args(hidden=hidden, layers=layers, seq=seq, iters=iters)
    try:
        import coremltools as ct
        import numpy as np
    except ImportError as error:  # pragma: no cover
        raise AneUnavailable(f"coremltools/numpy unavailable: {error}") from error

    import tempfile
    from pathlib import Path

    model = _build_model(hidden, layers, seq)
    sample = np.random.rand(1, seq, hidden).astype(np.float32)
    with tempfile.TemporaryDirectory() as tmp:
        package_path = str(Path(tmp) / "draft.mlpackage")
        model.save(package_path)
        cpu_ms = _time_model(package_path, ct.ComputeUnit.CPU_ONLY, sample, iters)
        ane_ms = _time_model(package_path, ct.ComputeUnit.CPU_AND_NE, sample, iters)
    result = decide_verdict(cpu_ms, ane_ms)
    result.update({"hidden": hidden, "layers": layers, "seq": seq, "iters": iters})
    return result


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hidden", type=int, default=2048)
    parser.add_argument("--layers", type=int, default=8)
    parser.add_argument("--seq", type=int, default=16)
    parser.add_argument("--iters", type=int, default=20)
    args = parser.parse_args(argv)
    try:
        result = probe_ane(hidden=args.hidden, layers=args.layers, seq=args.seq, iters=args.iters)
    except AneUnavailable as error:
        print(json.dumps({"skipped": True, "reason": str(error)}))
        return 0
    except AneProbeError as error:
        print(json.dumps({"error": str(error)}))
        return 2
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
