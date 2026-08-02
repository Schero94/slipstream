#!/usr/bin/env python3
"""Resolve a free-space-safe oMLX prefix-cache budget for Slipstream."""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import shutil
import sys
from typing import Any


GIB = 1024**3
UPSTREAM_AUTO_FRACTION = 0.10


def calculate_budget_bytes(
    *,
    total_bytes: int,
    free_bytes: int,
    existing_cache_bytes: int,
    reserve_bytes: int,
) -> int:
    """Return total cache bytes without spending the filesystem reserve."""
    if min(total_bytes, free_bytes, existing_cache_bytes, reserve_bytes) < 0:
        raise ValueError("cache budget inputs must be non-negative")
    if free_bytes <= reserve_bytes:
        return 0
    upstream_auto = int(total_bytes * UPSTREAM_AUTO_FRACTION)
    safely_maintainable = existing_cache_bytes + free_bytes - reserve_bytes
    return max(0, min(upstream_auto, safely_maintainable))


def _settings(base_path: Path) -> dict[str, Any]:
    path = base_path / "settings.json"
    if not path.is_file():
        return {}
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError("settings.json root must be an object")
    return value


def _existing_ancestor(path: Path) -> Path:
    candidate = path
    while not candidate.exists():
        parent = candidate.parent
        if parent == candidate:
            raise ValueError(f"no existing ancestor for cache path: {path}")
        candidate = parent
    return candidate


def _tree_size_bytes(path: Path) -> int:
    if not path.exists():
        return 0
    if path.is_file():
        return path.stat().st_size
    total = 0
    for root, directories, files in os.walk(path, followlinks=False):
        directories[:] = [name for name in directories if not (Path(root) / name).is_symlink()]
        for name in files:
            candidate = Path(root) / name
            try:
                total += candidate.stat(follow_symlinks=False).st_size
            except OSError:
                continue
    return total


def resolve_action(base_path: Path, cache_dir: Path | None, reserve_gib: float) -> str:
    """Return ``preserve``, ``disable``, or an exact byte cap."""
    if not math.isfinite(reserve_gib) or reserve_gib < 0:
        raise ValueError("reserve GiB must be a finite non-negative number")

    settings = _settings(base_path)
    cache = settings.get("cache") or {}
    if not isinstance(cache, dict):
        raise ValueError("settings.json cache must be an object")
    if cache.get("enabled", True) is False or cache.get("hot_cache_only", False) is True:
        return "preserve"
    configured = cache.get("ssd_cache_max_size", "auto")
    if not isinstance(configured, str) or configured.strip().lower() != "auto":
        return "preserve"

    configured_dir = cache.get("ssd_cache_dir")
    resolved_cache = cache_dir
    if resolved_cache is None and isinstance(configured_dir, str) and configured_dir.strip():
        resolved_cache = Path(configured_dir).expanduser()
    if resolved_cache is None:
        resolved_cache = base_path / "cache"
    resolved_cache = resolved_cache.expanduser()

    usage = shutil.disk_usage(_existing_ancestor(resolved_cache))
    cache_bytes = _tree_size_bytes(resolved_cache)
    cap = calculate_budget_bytes(
        total_bytes=usage.total,
        free_bytes=usage.free,
        existing_cache_bytes=cache_bytes,
        reserve_bytes=int(reserve_gib * GIB),
    )
    return str(cap) if cap > 0 else "disable"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-path", type=Path, required=True)
    parser.add_argument("--cache-dir", type=Path)
    parser.add_argument("--reserve-gib", type=float, default=3.0)
    args = parser.parse_args()
    try:
        print(resolve_action(args.base_path.expanduser(), args.cache_dir, args.reserve_gib))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"oMLX SSD cache budget failed: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
