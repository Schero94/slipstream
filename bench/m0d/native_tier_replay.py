"""Replay validated 35B routing accesses through the native PGRN tier policy."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
from typing import Iterable, Sequence

from bench.m0a.routing_format import PHASE_DECODE, RoutingFormatError, iter_records, read_header


class NativeTierReplayError(ValueError):
    """Raised when trace evidence or native replay output is incomplete."""


PAIR = struct.Struct("<HH")
DEFAULT_TRACE = Path("bench/artifacts/m0d-live-observe/live-observe-routing.bin")
DEFAULT_DRIVER = Path("vendor/llama.cpp/build/bin/test-peregrine-tier")
DEFAULT_EXPERT_BYTES = 1_769_472


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_decode_pairs(trace_paths: Sequence[Path], output: Path) -> dict[str, object]:
    if not trace_paths:
        raise NativeTierReplayError("at least one routing trace is required")
    identity: tuple[bytes, int, int, int] | None = None
    accesses = 0
    inputs: list[dict[str, object]] = []
    with output.open("wb") as sink:
        for path in trace_paths:
            try:
                with path.open("rb") as source:
                    header = read_header(source)
                current = (header.model_sha256, header.layer_count, header.expert_count, header.top_k)
                if identity is None:
                    identity = current
                elif current != identity:
                    raise NativeTierReplayError("routing traces have mixed model identity or geometry")
                before = accesses
                for record in iter_records(path):
                    if record.phase != PHASE_DECODE:
                        continue
                    for expert in record.experts:
                        sink.write(PAIR.pack(record.layer, expert))
                        accesses += 1
                inputs.append({
                    "path": str(path),
                    "sha256": _sha256(path),
                    "decode_accesses": accesses - before,
                })
            except (OSError, RoutingFormatError) as error:
                raise NativeTierReplayError(f"invalid routing trace {path}: {error}") from error
        sink.flush()
        os.fsync(sink.fileno())
    if identity is None or accesses == 0:
        raise NativeTierReplayError("routing traces contain no decode expert accesses")
    return {
        "model_sha256": identity[0].hex(),
        "layer_count": identity[1],
        "expert_count": identity[2],
        "top_k": identity[3],
        "accesses": accesses,
        "inputs": inputs,
    }


def run_native(
    driver: Path,
    pairs: Path,
    *,
    layer_count: int,
    total_capacity: int,
    hot_percent: int,
) -> dict[str, object]:
    command = [
        str(driver), "--replay", str(pairs), str(layer_count),
        str(total_capacity), str(hot_percent),
    ]
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        raise NativeTierReplayError(
            f"native tier replay failed ({completed.returncode}): {completed.stderr.strip()}"
        )
    try:
        result = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise NativeTierReplayError("native tier replay returned invalid JSON") from error
    required = {
        "accesses", "hot_hits", "warm_hits", "misses", "promotions",
        "demotions", "hot_slots", "warm_slots",
    }
    if not isinstance(result, dict) or required - result.keys():
        raise NativeTierReplayError("native tier replay output is incomplete")
    accesses = int(result["accesses"])
    hits = int(result["hot_hits"]) + int(result["warm_hits"])
    if accesses <= 0 or hits + int(result["misses"]) != accesses:
        raise NativeTierReplayError("native tier replay counters are inconsistent")
    result.update({
        "hot_percent": hot_percent,
        "hits": hits,
        "hit_rate": round(hits / accesses, 8),
        "total_capacity": total_capacity,
    })
    return result


def select_candidate(rows: Sequence[dict[str, object]]) -> dict[str, object]:
    baseline = next((row for row in rows if int(row["hot_percent"]) == 0), None)
    if baseline is None:
        raise NativeTierReplayError("candidate rows lack the HOT=0 baseline")
    baseline_hits = int(baseline["hits"])
    eligible = [
        row for row in rows
        if int(row["hot_percent"]) > 0 and int(row["hits"]) >= baseline_hits
    ]
    if not eligible:
        return baseline
    return max(eligible, key=lambda row: (int(row["hits"]), -int(row["hot_percent"])))


def qualify(
    trace_paths: Sequence[Path],
    *,
    driver: Path,
    cache_bytes: int,
    expert_bytes: int = DEFAULT_EXPERT_BYTES,
    candidates: Iterable[int] = (0, 10, 20, 25, 33),
) -> dict[str, object]:
    if cache_bytes <= 0 or expert_bytes <= 0:
        raise NativeTierReplayError("cache and expert bytes must be positive")
    if not driver.is_file():
        raise NativeTierReplayError(f"native tier driver is absent: {driver}")
    with tempfile.TemporaryDirectory(prefix="pgr-tier-") as directory:
        pairs = Path(directory) / "decode-pairs.bin"
        evidence = write_decode_pairs(trace_paths, pairs)
        total_capacity = cache_bytes // expert_bytes
        if total_capacity < int(evidence["layer_count"]):
            raise NativeTierReplayError("cache cannot provide one slot per expert layer")
        rows = [
            run_native(
                driver, pairs, layer_count=int(evidence["layer_count"]),
                total_capacity=total_capacity, hot_percent=int(percent),
            )
            for percent in candidates
        ]
    if any(int(row["accesses"]) != int(evidence["accesses"]) for row in rows):
        raise NativeTierReplayError("native driver did not consume every validated access")
    selected = select_candidate(rows)
    baseline = next(row for row in rows if int(row["hot_percent"]) == 0)
    return {
        "schema": "peregrine-native-tier-replay-v1",
        **evidence,
        "cache_bytes": cache_bytes,
        "expert_bytes": expert_bytes,
        "total_capacity": total_capacity,
        "rows": rows,
        "selected_hot_percent": int(selected["hot_percent"]),
        "selected_hits_delta": int(selected["hits"]) - int(baseline["hits"]),
        "decision": "NONREGRESSING_HOT" if int(selected["hot_percent"]) else "KEEP_PURE_CLOX",
    }


def _write_atomic(path: Path, payload: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trace", action="append", type=Path, default=[])
    parser.add_argument("--driver", type=Path, default=DEFAULT_DRIVER)
    parser.add_argument("--cache-gib", type=float, default=2.0)
    parser.add_argument("--expert-bytes", type=int, default=DEFAULT_EXPERT_BYTES)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    traces = args.trace or [DEFAULT_TRACE]
    try:
        report = qualify(
            traces, driver=args.driver,
            cache_bytes=int(args.cache_gib * 1024**3), expert_bytes=args.expert_bytes,
        )
    except NativeTierReplayError as error:
        print(json.dumps({"error": str(error)}))
        return 2
    _write_atomic(args.output, report)
    print(json.dumps({
        "decision": report["decision"],
        "selected_hot_percent": report["selected_hot_percent"],
        "selected_hits_delta": report["selected_hits_delta"],
        "output": str(args.output),
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
