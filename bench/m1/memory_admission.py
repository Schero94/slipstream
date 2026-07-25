"""Usability-headroom memory admission guard.

Enforces the hard product gate (Plan v3 / Blueprint §3): the engine may only
consume as much unified memory as keeps macOS + foreground apps responsive. It
answers, before a model load, whether the expected resident footprint leaves
enough headroom, and recommends a headroom-preserving wired-limit.

Two checks:
1. **Static ceiling (hard):** resident footprint must fit under
   `total - min_headroom` — even on an idle machine this reserve stays free for
   the OS and the user's apps. Over it => REFUSE.
2. **Current feasibility (soft):** if the footprint fits the ceiling but *right
   now* other apps hold memory (available < footprint + a small margin), loading
   would cause pressure => WARN.

Adapted from colibri's RAM-admission and PowerInfer's `--vram-budget` idea, but
the budget here targets *system stays interactive*, not max speed. Detection is
best-effort; unknown memory fails closed (REFUSE).
"""

from __future__ import annotations

import json
import re
import subprocess
from typing import Any

GIB = 1024 ** 3
DEFAULT_MIN_HEADROOM_BYTES = 3 * GIB  # true free buffer on top of the already-resident
# OS + app working set. Validated on a 36 GiB M-series Mac: a 14 GiB PGRN cache with a
# 3 GiB reserve ran the real 35B model with 0 swapouts and ~30% free after load. The
# reserve does not re-cover apps that are already resident, so 8-9 GiB was overkill.
CURRENT_MARGIN_BYTES = 1 * GIB  # slack for the soft current-feasibility check


class AdmissionError(Exception):
    """Raised for invalid admission inputs."""


class LoadPlanError(Exception):
    """Raised for invalid load-planning inputs."""


def plan_load(
    *,
    total_bytes: int,
    available_bytes: int | None,
    model_bytes: int,
    expert_total_bytes: int,
    kv_bytes: int,
    overhead_bytes: int,
    min_headroom_bytes: int = DEFAULT_MIN_HEADROOM_BYTES,
    layers: int | None = None,
    expert_bytes: int | None = None,
) -> dict[str, Any]:
    """Decide RESIDENT vs STREAMING so a big model still loads and the Mac stays usable.

    The non-streamable core (dense weights + KV + runtime) must be resident. The
    expert weights are streamable: as many as fit the headroom budget stay hot/warm,
    the rest stream cold from SSD via the tiered cache. A model is only refused when
    even the core alone would starve the usability headroom.
    """
    if total_bytes <= 0 or model_bytes <= 0 or min_headroom_bytes < 0:
        raise LoadPlanError("total/model must be > 0 and headroom >= 0")
    if expert_total_bytes < 0 or expert_total_bytes > model_bytes:
        raise LoadPlanError("expert_total_bytes must be in [0, model_bytes]")
    if kv_bytes < 0 or overhead_bytes < 0:
        raise LoadPlanError("kv/overhead must be >= 0")

    dense_bytes = model_bytes - expert_total_bytes
    mandatory_resident = dense_bytes + kv_bytes + overhead_bytes
    ceiling = total_bytes - min_headroom_bytes
    plan: dict[str, Any] = {
        "total_bytes": total_bytes,
        "available_bytes": available_bytes,
        "model_bytes": model_bytes,
        "dense_bytes": dense_bytes,
        "expert_total_bytes": expert_total_bytes,
        "kv_bytes": kv_bytes,
        "overhead_bytes": overhead_bytes,
        "min_headroom_bytes": min_headroom_bytes,
        "static_ceiling_bytes": ceiling,
        "mandatory_resident_bytes": mandatory_resident,
        "recommended_wired_limit_mb": (
            int(ceiling // (1024 * 1024)) if ceiling > 0 else 0
        ),
    }

    if ceiling <= 0 or mandatory_resident > ceiling:
        return {
            **plan,
            "mode": "refuse",
            "streamed_expert_bytes": 0,
            "resident_bytes": mandatory_resident,
            "reason": (
                f"non-streamable core {mandatory_resident / GIB:.1f} GiB (dense+KV+runtime) "
                f"alone exceeds the {ceiling / GIB:.1f} GiB headroom ceiling; even streaming "
                f"cannot make this usable — reduce context/KV or pick a smaller core"
            ),
        }

    expert_resident_budget = ceiling - mandatory_resident
    if expert_total_bytes <= expert_resident_budget:
        resident_experts = expert_total_bytes
        streamed = 0
        mode = "resident"
        reason = (
            f"fits fully resident: core {mandatory_resident / GIB:.1f} + experts "
            f"{expert_total_bytes / GIB:.1f} GiB under the {ceiling / GIB:.1f} GiB ceiling"
        )
    else:
        resident_experts = expert_resident_budget
        streamed = expert_total_bytes - expert_resident_budget
        mode = "streaming"
        reason = (
            f"streaming: {resident_experts / GIB:.1f} GiB experts stay hot/warm, "
            f"{streamed / GIB:.1f} GiB stream cold from SSD; resident stays at the "
            f"{ceiling / GIB:.1f} GiB ceiling so the Mac stays usable"
        )

    plan.update(
        {
            "mode": mode,
            "resident_experts_bytes": resident_experts,
            "streamed_expert_bytes": streamed,
            "resident_bytes": mandatory_resident + resident_experts,
            "reason": reason,
            "execution_note": (
                "resident mode runs on the current llama.cpp gateway today. Smart tiered "
                "streaming (coupling-prefetch hot/warm/cold) is the pending Metal engine; "
                "today's crude fallback is llama.cpp mmap + a bounded wired-limit (OS paging)."
            ),
        }
    )
    if layers and expert_bytes and layers > 0 and expert_bytes > 0:
        plan["resident_experts_per_layer"] = int(resident_experts / layers / expert_bytes)
    return plan


def recommend_wired_limit_mb(*, total_bytes: int, min_headroom_bytes: int) -> int:
    if total_bytes <= 0 or min_headroom_bytes < 0:
        raise AdmissionError("total_bytes must be > 0 and headroom >= 0")
    ceiling = total_bytes - min_headroom_bytes
    if ceiling <= 0:
        raise AdmissionError("min_headroom exceeds total memory")
    return int(ceiling // (1024 * 1024))


def evaluate_admission(
    *,
    total_bytes: int,
    available_bytes: int | None,
    expected_resident_bytes: int,
    min_headroom_bytes: int = DEFAULT_MIN_HEADROOM_BYTES,
) -> dict[str, Any]:
    if total_bytes <= 0 or expected_resident_bytes <= 0 or min_headroom_bytes < 0:
        raise AdmissionError("total/resident must be > 0 and headroom >= 0")
    ceiling = total_bytes - min_headroom_bytes
    recommended_mb = recommend_wired_limit_mb(total_bytes=total_bytes, min_headroom_bytes=min_headroom_bytes)

    base = {
        "total_bytes": total_bytes,
        "available_bytes": available_bytes,
        "expected_resident_bytes": expected_resident_bytes,
        "min_headroom_bytes": min_headroom_bytes,
        "static_ceiling_bytes": ceiling,
        "recommended_wired_limit_mb": recommended_mb,
    }

    if available_bytes is None:
        return {
            **base,
            "status": "REFUSE",
            "reason": "cannot determine available memory; failing closed",
        }
    if expected_resident_bytes > ceiling:
        return {
            **base,
            "status": "REFUSE",
            "reason": (
                f"resident footprint {expected_resident_bytes / GIB:.1f} GiB exceeds the "
                f"{ceiling / GIB:.1f} GiB ceiling (total - headroom); would starve the "
                f"{min_headroom_bytes / GIB:.1f} GiB usability headroom"
            ),
        }
    if available_bytes < expected_resident_bytes + CURRENT_MARGIN_BYTES:
        return {
            **base,
            "status": "WARN",
            "reason": (
                f"footprint fits the ceiling, but only {available_bytes / GIB:.1f} GiB is free "
                f"now; loading may pressure the system until other apps release memory"
            ),
        }
    return {
        **base,
        "status": "OK",
        "system_free_after_load_bytes": total_bytes - expected_resident_bytes,
        "reason": (
            f"engine footprint {expected_resident_bytes / GIB:.1f} GiB leaves "
            f"{(total_bytes - expected_resident_bytes) / GIB:.1f} GiB for OS+apps "
            f"(>= {min_headroom_bytes / GIB:.1f} GiB headroom); {available_bytes / GIB:.1f} GiB free now"
        ),
    }


def detect_total_bytes() -> int | None:
    try:
        out = subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, timeout=5)
        return int(out.stdout.strip())
    except (OSError, ValueError, subprocess.SubprocessError):  # pragma: no cover - env dependent
        return None


def detect_available_bytes(total_bytes: int | None) -> int | None:
    # Primary: macOS `memory_pressure` system-wide free percentage.
    try:
        out = subprocess.run(["memory_pressure"], capture_output=True, text=True, timeout=5).stdout
        match = re.search(r"free percentage:\s*(\d+)%", out)
        if match and total_bytes:
            return int(total_bytes * int(match.group(1)) / 100)
    except (OSError, subprocess.SubprocessError):  # pragma: no cover
        pass
    # Fallback: vm_stat free + inactive + speculative + purgeable pages.
    try:
        out = subprocess.run(["vm_stat"], capture_output=True, text=True, timeout=5).stdout
        page = 4096
        pm = re.search(r"page size of (\d+) bytes", out)
        if pm:
            page = int(pm.group(1))
        pages = 0
        for key in ("Pages free", "Pages inactive", "Pages speculative", "Pages purgeable"):
            m = re.search(rf"{key}:\s*(\d+)", out)
            if m:
                pages += int(m.group(1))
        return pages * page if pages else None
    except (OSError, subprocess.SubprocessError):  # pragma: no cover
        return None


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--total-gb", type=float, help="override detected total RAM")
    parser.add_argument("--available-gb", type=float, help="override detected available RAM")
    parser.add_argument("--model-gb", type=float, required=True, help="GGUF/model bytes")
    parser.add_argument("--kv-gb", type=float, default=2.0, help="expected KV-cache footprint")
    parser.add_argument("--overhead-gb", type=float, default=1.5, help="runtime overhead")
    parser.add_argument("--min-headroom-gb", type=float, default=DEFAULT_MIN_HEADROOM_BYTES / GIB)
    args = parser.parse_args(argv)

    total = int(args.total_gb * GIB) if args.total_gb is not None else detect_total_bytes()
    if total is None:
        print(json.dumps({"status": "REFUSE", "reason": "cannot determine total memory"}))
        return 2
    available = (
        int(args.available_gb * GIB) if args.available_gb is not None else detect_available_bytes(total)
    )
    resident = int((args.model_gb + args.kv_gb + args.overhead_gb) * GIB)
    try:
        verdict = evaluate_admission(
            total_bytes=total,
            available_bytes=available,
            expected_resident_bytes=resident,
            min_headroom_bytes=int(args.min_headroom_gb * GIB),
        )
    except AdmissionError as error:
        print(json.dumps({"status": "REFUSE", "reason": str(error)}))
        return 2
    print(json.dumps(verdict, indent=2))
    return 0 if verdict["status"] in {"OK", "WARN"} else 2


if __name__ == "__main__":
    raise SystemExit(main())
