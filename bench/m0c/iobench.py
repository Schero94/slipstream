"""M0c: SSD read-bandwidth microbenchmark for the FLB cost model.

Purpose: replace the assumed ~6 GB/s SSD figure in the Peregrine blueprint with
a measured number. Expert misses in the FLB "assembly line" are scattered
reads of expert-sized records, so this mirrors colibri's `iobench.c` method:

* a fresh large file (defeats warm-file assumptions),
* random, page-aligned offsets with a fixed seed (reproducible),
* expert-record-sized reads (~1.5 MB by default, 16 KB / 4 KB aligned),
* multiple threads (the syscall releases the GIL, so disk I/O overlaps),
* macOS `F_NOCACHE` so reads bypass the unified buffer cache and hit the device.

Only the runner touches disk; all geometry/aggregation is pure and unit-tested.
"""

from __future__ import annotations

import json
import mmap
import os
import random
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

PAGE = 4096
# macOS <sys/fcntl.h>: F_NOCACHE = 48. fcntl.F_NOCACHE is defined on Darwin.
try:  # pragma: no cover - platform constant
    import fcntl

    F_NOCACHE = getattr(fcntl, "F_NOCACHE", 48)
except ImportError:  # pragma: no cover
    fcntl = None  # type: ignore[assignment]
    F_NOCACHE = 48

DEFAULT_FILE_BYTES = 4 * 1024 * 1024 * 1024  # 4 GiB fresh file
DEFAULT_RECORD_BYTES = 1_572_864  # ~1.5 MB, = 384 * 4096 = 96 * 16384
DEFAULT_READS = 4096  # 4096 * 1.5 MB = ~6 GiB of reads over a 4 GiB file
DEFAULT_THREADS = 8
DEFAULT_SEED = 1234


class IoBenchError(Exception):
    """Raised for invalid configuration or missing benchmark inputs."""


@dataclass(frozen=True)
class IoBenchConfig:
    path: Path
    file_bytes: int = DEFAULT_FILE_BYTES
    record_bytes: int = DEFAULT_RECORD_BYTES
    reads: int = DEFAULT_READS
    threads: int = DEFAULT_THREADS
    seed: int = DEFAULT_SEED


def validate_config(config: IoBenchConfig) -> None:
    if config.record_bytes <= 0 or config.record_bytes % PAGE != 0:
        raise IoBenchError(f"record_bytes must be a positive multiple of {PAGE}")
    if config.file_bytes <= config.record_bytes:
        raise IoBenchError("file_bytes must be larger than record_bytes")
    if config.reads < 1:
        raise IoBenchError("reads must be >= 1")
    if config.threads < 1:
        raise IoBenchError("threads must be >= 1")


def generate_offsets(*, file_bytes: int, record_bytes: int, reads: int, seed: int) -> list[int]:
    """Deterministic, page-aligned random offsets in [0, file_bytes - record_bytes]."""
    span = file_bytes - record_bytes
    if span < 0:
        raise IoBenchError("file_bytes must be larger than record_bytes")
    rng = random.Random(seed)
    aligned_max = span - (span % PAGE)
    offsets = []
    for _ in range(reads):
        raw = rng.randint(0, aligned_max)
        offsets.append(raw - (raw % PAGE))
    return offsets


def partition_reads(*, reads: int, threads: int) -> list[list[int]]:
    """Split read indices [0, reads) into `threads` balanced disjoint groups."""
    buckets: list[list[int]] = [[] for _ in range(threads)]
    for index in range(reads):
        buckets[index % threads].append(index)
    return buckets


def throughput_gb_s(total_bytes: int, seconds: float) -> float:
    if seconds <= 0:
        raise IoBenchError("seconds must be > 0 to compute throughput")
    return total_bytes / 1e9 / seconds


def summarize(config: IoBenchConfig, *, total_bytes: int, seconds: float, use_nocache: bool) -> dict[str, Any]:
    return {
        "path": str(config.path),
        "file_bytes": config.file_bytes,
        "record_bytes": config.record_bytes,
        "reads": config.reads,
        "threads": config.threads,
        "seed": config.seed,
        "use_nocache": use_nocache,
        "total_bytes": total_bytes,
        "seconds": seconds,
        "gb_s": throughput_gb_s(total_bytes, seconds),
    }


def write_fresh_file(path: Path, file_bytes: int, *, chunk_bytes: int = 64 * 1024 * 1024) -> int:
    """Write a fresh file of `file_bytes` and fsync it. Returns bytes written."""
    path.parent.mkdir(parents=True, exist_ok=True)
    pattern = os.urandom(min(chunk_bytes, file_bytes) or 1)
    written = 0
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        while written < file_bytes:
            remaining = file_bytes - written
            block = pattern if remaining >= len(pattern) else pattern[:remaining]
            written += os.write(fd, block)
        os.fsync(fd)
    finally:
        os.close(fd)
    return written


def _read_slice(fd: int, offsets: list[int], indices: list[int], record_bytes: int, out: list[int], slot: int) -> None:
    # A page-aligned reusable buffer keeps F_NOCACHE reads on the fast path.
    buffer = mmap.mmap(-1, record_bytes)
    total = 0
    try:
        for index in indices:
            total += os.preadv(fd, [buffer], offsets[index])
    finally:
        buffer.close()
    out[slot] = total


def read_benchmark(config: IoBenchConfig, *, use_nocache: bool) -> dict[str, Any]:
    validate_config(config)
    if not config.path.is_file():
        raise IoBenchError(f"benchmark file does not exist: {config.path}")
    offsets = generate_offsets(
        file_bytes=config.file_bytes,
        record_bytes=config.record_bytes,
        reads=config.reads,
        seed=config.seed,
    )
    buckets = partition_reads(reads=config.reads, threads=config.threads)
    fd = os.open(config.path, os.O_RDONLY)
    try:
        if use_nocache and fcntl is not None:
            fcntl.fcntl(fd, F_NOCACHE, 1)
        totals = [0] * config.threads
        workers = [
            threading.Thread(
                target=_read_slice,
                args=(fd, offsets, buckets[slot], config.record_bytes, totals, slot),
            )
            for slot in range(config.threads)
        ]
        start = time.monotonic()
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join()
        seconds = time.monotonic() - start
    finally:
        os.close(fd)
    return summarize(config, total_bytes=sum(totals), seconds=seconds, use_nocache=use_nocache)


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--path", type=Path, default=Path("artifacts/m0c/iobench-scratch.dat"))
    parser.add_argument("--file-bytes", type=int, default=DEFAULT_FILE_BYTES)
    parser.add_argument("--record-bytes", type=int, default=DEFAULT_RECORD_BYTES)
    parser.add_argument("--reads", type=int, default=DEFAULT_READS)
    parser.add_argument("--threads", type=int, default=DEFAULT_THREADS)
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED)
    parser.add_argument("--also-cached", action="store_true", help="run a second F_NOCACHE-off comparison")
    parser.add_argument("--keep-file", action="store_true", help="do not delete the scratch file afterwards")
    parser.add_argument(
        "--use-existing",
        action="store_true",
        help="read an existing large file read-only (no write, no gate, no delete); "
        "derives file_bytes from its size. Use against a file larger than RAM to defeat caching.",
    )
    args = parser.parse_args(argv)

    if args.use_existing:
        if not args.path.is_file():
            print(json.dumps({"error": f"file does not exist: {args.path}"}))
            return 2
        config = IoBenchConfig(
            path=args.path,
            file_bytes=args.path.stat().st_size,
            record_bytes=args.record_bytes,
            reads=args.reads,
            threads=args.threads,
            seed=args.seed,
        )
        validate_config(config)
        results: dict[str, Any] = {"mode": "existing-file", "file_bytes": config.file_bytes}
        results["nocache"] = read_benchmark(config, use_nocache=True)
        if args.also_cached:
            results["cached"] = read_benchmark(config, use_nocache=False)
        print(json.dumps(results, indent=2))
        return 0

    config = IoBenchConfig(
        path=args.path,
        file_bytes=args.file_bytes,
        record_bytes=args.record_bytes,
        reads=args.reads,
        threads=args.threads,
        seed=args.seed,
    )
    validate_config(config)

    import shutil

    from scripts.disk_gate import RESERVE_FREE_BYTES, MAX_PROJECT_BYTES, evaluate_gate

    disk = shutil.disk_usage(config.path.parent if config.path.parent.exists() else Path.cwd())
    ok, reasons = evaluate_gate(
        project_bytes=0,
        free_bytes=disk.free,
        expected_bytes=config.file_bytes,
        max_project_bytes=MAX_PROJECT_BYTES,
        reserve_free_bytes=RESERVE_FREE_BYTES,
    )
    if not ok:
        print(json.dumps({"disk_gate": "FAIL", "reasons": reasons, "free_bytes": disk.free}))
        return 2

    results: dict[str, Any] = {"disk_gate": "PASS", "free_bytes_before": disk.free}
    try:
        write_fresh_file(config.path, config.file_bytes)
        results["nocache"] = read_benchmark(config, use_nocache=True)
        if args.also_cached:
            results["cached"] = read_benchmark(config, use_nocache=False)
    finally:
        if not args.keep_file and config.path.exists():
            config.path.unlink()
    print(json.dumps(results, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
