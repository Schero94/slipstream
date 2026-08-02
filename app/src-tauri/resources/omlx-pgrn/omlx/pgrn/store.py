"""Bounded expert fetch via libpgrn_host (portable PGRN C core).

Opens the sidecar and a HOT/WARM stream cache; `fetch_many` / `fetch_bank_mx`
are what MoE staging calls each step. A host-side MX piece LRU sits on top of
the C stream so warm decode skips numpy→mx conversion (the ~1 tok/s ceiling).

Prefetch modes (`SLIPSTREAM_PGRN_PREFETCH`):
  0 / off     — product default; demand fetch only. Opt-in decode sticky after
                keep-hot via SLIPSTREAM_PGRN_STICKY_AFTER_KEEP_HOT=1 (A/B).
  1 / sync    — sticky L→L+1 before get_many (serial; decode experiments)
  overlap     — Metal-style kick/settle for decode-sized sets (≤ kick_budget
                and ≤32 unless keep-hot armed). Prefill unique (100+) skip.
                Cap: SLIPSTREAM_PGRN_PREFETCH_MAX (overlap default max(256, hot)).
                Hide budget: SLIPSTREAM_PGRN_PREFETCH_KICK (default 64; 0=up to max).

Residency (`SLIPSTREAM_PGRN_RESIDENCY`, default ``touch``) + MX keep-hot
(`SLIPSTREAM_PGRN_KEEP_HOT=1`) stabilize warm RSS on the host-owned arena.
``mlock`` is opt-in (benches / short measured runs) — dual mlock can freeze.
Metal external slots are a no-op in the C helpers.

Cold-start (`SLIPSTREAM_PGRN_PRIME=1` default, optional `COLD_IO_WIDTH` /
`WARMUP`): prefetch a seed expert set into the C stream at attach, touch/mlock
those slots, and optionally run a short prompt so the first user request is warm.
Cold I/O boost is opt-in (`SLIPSTREAM_PGRN_COLD_IO_WIDTH=32`); default keeps
profile ``io_width`` (balanced/quality = 16) for stable warm decode.

Predict (default OFF): ``SLIPSTREAM_PGRN_ONLINE=1`` (live co-activation) and/or
``SLIPSTREAM_PGRN_PREDICT=/path.pgct`` (static PGCT1). Parity-neutral warm only.
"""
from __future__ import annotations

import logging
import os
import sys
import time
from collections import OrderedDict
from concurrent.futures import Future, ThreadPoolExecutor
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional, Tuple

from .profile import PgrnProfile, resolve_profile

logger = logging.getLogger(__name__)

# Prefill often misses 100+ experts/layer; keep a process-wide CPU pool so we
# do not pay ThreadPoolExecutor create/teardown on every MoE step.
_CPU_POOL: Optional[ThreadPoolExecutor] = None
_CPU_POOL_WORKERS = 8
# Single worker: pgr_stream_prefetch_many must not race get_many on the stream.
_IO_POOL: Optional[ThreadPoolExecutor] = None


def _cpu_pool() -> ThreadPoolExecutor:
    global _CPU_POOL
    if _CPU_POOL is None:
        _CPU_POOL = ThreadPoolExecutor(
            max_workers=_CPU_POOL_WORKERS,
            thread_name_prefix="pgrn-np",
        )
    return _CPU_POOL


def _io_pool() -> ThreadPoolExecutor:
    global _IO_POOL
    if _IO_POOL is None:
        _IO_POOL = ThreadPoolExecutor(
            max_workers=1,
            thread_name_prefix="pgrn-io",
        )
    return _IO_POOL


def resolve_prefetch_mode(raw: str | None = None) -> str:
    """Return 'off' | 'sync' | 'overlap'."""
    v = (raw if raw is not None else os.environ.get("SLIPSTREAM_PGRN_PREFETCH", "0"))
    v = str(v).strip().lower()
    if v in ("1", "true", "yes", "on", "sync"):
        return "sync"
    if v in ("overlap", "async", "kick"):
        return "overlap"
    return "off"


# Decode sticky sets are tiny (top_k); prefill unique sets are 100+ / layer.
_DEFAULT_PREFETCH_MAX_SYNC = 24
_DEFAULT_PREFETCH_MAX_OVERLAP_FLOOR = 256
# Hide-friendly kick size: ~64 × 1.7 MiB ≈ 110 MiB — fits a convert+GPU window
# on internal NVMe. Full unique sets (100+) make settle longer than the hide.
_DEFAULT_PREFETCH_KICK_BUDGET = 64
# Prefill→decode band: keep-hot arms at ≤32; sticky/predict kicks prefer this.
_DECODE_EXPERT_GATE = 32
# Parallel numpy parse floor (was 8). Prefill miss bursts of 4–7 are common.
_PARALLEL_CONVERT_MIN = 4


def parallel_convert_min() -> int:
    """Public floor for parallel numpy→pieces conversion (unit-tested)."""
    return _PARALLEL_CONVERT_MIN


def bank_reuse_indices(
    order: Tuple[int, ...] | list[int],
    prev_order: Tuple[int, ...] | list[int] | None,
) -> Optional[List[int]]:
    """Gather indices when ``order`` is a permutation of ``prev_order``.

    Returns ``None`` when sets differ, lengths mismatch, or either side has
    duplicate expert ids (ambiguous gather — caller rebuilds). Exact same
    order yields ``list(range(n))`` — caller may short-circuit.
    """
    if prev_order is None:
        return None
    try:
        cur = tuple(int(e) for e in order)
        prev = tuple(int(e) for e in prev_order)
    except (TypeError, ValueError):
        return None
    if len(cur) != len(prev) or not cur:
        return None
    if len(set(cur)) != len(cur) or len(set(prev)) != len(prev):
        return None  # duplicates → rebuild (pos map would be ambiguous)
    if frozenset(cur) != frozenset(prev):
        return None
    pos = {e: i for i, e in enumerate(prev)}
    try:
        return [pos[e] for e in cur]
    except KeyError:
        return None


def resolve_prefetch_max(
    mode: str,
    hot_capacity: int = 0,
    raw: str | None = None,
) -> int:
    """Hard ceiling for sticky kick size (0 = never kick). Truncates, does not skip.

    Env ``SLIPSTREAM_PGRN_PREFETCH_MAX`` wins when set.
    Overlap default: ``max(256, hot_capacity)`` so prefill sets are eligible.
    Sync keeps the conservative 24 (serial warm; decode experiments).
    """
    env = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_PREFETCH_MAX", "")
    )
    env = str(env).strip() if env is not None else ""
    if env:
        try:
            return max(0, int(env))
        except ValueError:
            pass
    if mode == "overlap":
        return max(_DEFAULT_PREFETCH_MAX_OVERLAP_FLOOR, max(0, int(hot_capacity)))
    return _DEFAULT_PREFETCH_MAX_SYNC


def resolve_prefetch_kick_budget(
    mode: str,
    raw: str | None = None,
) -> int:
    """Further truncate kick size for overlap hide (0 = kick up to prefetch_max).

    Env ``SLIPSTREAM_PGRN_PREFETCH_KICK``. Default 64 in overlap mode so settle
    does not serialize a full 100+ expert prefill set behind the next layer.
    """
    env = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_PREFETCH_KICK", "")
    )
    env = str(env).strip() if env is not None else ""
    if env:
        try:
            return max(0, int(env))
        except ValueError:
            pass
    if mode == "overlap":
        return _DEFAULT_PREFETCH_KICK_BUDGET
    return 0


def sticky_after_keep_hot_enabled(raw: str | None = None) -> bool:
    """Opt-in auto sticky kick when ``PREFETCH=0`` after keep-hot.

    Default **OFF**. Track B shipped auto-after-keep-hot before quiet A/B;
    2026-07-30 warm sanity then measured p50≈3.2 tok/s with ``kicked=True`` /
    settle tax on decode layers (vs PERF_RECOVERY ~18.9). Re-enable only via
    ``SLIPSTREAM_PGRN_STICKY_AFTER_KEEP_HOT=1`` for exclusive A/B.
    """
    env = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_STICKY_AFTER_KEEP_HOT", "0")
    )
    return str(env).strip().lower() in ("1", "true", "yes", "on")


def sticky_kick_allowed(
    mode: str,
    n_experts: int,
    *,
    kick_budget: int,
    prefetch_max: int,
    keep_hot_armed: bool,
    decode_gate: int = _DECODE_EXPERT_GATE,
    sticky_after_keep_hot: bool | None = None,
) -> bool:
    """Whether sticky L+1 overlap kick should run (decode-gated).

    - Prefill unique sets (``n > kick_budget``) never kick when budget > 0.
    - ``PREFETCH=overlap``: kick decode-sized sets (≤ decode_gate); larger sets
      only after keep-hot is armed (still ≤ budget). ``kick_budget=0`` disables
      the size gate (bench escape hatch — not a product default).
    - ``PREFETCH=0`` (product): **no** auto-kick by default. Opt-in after
      keep-hot via ``SLIPSTREAM_PGRN_STICKY_AFTER_KEEP_HOT=1`` (decode-sized).
    """
    try:
        n = int(n_experts)
        pmax = int(prefetch_max)
        budget = int(kick_budget)
        gate = max(1, int(decode_gate))
    except (TypeError, ValueError):
        return False
    mode_n = str(mode or "off").strip().lower()
    if n <= 0 or pmax <= 0 or n > pmax:
        return False
    if mode_n == "overlap":
        if budget > 0 and n > budget:
            return False
        if budget > 0 and n > gate and not keep_hot_armed:
            return False
        return True
    if mode_n == "off" and keep_hot_armed:
        if sticky_after_keep_hot is None:
            sticky_after_keep_hot = sticky_after_keep_hot_enabled()
        if not sticky_after_keep_hot:
            return False
        # Opt-in auto decode sticky: use default hide budget when mode-off left it at 0.
        eff = budget if budget > 0 else _DEFAULT_PREFETCH_KICK_BUDGET
        return n <= min(gate, eff, pmax)
    return False


def prime_experts_for_layer(
    layer: int,
    take: int,
    epl: int,
    *,
    pgct1: Any = None,
) -> Tuple[List[int], str]:
    """Seed expert ids for ``prime_hot_set`` (PGCT1 hot-set when present).

    Returns ``(ids, source)`` where source is ``pgct1`` or ``sequential``.
    Same wired footprint as ``range(take)`` — only the identity of pages changes.
    """
    take = max(0, min(int(take), int(epl)))
    if take <= 0 or int(epl) <= 0:
        return [], "sequential"
    hot: List[int] = []
    if pgct1 is not None:
        try:
            raw = pgct1.hot(int(layer), max_n=take)
        except Exception:  # noqa: BLE001 — soft-fail to sequential seed
            raw = []
        for v in raw or []:
            try:
                e = int(v)
            except (TypeError, ValueError):
                continue
            if 0 <= e < int(epl) and e not in hot:
                hot.append(e)
            if len(hot) >= take:
                break
    if not hot:
        return list(range(take)), "sequential"
    out = list(hot)
    if len(out) < take:
        seen = set(out)
        for i in range(int(epl)):
            if i not in seen:
                out.append(i)
                if len(out) >= take:
                    break
    return out[:take], "pgct1"


# Per-step and cache_stats averages share these keys (TIMING=1 A/B clarity).
TIMING_KEYS: Tuple[str, ...] = (
    "settle_ms",
    "get_many_ms",
    "convert_ms",
    "stack_ms",
    "prefetch_kick_ms",
)


def resolve_timing_enabled(raw: str | None = None) -> bool:
    """``SLIPSTREAM_PGRN_TIMING=1`` — layer settle/get_many/convert/stack/kick ms."""
    flag = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_TIMING", "")
    )
    return str(flag).strip().lower() in ("1", "true", "yes", "on")


def _timing_enabled() -> bool:
    """Backward-compatible alias for :func:`resolve_timing_enabled`."""
    return resolve_timing_enabled()


def new_timing_totals() -> Dict[str, float]:
    """Zeroed accumulator for ``cache_stats()['timing_avg_ms']``."""
    return {k: 0.0 for k in TIMING_KEYS}


def step_timing_ms(
    *,
    settle_ms: float = 0.0,
    get_many_ms: float = 0.0,
    convert_ms: float = 0.0,
    stack_ms: float = 0.0,
    prefetch_kick_ms: float = 0.0,
) -> Dict[str, float]:
    """Rounded per-step ``timing_ms`` dict (same keys as averages)."""
    return {
        "settle_ms": round(settle_ms, 3),
        "get_many_ms": round(get_many_ms, 3),
        "convert_ms": round(convert_ms, 3),
        "stack_ms": round(stack_ms, 3),
        "prefetch_kick_ms": round(prefetch_kick_ms, 3),
    }


def resolve_residency_mode(raw: str | None = None) -> str:
    """Return 'off' | 'touch' | 'mlock' (default touch — interactive-safe).

    Env ``SLIPSTREAM_PGRN_RESIDENCY``: ``0``/``off``, ``touch`` (product default),
    ``1``/``mlock`` (opt-in for benches / short measured warm runs).
    Metal-external arenas ignore the C lock helper (no-op); this is for oMLX host.
    """
    v = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_RESIDENCY", "touch")
    )
    v = str(v).strip().lower()
    if v in ("0", "false", "no", "off", "none"):
        return "off"
    if v in ("1", "true", "yes", "on", "mlock", "lock"):
        return "mlock"
    if v in ("touch", "fault", "willneed"):
        return "touch"
    # Unrecognized / empty → interactive-safe default (not mlock).
    return "touch"


def resolve_keep_hot(raw: str | None = None) -> bool:
    """Protect post-prefill MX pieces from LRU until unprotected victims exist."""
    v = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_KEEP_HOT", "1")
    )
    v = str(v).strip().lower()
    return v not in ("0", "false", "no", "off")


# Seed: top_k-sized experts × layers, capped so attach stays sub-second-ish on NVMe.
_DEFAULT_PRIME_EXPERTS = 8
_DEFAULT_PRIME_MAX_TOTAL = 320  # 40 layers × 8 on 35B A3B


def resolve_cold_io_width(profile_io: int, raw: str | None = None) -> int:
    """Effective stream ``io_width`` including optional cold boost.

    Env ``SLIPSTREAM_PGRN_COLD_IO_WIDTH``: unset / ``0`` → profile_io only
    (product default — balanced/quality stay at io=16 for stable warm).
    Positive value → ``max(profile, value)`` (opt-in ``32`` for cold A/B).
    Explicit ``SLIPSTREAM_PGRN_IO_WIDTH`` also forces profile width (no boost).
    """
    base = max(1, int(profile_io))
    env = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_COLD_IO_WIDTH", "")
    )
    env = str(env).strip() if env is not None else ""
    if env:
        try:
            v = int(env)
            if v <= 0:
                return base
            return max(base, v)
        except ValueError:
            pass
    # Explicit profile IO override means the user already chose width.
    if os.environ.get("SLIPSTREAM_PGRN_IO_WIDTH", "").strip():
        return base
    # Default: no silent boost (was max(profile, 32) when profile ≥ 8).
    return base


def resolve_prime_enabled(raw: str | None = None) -> bool:
    """Prefetch + touch/mlock a seed set at attach (default on)."""
    v = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_PRIME", "1")
    )
    v = str(v).strip().lower()
    return v not in ("0", "false", "no", "off")


def resolve_prime_experts_per_layer(raw: str | None = None) -> int:
    env = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_PRIME_EXPERTS", "")
    )
    env = str(env).strip() if env is not None else ""
    if env:
        try:
            return max(0, int(env))
        except ValueError:
            pass
    return _DEFAULT_PRIME_EXPERTS


def resolve_prime_max_total(raw: str | None = None) -> int:
    env = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_PRIME_MAX", "")
    )
    env = str(env).strip() if env is not None else ""
    if env:
        try:
            return max(0, int(env))
        except ValueError:
            pass
    return _DEFAULT_PRIME_MAX_TOTAL


def resolve_warmup_enabled(raw: str | None = None) -> bool:
    """Optional short generate after load so first user request is warm."""
    v = (
        raw
        if raw is not None
        else os.environ.get("SLIPSTREAM_PGRN_WARMUP", "0")
    )
    v = str(v).strip().lower()
    return v in ("1", "true", "yes", "on")


def _repo_pgrn_host() -> Optional[Path]:
    """Locate pgrn_host.py: env, bundled sibling, or checkout tools path."""
    here = Path(__file__).resolve()
    env = Path(os.environ.get("SLIPSTREAM_PGRN_HOST", "").strip())
    candidates = [
        env,
        # Bundled: …/omlx-pgrn/omlx/pgrn/store.py → …/omlx-pgrn/pgrn_host.py
        here.parents[2] / "pgrn_host.py",
        # Checkout: …/vendor/omlx/omlx/pgrn/store.py → repo/tools/…
        here.parents[4] / "tools" / "pgrn-mlx" / "pgrn_host.py",
    ]
    for c in candidates:
        if c and c.is_file():
            return c
    return None


def pgrn_sidecar_path(model_path: str | Path) -> Optional[Path]:
    root = Path(model_path)
    for candidate in (root / "experts.pgrn", root.parent / f"{root.name}.pgrn"):
        if candidate.is_file():
            return candidate
    override = os.environ.get("SLIPSTREAM_PGRN_PATH", "").strip()
    if override:
        p = Path(override)
        if p.is_file():
            return p
    return None


def streaming_requested(model_path: str | Path) -> bool:
    flag = os.environ.get("SLIPSTREAM_PGRN", "").strip().lower()
    if flag in ("0", "false", "no", "off"):
        return False
    if flag in ("1", "true", "yes", "on"):
        return pgrn_sidecar_path(model_path) is not None
    # Auto: sidecar present → stream (opt out with SLIPSTREAM_PGRN=0).
    return pgrn_sidecar_path(model_path) is not None


class PgrnMoEStore:
    """One open PGRN file for a model; bounded HOT/WARM expert cache."""

    def __init__(
        self,
        pgrn_path: Path,
        lib_path: Optional[Path] = None,
        profile: Optional[PgrnProfile] = None,
    ):
        host_py = _repo_pgrn_host()
        if host_py is None:
            raise RuntimeError(
                "pgrn_host.py not found; build tools/pgrn-mlx (make) and/or set "
                "SLIPSTREAM_PGRN_HOST"
            )
        sys.path.insert(0, str(host_py.parent))
        from pgrn_host import PgrnFile  # type: ignore

        lib = lib_path
        if lib is None:
            env_lib = os.environ.get("SLIPSTREAM_PGRN_LIB", "").strip()
            lib = Path(env_lib) if env_lib else None
        self._file = PgrnFile.open(pgrn_path, lib_path=lib)
        self.path = Path(pgrn_path)
        self.profile = profile or resolve_profile()
        self._io_width = resolve_cold_io_width(self.profile.io_width)
        self._stream = self._file.open_stream(
            capacity=self.profile.capacity,
            io_width=self._io_width,
            hot_capacity=self.profile.hot_capacity,
            clox_k=self.profile.clox_k,
        )
        # Host MX piece cache — same slot budget as the C stream.
        self._mx_cap = int(self.profile.capacity)
        self._mx: "OrderedDict[Tuple[int, int], Dict[str, Any]]" = OrderedDict()
        self._mx_hits = 0
        self._mx_misses = 0
        self._mx_protected: set[Tuple[int, int]] = set()
        self._keep_hot = resolve_keep_hot()
        self._keep_hot_armed = False
        self._residency = resolve_residency_mode()
        self._residency_applied = False
        self._residency_bytes = 0
        self._prime_stats: Dict[str, Any] = {"primed": 0, "skipped": True}
        self._prefetch_mode = resolve_prefetch_mode()
        self._prefetch_max = resolve_prefetch_max(
            self._prefetch_mode,
            hot_capacity=int(self.profile.hot_capacity),
        )
        self._prefetch_kick_budget = resolve_prefetch_kick_budget(self._prefetch_mode)
        self._prev_layer: Optional[int] = None
        self._prev_experts: List[int] = []
        self._prefetch_future: Optional[Future] = None
        self._prefetch_target_layer: Optional[int] = None
        self._timing_on = resolve_timing_enabled()
        self._timing_totals: Dict[str, float] = new_timing_totals()
        self._timing_steps = 0
        # L3 on-demand (HF / file:// / LAN peer) — default OFF; never rewrites experts.pgrn.
        from .on_demand import ExpertOnDemandCache, demo_serve_enabled

        self._ondemand: Optional[Any] = ExpertOnDemandCache.from_env(self.path)
        self._ondemand_demo_serve = bool(
            self._ondemand is not None and demo_serve_enabled()
        )
        # PGCT1 / online predict — default OFF (Metal parity: opt-in warm only).
        from .predict import build_predictors, resolve_online_enabled, resolve_predict_path

        geo_layers = 0
        try:
            from .stage import read_pgrn_meta
            from .on_demand import layer_ids_from_meta

            _meta = read_pgrn_meta(self.path)
            geo_layers = len(layer_ids_from_meta(_meta)) or int(
                (_meta.get("geometry") or {}).get("layers_with_experts") or 0
            )
        except Exception:
            geo_layers = 0
        epl = int(self._file.experts_per_layer())
        self._pgct1, self._online = build_predictors(
            layers=max(geo_layers, 2),
            experts=max(epl, 1),
        )
        self._predict_path = resolve_predict_path()
        self._online_enabled = resolve_online_enabled()
        self._predict_kicks = 0
        _l3_mode = (
            getattr(self._ondemand, "mode", "hf") if self._ondemand is not None else "off"
        )
        logger.info(
            "PGRN stream profile=%s capacity=%d hot=%d io_width=%d (profile=%d) "
            "high_water=%.2f GiB prefetch=%s max=%d kick_budget=%d residency=%s "
            "keep_hot=%s prime=%s timing=%s l3=%s demo_serve=%s online=%s pgct1=%s",
            self.profile.name,
            self.profile.capacity,
            self.profile.hot_capacity,
            self._io_width,
            self.profile.io_width,
            self._stream.high_water_bytes() / (1024**3),
            self._prefetch_mode,
            self._prefetch_max,
            self._prefetch_kick_budget,
            self._residency,
            "on" if self._keep_hot else "off",
            "on" if resolve_prime_enabled() else "off",
            "on" if self._timing_on else "off",
            _l3_mode,
            "on" if self._ondemand_demo_serve else "off",
            "on" if self._online is not None else "off",
            "on" if self._pgct1 is not None else "off",
        )

    @classmethod
    def open_for_model(cls, model_path: str | Path) -> Optional["PgrnMoEStore"]:
        if not streaming_requested(model_path):
            return None
        side = pgrn_sidecar_path(model_path)
        if side is None:
            return None
        return cls(side)

    def close(self) -> None:
        self.prefetch_settle()
        if getattr(self, "_stream", None) is not None:
            self._stream.close()
            self._stream = None
        self._file.close()

    @property
    def experts_per_layer(self) -> int:
        return self._file.experts_per_layer()

    def read_expert(self, layer: int, expert: int) -> bytes:
        return self._file.read_expert(layer, expert)

    def ensure_on_demand(self, layer: int, experts: list[int]) -> dict:
        """Miss→fetch→stage for L3 catalog experts. No-op when flag OFF.

        Fail closed: raises if on-demand is enabled and a listed expert cannot
        be fetched (offline / missing catalog entry / checksum).
        """
        if self._ondemand is None or not experts:
            return {"enabled": False, "ensured": 0}
        paths = self._ondemand.ensure_many(int(layer), [int(e) for e in experts])
        return {
            "enabled": True,
            "ensured": len(paths),
            "stats": dict(self._ondemand.stats),
        }

    def _fill_mx_from_on_demand(
        self,
        layer: int,
        experts: List[int],
        layout: Mapping[str, Any],
        preserve: set[Tuple[int, int]] | None = None,
    ) -> List[int]:
        """Stage L3 experts; when demo-serve is on, convert into MX and drop them.

        Returns experts that still need the local C stream ``get_many``.
        Prefetch-only mode (flag on, demo-serve off) stages bytes then falls
        through to the fast local ``experts.pgrn`` path.
        """
        if self._ondemand is None or not experts:
            return experts
        # Always prove miss→fetch→stage (product: warm side cache).
        self._ondemand.ensure_many(int(layer), experts)
        if not self._ondemand_demo_serve:
            return experts

        from .stage import numpy_pieces_to_mx, record_to_numpy

        for e in experts:
            rec = self._ondemand.read(int(layer), int(e))
            self._mx_put(
                layer,
                e,
                numpy_pieces_to_mx(record_to_numpy(rec, layout, own=False), layout),
                preserve=preserve,
            )
            self._mx_misses += 1
        return []

    def fetch_many(self, layer: int, experts: list[int]) -> tuple[list[bytes], list[int]]:
        """Pinned-slot fetch; copies out so callers outlive the next batch_begin."""
        self.prefetch_settle()
        if self._ondemand is not None and experts:
            # Stage into side cache; serve still comes from local pgrn here.
            self.ensure_on_demand(layer, experts)
        views, hits = self._stream.get_many(layer, experts)
        return [bytes(v) for v in views], hits

    def prefetch_experts(self, layer: int, experts: list[int]) -> int:
        """Best-effort COLD warm via pgr_stream_prefetch_many (no pin, no error)."""
        if not experts or getattr(self, "_stream", None) is None:
            return 0
        return int(self._stream.prefetch_many(layer, experts))

    def prefetch_settle(self) -> int:
        """Join any in-flight overlap prefetch (Metal settle). Returns warmed count."""
        fut = self._prefetch_future
        if fut is None:
            return 0
        warmed = 0
        try:
            warmed = int(fut.result() or 0)
        except Exception as exc:  # noqa: BLE001 — best-effort warm
            logger.debug("PGRN prefetch settle ignored error: %s", exc)
        self._prefetch_future = None
        self._prefetch_target_layer = None
        return max(0, warmed)

    def _predicted_next(self, src_layer: int, fired: list[int]) -> list[int]:
        """Union of online + PGCT1 predictions for ``src_layer+1`` (may be empty).

        Always runs online observe (learning) even when a later kick is skipped.
        """
        from .predict import merge_expert_ids

        budget = self._prefetch_kick_budget or self._prefetch_max or 64
        budget = max(1, min(int(budget), 256))
        online_ids: list[int] = []
        if self._online is not None:
            online_ids = list(
                self._online.observe_and_predict(src_layer, fired, budget=budget)
            )
        pgct_ids: list[int] = []
        if self._pgct1 is not None:
            pgct_ids = list(self._pgct1.hot(int(src_layer) + 1, max_n=budget))
        epl = 0
        try:
            epl = int(self.experts_per_layer())
        except Exception:  # noqa: BLE001 — geometry optional for merge filter
            epl = 0
        return merge_expert_ids(
            online_ids,
            pgct_ids,
            budget=budget,
            experts=epl if epl > 0 else None,
        )

    def prefetch_predicted(self, src_layer: int, fired: list[int]) -> int:
        """Warm ``src_layer+1`` from online/PGCT1 (parity-neutral). Returns warmed count.

        Uses sync ``prefetch_experts`` so an in-flight overlap sticky kick is
        not settled/cancelled. Mispredict only wastes a speculative read.

        Prefill-sized fired sets still update the online table via
        ``_predicted_next``, but skip the SSD kick — same decode hide rule as
        sticky overlap (``predict_kick_allowed`` + decode gate / keep-hot).
        """
        from .predict import predict_kick_allowed

        targets = self._predicted_next(src_layer, fired)
        if not targets:
            return 0
        # Hide-gate: overlap uses PREFETCH_KICK (0 = no size gate). When prefetch
        # is off/sync, still apply the default 64 so ONLINE=1 does not serialize
        # prefill unique sets (92–134) behind speculative SSD reads.
        gate_budget = int(self._prefetch_kick_budget)
        if gate_budget <= 0 and self._prefetch_mode != "overlap":
            gate_budget = _DEFAULT_PREFETCH_KICK_BUDGET
        n_fired = len(fired)
        if not predict_kick_allowed(
            n_fired,
            gate_budget,
            keep_hot_armed=bool(self._keep_hot_armed),
            decode_gate=_DECODE_EXPERT_GATE,
        ):
            return 0
        n = self.prefetch_experts(int(src_layer) + 1, targets)
        if n:
            self._predict_kicks += 1
        return int(n)

    def prefetch_kick(self, layer: int, experts: list[int]) -> bool:
        """Start background sticky warm for `layer` (must not hold stream pins).

        Prefill unique sets make settle longer than the convert/GPU hide window —
        measured net loss vs demand fetch. Decode-gated via
        :func:`sticky_kick_allowed` (kick_budget + keep-hot / ≤32 band).
        Product ``PREFETCH=0`` auto-kicks only after keep-hot so prefill never
        pays settle tax. Set ``SLIPSTREAM_PGRN_PREFETCH_KICK=0`` under
        ``PREFETCH=overlap`` to disable the size gate (bench escape hatch).
        """
        if not experts or getattr(self, "_stream", None) is None:
            return False
        if not sticky_kick_allowed(
            self._prefetch_mode,
            len(experts),
            kick_budget=int(self._prefetch_kick_budget),
            prefetch_max=int(self._prefetch_max),
            keep_hot_armed=bool(self._keep_hot_armed),
        ):
            return False
        # One in flight; join stale work before replacing.
        self.prefetch_settle()
        targets = [int(e) for e in experts]
        target_layer = int(layer)
        stream = self._stream

        def _run() -> int:
            return int(stream.prefetch_many(target_layer, targets))

        self._prefetch_target_layer = target_layer
        self._prefetch_future = _io_pool().submit(_run)
        return True

    def _mx_evict_one(
        self, preserve: set[Tuple[int, int]] | None = None
    ) -> bool:
        """Evict LRU outside the active bank; prefer unprotected keys."""
        if not self._mx:
            return False
        keep = preserve or set()
        if self._keep_hot_armed and self._mx_protected:
            for key in self._mx:
                if key not in self._mx_protected and key not in keep:
                    self._mx.pop(key, None)
                    return True
        for key in self._mx:
            if key not in keep:
                self._mx.pop(key, None)
                self._mx_protected.discard(key)
                return True
        return False

    def _mx_put(
        self,
        layer: int,
        expert: int,
        pieces: Dict[str, Any],
        preserve: set[Tuple[int, int]] | None = None,
    ) -> None:
        key = (int(layer), int(expert))
        if key in self._mx:
            self._mx.move_to_end(key)
            self._mx[key] = pieces
            return
        while len(self._mx) >= self._mx_cap:
            if not self._mx_evict_one(preserve=preserve):
                raise RuntimeError(
                    "PGRN MX cache cannot fit the active expert bank "
                    f"(capacity={self._mx_cap}, required>{len(self._mx)})"
                )
        self._mx[key] = pieces

    def _arm_keep_hot(self) -> None:
        """Freeze the current MX working set against LRU (post-prefill)."""
        if not self._keep_hot or self._keep_hot_armed:
            return
        self._mx_protected = set(self._mx.keys())
        self._keep_hot_armed = True
        logger.info(
            "PGRN keep-hot armed: protected=%d mx_size=%d",
            len(self._mx_protected),
            len(self._mx),
        )

    def prime_hot_set(self, meta: Optional[Mapping[str, Any]] = None) -> dict:
        """Prefetch a seed expert set into the C stream and touch/mlock it.

        Cheap cold-start win: SSD pages + stream slots are warm before the first
        user prefill. Does not convert to MX (optional ``SLIPSTREAM_PGRN_WARMUP``
        covers that). Default on; disable with ``SLIPSTREAM_PGRN_PRIME=0``.
        """
        out: Dict[str, Any] = {
            "primed": 0,
            "layers": 0,
            "experts_per_layer": 0,
            "ms": 0.0,
            "skipped": False,
            "residency": {},
            "seed": "sequential",
        }
        if not resolve_prime_enabled() or getattr(self, "_stream", None) is None:
            out["skipped"] = True
            self._prime_stats = out
            return out

        from .on_demand import layer_ids_from_meta
        from .stage import read_pgrn_meta

        if meta is None:
            try:
                meta = read_pgrn_meta(self.path)
            except Exception as exc:
                logger.warning("PGRN prime: meta read failed: %s", exc)
                out["skipped"] = True
                out["error"] = str(exc)
                self._prime_stats = out
                return out

        layers = layer_ids_from_meta(meta)
        per = resolve_prime_experts_per_layer()
        budget = resolve_prime_max_total()
        epl = int(self.experts_per_layer)
        if per <= 0 or budget <= 0 or not layers or epl <= 0:
            out["skipped"] = True
            self._prime_stats = out
            return out
        per = min(per, epl)
        # Stay inside stream capacity.
        budget = min(budget, int(self.profile.capacity))

        t0 = time.perf_counter()
        primed = 0
        used_layers = 0
        seed_source = "sequential"
        pgct1 = getattr(self, "_pgct1", None)
        for layer in layers:
            if primed >= budget:
                break
            take = min(per, budget - primed)
            experts, src = prime_experts_for_layer(
                int(layer), take, epl, pgct1=pgct1
            )
            if src == "pgct1":
                seed_source = "pgct1"
            n = self.prefetch_experts(int(layer), experts)
            primed += int(n) if n else take
            used_layers += 1
        # Wire whatever landed (touch/mlock). force=True: ignore MX fill threshold.
        res = self.ensure_resident(force=True)
        ms = (time.perf_counter() - t0) * 1000.0
        out.update(
            primed=primed,
            layers=used_layers,
            experts_per_layer=per,
            ms=round(ms, 2),
            residency=res,
            seed=seed_source,
        )
        self._prime_stats = out
        logger.info(
            "PGRN prime-at-load: primed≈%d experts across %d layers in %.1f ms "
            "(per_layer=%d seed=%s io=%d locked=%s touched=%.2f MiB)",
            primed,
            used_layers,
            ms,
            per,
            seed_source,
            self._io_width,
            res.get("locked"),
            float(res.get("touched_bytes") or 0) / (1024 * 1024),
        )
        return out

    def ensure_resident(self, force: bool = False) -> dict:
        """Touch/mlock occupied host slots after the working set has filled.

        Default ``SLIPSTREAM_PGRN_RESIDENCY=touch`` (interactive-safe).
        ``mlock`` is opt-in; soft-fails to touch-only if lock fails.
        No-op for Metal external arenas (C helper returns 0 immediately).
        Re-entrant in mlock mode so newly published slots get wired too.
        """
        out = {
            "mode": self._residency,
            "locked": False,
            "touched_bytes": 0,
            "applied": self._residency_applied,
        }
        if self._residency == "off" or getattr(self, "_stream", None) is None:
            return out
        filled = len(self._mx)
        # Wait until a meaningful set is resident before first wiring.
        if (
            not force
            and not self._residency_applied
            and filled < max(8, int(self.profile.hot_capacity) // 8)
        ):
            return out
        # touch-only: once is enough unless forced.
        if (
            self._residency_applied
            and not force
            and self._residency == "touch"
        ):
            out["touched_bytes"] = int(self._residency_bytes)
            return out
        warm = int(self._stream.warm_hits()) + int(self._stream.hot_hits())
        touched = int(self._stream.touch_resident())
        locked = False
        if self._residency == "mlock":
            locked = bool(self._stream.lock_resident())
            if not locked and not self._residency_applied:
                logger.warning(
                    "PGRN mlock failed after touch=%d bytes (warm+hot hits=%d); "
                    "continuing with touch-only residency",
                    touched,
                    warm,
                )
        first = not self._residency_applied
        self._residency_bytes = max(int(self._residency_bytes), touched)
        self._residency_applied = True
        out.update(
            locked=locked or bool(self._stream.resident_locked()),
            touched_bytes=touched,
            applied=True,
        )
        if first:
            logger.info(
                "PGRN residency mode=%s locked=%s touched=%.2f MiB mx_size=%d",
                self._residency,
                out["locked"],
                touched / (1024 * 1024),
                filled,
            )
        return out

    def fetch_bank_mx(
        self,
        layer: int,
        experts: List[int],
        layout: Mapping[str, Any],
    ) -> Tuple[Dict[str, Any], dict]:
        """Return a stacked MX expert bank, reusing host-side converted pieces."""
        import mlx.core as mx

        if not experts:
            return {}, {
                "unique_experts": 0,
                "cache_hits": 0,
                "cache_misses": 0,
                "mx_hits": 0,
                "mx_misses": 0,
                "prefetch_warmed": 0,
                "prefetch_mode": self._prefetch_mode,
            }

        active_keys = {(int(layer), int(e)) for e in experts}
        if len(active_keys) > self._mx_cap:
            raise RuntimeError(
                "PGRN MX cache is smaller than the active expert bank "
                f"(capacity={self._mx_cap}, required={len(active_keys)})"
            )

        t0 = time.perf_counter() if self._timing_on else 0.0
        # Join speculative warm aimed at THIS layer before touching the stream.
        settled = self.prefetch_settle()
        settle_ms = (time.perf_counter() - t0) * 1000.0 if self._timing_on else 0.0

        # Sync sticky: previous layer's ids often recur on L+1 — serial warm.
        # Overlap mode does kick after pin release instead (see below).
        warmed = settled
        if (
            self._prefetch_mode == "sync"
            and self._prev_layer is not None
            and int(layer) == int(self._prev_layer) + 1
            and self._prefetch_max > 0
            and self._prev_experts
        ):
            sync_n = min(len(self._prev_experts), self._prefetch_max)
            warmed += self.prefetch_experts(
                int(layer), self._prev_experts[:sync_n]
            )

        missing: List[int] = []
        mx_hits = 0
        for e in experts:
            key = (int(layer), int(e))
            if key in self._mx:
                self._mx.move_to_end(key)
                mx_hits += 1
            else:
                missing.append(int(e))

        stream_hits = 0
        stream_misses = 0
        get_many_ms = 0.0
        convert_ms = 0.0
        kick_ms = 0.0
        kicked = False
        on_demand_served = 0
        if missing and self._ondemand is not None:
            before = len(missing)
            missing = self._fill_mx_from_on_demand(
                layer, missing, layout, preserve=active_keys
            )
            on_demand_served = before - len(missing)
        if missing:
            from .stage import numpy_pieces_to_mx, record_to_numpy

            t1 = time.perf_counter() if self._timing_on else 0.0
            views, hits = self._stream.get_many(layer, missing)
            # Copy out then release pins ASAP so overlap kick can run during
            # CPU convert (Metal: no mid-batch prefetch).
            owned = [bytes(v) for v in views]
            self._stream.batch_begin()
            if self._timing_on:
                get_many_ms = (time.perf_counter() - t1) * 1000.0

            # Kick L+1 while we convert this layer's misses (sticky and/or predict).
            # sticky_kick_allowed also covers PREFETCH=0 auto-after-keep-hot.
            if (
                self._prefetch_mode in ("overlap", "off")
                or self._online
                or self._pgct1
            ):
                t_kick = time.perf_counter() if self._timing_on else 0.0
                kicked = self.prefetch_kick(int(layer) + 1, experts)
                if self._online is not None or self._pgct1 is not None:
                    pw = self.prefetch_predicted(int(layer), experts)
                    warmed += pw
                    kicked = bool(kicked or pw)
                if self._timing_on:
                    kick_ms = (time.perf_counter() - t_kick) * 1000.0

            t2 = time.perf_counter() if self._timing_on else 0.0
            if len(missing) >= _PARALLEL_CONVERT_MIN:
                np_list = list(
                    _cpu_pool().map(
                        lambda rec: record_to_numpy(rec, layout, own=False),
                        owned,
                    )
                )
                for e, np_pieces in zip(missing, np_list):
                    self._mx_put(
                        layer,
                        e,
                        numpy_pieces_to_mx(np_pieces, layout),
                        preserve=active_keys,
                    )
                    self._mx_misses += 1
            else:
                for e, rec in zip(missing, owned):
                    self._mx_put(
                        layer,
                        e,
                        numpy_pieces_to_mx(
                            record_to_numpy(rec, layout, own=False), layout
                        ),
                        preserve=active_keys,
                    )
                    self._mx_misses += 1
            stream_hits = int(sum(hits))
            stream_misses = int(len(hits) - stream_hits)
            if self._timing_on:
                convert_ms = (time.perf_counter() - t2) * 1000.0
        elif (
            self._prefetch_mode in ("overlap", "off")
            or self._online
            or self._pgct1
        ):
            # Warm path: stream idle — sticky and/or predict kick during stack/GPU.
            t_kick = time.perf_counter() if self._timing_on else 0.0
            kicked = self.prefetch_kick(int(layer) + 1, experts)
            if self._online is not None or self._pgct1 is not None:
                pw = self.prefetch_predicted(int(layer), experts)
                warmed += pw
                kicked = bool(kicked or pw)
            if self._timing_on:
                kick_ms = (time.perf_counter() - t_kick) * 1000.0

        self._mx_hits += mx_hits

        t3 = time.perf_counter() if self._timing_on else 0.0
        # Reuse last stacked bank when the expert *set* is unchanged (sticky
        # decode). Exact order → full reuse; permutation → axis-0 gather so
        # gather_qmm remap via current expert_ids stays correct.
        order = tuple(int(e) for e in experts)
        bank_key = (int(layer), order)
        set_key = (int(layer), frozenset(order))
        last_bank = getattr(self, "_last_bank", None)
        prev_order = getattr(self, "_last_bank_order", None)
        if last_bank is not None and getattr(self, "_last_bank_key", None) == bank_key:
            bank = last_bank
        else:
            idx = bank_reuse_indices(order, prev_order)
            if last_bank is not None and idx is not None:
                if idx == list(range(len(order))):
                    bank = last_bank
                else:
                    perm = mx.array(idx, dtype=mx.int32)
                    bank = {k: v[perm] for k, v in last_bank.items()}
            else:
                per = [self._mx[(int(layer), int(e))] for e in experts]
                bank = {k: mx.stack([p[k] for p in per], axis=0) for k in per[0]}
            self._last_bank_key = bank_key
            self._last_bank = bank
            self._last_bank_order = order
            self._last_bank_set_key = set_key
        stack_ms = (time.perf_counter() - t3) * 1000.0 if self._timing_on else 0.0

        self._prev_layer = int(layer)
        self._prev_experts = [int(e) for e in experts]

        # After prefill has filled the MX/stream working set: wire pages + protect.
        # Re-lock on further cold publishes so new slots stay wired.
        if stream_misses or not self._residency_applied:
            self.ensure_resident(force=bool(stream_misses and self._residency_applied))
        # Prefill→decode transition: large MX set + decode-sized expert list.
        if (
            not self._keep_hot_armed
            and self._keep_hot
            and len(self._mx) >= max(8, int(self.profile.hot_capacity) // 2)
            and len(experts) <= 32
        ):
            self._arm_keep_hot()

        if self._timing_on:
            self._timing_totals["settle_ms"] += settle_ms
            self._timing_totals["get_many_ms"] += get_many_ms
            self._timing_totals["convert_ms"] += convert_ms
            self._timing_totals["stack_ms"] += stack_ms
            self._timing_totals["prefetch_kick_ms"] += kick_ms
            self._timing_steps += 1

        stats = {
            "unique_experts": len(experts),
            "cache_hits": stream_hits,
            "cache_misses": stream_misses,
            "mx_hits": mx_hits,
            "mx_misses": int(len(experts) - mx_hits),
            "prefetch_warmed": warmed,
            "prefetch_mode": self._prefetch_mode,
            "prefetch_kicked": bool(kicked),
            "residency": self._residency,
            "resident_locked": bool(
                getattr(self._stream, "resident_locked", lambda: False)()
            ),
            "keep_hot": self._keep_hot_armed,
            "on_demand_served": int(on_demand_served),
            "predict_online": self._online is not None,
            "predict_pgct1": self._pgct1 is not None,
        }
        if self._timing_on:
            stats["timing_ms"] = step_timing_ms(
                settle_ms=settle_ms,
                get_many_ms=get_many_ms,
                convert_ms=convert_ms,
                stack_ms=stack_ms,
                prefetch_kick_ms=kick_ms,
            )
        return bank, stats

    def cache_stats(self) -> dict:
        out = {
            "hits": self._stream.hits(),
            "misses": self._stream.misses(),
            "hot_hits": self._stream.hot_hits(),
            "warm_hits": self._stream.warm_hits(),
            "mx_hits": self._mx_hits,
            "mx_misses": self._mx_misses,
            "mx_size": len(self._mx),
            "mx_protected": len(self._mx_protected),
            "high_water_bytes": self._stream.high_water_bytes(),
            "profile": self.profile.name,
            "capacity": self.profile.capacity,
            "io_width": self._io_width,
            "profile_io_width": self.profile.io_width,
            "prefetch_mode": self._prefetch_mode,
            "prefetch_max": self._prefetch_max,
            "prefetch_kick_budget": self._prefetch_kick_budget,
            "residency": self._residency,
            "residency_applied": self._residency_applied,
            "resident_locked": bool(self._stream.resident_locked()),
            "residency_touched_bytes": int(self._residency_bytes),
            "keep_hot": self._keep_hot_armed,
            "sticky_after_keep_hot": sticky_after_keep_hot_enabled(),
            "parallel_convert_min": parallel_convert_min(),
            "decode_expert_gate": int(_DECODE_EXPERT_GATE),
            "prime": dict(getattr(self, "_prime_stats", {}) or {}),
            "hf_on_demand": self._ondemand is not None,  # legacy alias
            "l3_on_demand": self._ondemand is not None,
            "l3_mode": (
                getattr(self._ondemand, "mode", "hf")
                if self._ondemand is not None
                else "off"
            ),
            "hf_demo_serve": bool(self._ondemand_demo_serve),
            "predict_online": self._online is not None,
            "predict_pgct1": self._pgct1 is not None,
            "predict_kicks": int(self._predict_kicks),
        }
        if self._online is not None:
            out["online"] = dict(self._online.stats())
        if self._ondemand is not None:
            out["on_demand"] = dict(self._ondemand.stats)
        if self._timing_on and self._timing_steps:
            n = float(self._timing_steps)
            out["timing_avg_ms"] = {
                k: round(v / n, 3) for k, v in self._timing_totals.items()
            }
            out["timing_steps"] = self._timing_steps
        return out
