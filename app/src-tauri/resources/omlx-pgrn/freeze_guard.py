"""Shared FREEZE_RISK / MEMORY kill policy (unit-testable, no side effects).

Lesson (2026-07-30 self-improve): admit@17 mlock drops free+inactive to ~5–6 GiB
by design. Soft floor 8 GiB must not kill a sole coordinated short bench.
Foreign FREEZE_RISK hunters that SIGKILL every omlx under 8 GiB abort valid
quiet warms and PGCT1 arms.

Callers (safety_watchdog, Track J emergency_kill, ad-hoc hunters) should use
``may_kill_heavy_serve`` before SIGTERM/SIGKILL.
"""
from __future__ import annotations

from pathlib import Path

DEFAULT_SOFT_FLOOR_GIB = 8.0
DEFAULT_HARD_FLOOR_GIB = 2.0
DEFAULT_EMERGENCY_GIB = 3.0
DEFAULT_MAX_BENCH_WALL_SEC = 2700

HANDS_OFF = Path("/tmp/slipstream-HANDS_OFF_OMLX.txt")
J_BUSY = Path("/tmp/slipstream-omlx-J-BUSY")


def coordinated_bench_active(
    *,
    hands_off: bool | None = None,
    j_busy: bool | None = None,
    lock_age_sec: float | None = None,
    max_wall_sec: float = DEFAULT_MAX_BENCH_WALL_SEC,
) -> bool:
    """True when a short sole bench is marked and within wall time."""
    if hands_off is None:
        hands_off = HANDS_OFF.is_file()
    if j_busy is None:
        j_busy = J_BUSY.is_file()
    if not hands_off and not j_busy:
        return False
    if lock_age_sec is None:
        return True  # marker present; age unknown → treat as active
    if lock_age_sec < 0:
        return True
    return lock_age_sec <= max_wall_sec


def may_kill_heavy_serve(
    ram_gib: float,
    *,
    heavy_count: int = 1,
    hands_off: bool | None = None,
    j_busy: bool | None = None,
    lock_age_sec: float | None = None,
    soft_floor_gib: float = DEFAULT_SOFT_FLOOR_GIB,
    hard_floor_gib: float = DEFAULT_HARD_FLOOR_GIB,
    emergency_gib: float = DEFAULT_EMERGENCY_GIB,
    max_wall_sec: float = DEFAULT_MAX_BENCH_WALL_SEC,
) -> tuple[bool, str]:
    """Whether to kill a heavy oMLX/PGRN serve for memory/freeze risk.

    Returns ``(may_kill, reason)``.

    Rules (highest priority first):
    1. Always kill if heavy_count >= 2 (dual-serve freeze class).
    2. Always kill if ram < hard_floor (true emergency).
    3. If coordinated short bench (HANDS_OFF / J-BUSY within wall):
       kill only if ram < emergency_gib (default 3). Soft floor 8 is ignored.
    4. Otherwise kill if ram < soft_floor (default 8).
    """
    if heavy_count >= 2:
        return True, f"dual_serve heavy_count={heavy_count}"
    if ram_gib < hard_floor_gib:
        return True, f"hard_floor ram={ram_gib:.2f} < {hard_floor_gib:g}"

    coordinated = coordinated_bench_active(
        hands_off=hands_off,
        j_busy=j_busy,
        lock_age_sec=lock_age_sec,
        max_wall_sec=max_wall_sec,
    )
    if coordinated:
        if ram_gib < emergency_gib:
            return True, (
                f"bench_emergency ram={ram_gib:.2f} < {emergency_gib:g} "
                f"(coordinated bench)"
            )
        return False, (
            f"bench_exempt ram={ram_gib:.2f} soft_floor={soft_floor_gib:g} "
            f"emergency={emergency_gib:g}"
        )

    if ram_gib < soft_floor_gib:
        return True, f"soft_floor ram={ram_gib:.2f} < {soft_floor_gib:g}"
    return False, f"ok ram={ram_gib:.2f}"
