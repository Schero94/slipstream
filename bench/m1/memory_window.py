"""Capture and diff explicit macOS paging windows for H1 qualification."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Mapping

from bench.m0a.smoke_server import _write_json_atomic
from bench.m1.headroom import _command_output, parse_memory_pressure, parse_vm_stat


class MemoryWindowError(RuntimeError):
    pass


def capture_snapshot() -> dict[str, object]:
    return {
        "schema": 1,
        "vm": parse_vm_stat(_command_output(["vm_stat"])),
        "free_percent": parse_memory_pressure(_command_output(["memory_pressure"])),
    }


def _vm(snapshot: Mapping[str, object]) -> Mapping[str, object]:
    vm = snapshot.get("vm")
    free = snapshot.get("free_percent")
    if not isinstance(vm, Mapping) or isinstance(free, bool) or not isinstance(free, int):
        raise MemoryWindowError("memory snapshot is malformed")
    return vm


def build_window(
    before: Mapping[str, object], after: Mapping[str, object]
) -> dict[str, object]:
    before_vm = _vm(before)
    after_vm = _vm(after)
    try:
        page_size = int(before_vm["page_size_bytes"])
        after_page_size = int(after_vm["page_size_bytes"])
        deltas = {
            f"{name}_delta": int(after_vm[name]) - int(before_vm[name])
            for name in ("pageins", "pageouts", "swapins", "swapouts")
        }
    except (KeyError, TypeError, ValueError) as error:
        raise MemoryWindowError("memory snapshot counters are malformed") from error
    if page_size <= 0 or after_page_size != page_size:
        raise MemoryWindowError("memory snapshot page size changed")
    if any(value < 0 for value in deltas.values()):
        raise MemoryWindowError("memory snapshot counter moved backwards")
    return {
        "schema": 1,
        "page_size_bytes": page_size,
        **deltas,
        "pageout_bytes_delta": deltas["pageouts_delta"] * page_size,
        "free_percent_before": int(before["free_percent"]),
        "free_percent_after": int(after["free_percent"]),
    }


def _read(path: Path) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise MemoryWindowError("snapshot must be a regular non-symlink file")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise MemoryWindowError(f"cannot read memory snapshot: {error}") from error
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise MemoryWindowError("memory snapshot schema is invalid")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    snapshot = subparsers.add_parser("snapshot")
    snapshot.add_argument("--output", required=True, type=Path)
    diff = subparsers.add_parser("diff")
    diff.add_argument("--before", required=True, type=Path)
    diff.add_argument("--after", required=True, type=Path)
    diff.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.command == "snapshot":
            result = capture_snapshot()
        else:
            result = build_window(_read(args.before), _read(args.after))
        _write_json_atomic(args.output, result)
    except (MemoryWindowError, OSError, ValueError) as error:
        print(f"memory window failed: {error}")
        return 2
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
