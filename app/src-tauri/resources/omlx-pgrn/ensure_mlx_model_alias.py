#!/usr/bin/env python3
"""Ensure oMLX model_settings.json exposes an API alias (default: slipstream).

Agents often hardcode ``model=slipstream`` (Metal ``--alias``). MLX/oMLX uses the
directory id unless ``model_alias`` is set in ``<base-path>/model_settings.json``.

Usage:
  ensure_mlx_model_alias.py --base-path DIR --model-id Qwen3.6-35B-A3B-4bit
  ensure_mlx_model_alias.py --base-path DIR --model-dir ~/Modelle/mlx --alias slipstream

Env:
  SLIPSTREAM_OMLX_MODEL_ALIAS  alias to write (default slipstream).
                               Set to 0/off/false/empty to skip.
No server start. Safe to call before ``run_omlx_pgrn.sh serve``.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

SETTINGS_VERSION = 1


_ALIAS_OFF = frozenset({"0", "off", "false", "none", "-", "disabled", "no"})


def resolve_alias(raw: str | None) -> str | None:
    """Return alias string, or None when disabled via env/flag."""
    if raw is None:
        raw = os.environ.get("SLIPSTREAM_OMLX_MODEL_ALIAS", "slipstream")
    v = str(raw).strip()
    if not v or v.lower() in _ALIAS_OFF:
        return None
    return v


def normalize_model_id(model_id: str) -> str:
    """Strip and reject empty / path-like model ids."""
    mid = str(model_id or "").strip().rstrip("/")
    if not mid:
        raise ValueError("model_id must be non-empty")
    # Directory basename only — reject absolute / relative paths with separators
    if "/" in mid or "\\" in mid:
        mid = Path(mid).name
    if not mid:
        raise ValueError("model_id must be non-empty")
    return mid


def model_id_from_dir(model_dir: Path, explicit: str | None) -> str:
    if explicit:
        return normalize_model_id(explicit)
    preferred = model_dir / "Qwen3.6-35B-A3B-4bit"
    if preferred.is_dir():
        return preferred.name
    if model_dir.is_dir():
        for p in sorted(model_dir.iterdir()):
            if p.is_dir() and (p / "experts.pgrn").is_file():
                return p.name
    raise SystemExit(f"could not resolve model id under {model_dir}")


def load_models_map(path: Path) -> tuple[int, dict]:
    """Parse model_settings.json → (version, models dict). Tolerates flat layout."""
    models: dict = {}
    version = SETTINGS_VERSION
    if not path.is_file():
        return version, models
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return version, models
    if not isinstance(data, dict):
        return version, models
    version = int(data.get("version") or SETTINGS_VERSION)
    raw_models = data.get("models")
    if isinstance(raw_models, dict):
        return version, dict(raw_models)
    # Accidental flat layout → treat top-level dict values as models
    models = {
        k: v
        for k, v in data.items()
        if k not in ("version", "models") and isinstance(v, dict)
    }
    return version, models


def ensure_alias(base_path: Path, model_id: str, alias: str) -> dict:
    """Write versioned oMLX model_settings.json with model_alias set."""
    mid = normalize_model_id(model_id)
    alias_s = str(alias).strip()
    if not alias_s:
        raise ValueError("alias must be non-empty")
    base_path = Path(base_path).expanduser()
    base_path.mkdir(parents=True, exist_ok=True)
    path = base_path / "model_settings.json"
    version, models = load_models_map(path)

    entry = models.get(mid)
    if not isinstance(entry, dict):
        entry = {}
    else:
        entry = dict(entry)
    prev = entry.get("model_alias")
    entry["model_alias"] = alias_s
    models[mid] = entry

    out = {"version": version, "models": models}
    path.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {
        "path": str(path),
        "model_id": mid,
        "alias": alias_s,
        "previous": prev,
        "changed": prev != alias_s,
    }


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--base-path", required=True, type=Path)
    ap.add_argument("--model-id", default="")
    ap.add_argument("--model-dir", type=Path, default=None)
    ap.add_argument("--alias", default=None, help="Override SLIPSTREAM_OMLX_MODEL_ALIAS")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args(argv)

    alias = resolve_alias(args.alias)
    if alias is None:
        print("skip: alias disabled")
        return 0

    try:
        mid = args.model_id.strip()
        if not mid:
            if args.model_dir is None:
                print("need --model-id or --model-dir", file=sys.stderr)
                return 2
            mid = model_id_from_dir(args.model_dir.expanduser(), None)
        else:
            mid = normalize_model_id(mid)
    except ValueError as e:
        print(str(e), file=sys.stderr)
        return 2

    if args.dry_run:
        print(
            json.dumps(
                {
                    "base_path": str(args.base_path.expanduser()),
                    "model_id": mid,
                    "alias": alias,
                }
            )
        )
        return 0

    try:
        info = ensure_alias(args.base_path.expanduser(), mid, alias)
    except ValueError as e:
        print(str(e), file=sys.stderr)
        return 2
    print(json.dumps(info))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
