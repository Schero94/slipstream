"""M0d streaming backing store — the one missing link: a REAL cold SSD read on miss.

Everything else in the streaming chain already exists and is tested (plan_load,
tiered_cache_sim, coupling_predictor, stream_projection). What was always simulated
is the miss itself. This module makes a cache miss cost a real `preadv` of one
expert-sized record from the model file on the SSD, with macOS `F_NOCACHE` set so the
OS buffer cache cannot fake the hit (same method as `bench/m0c/iobench.py`).

Scope, honest: the per-expert byte offset is a deterministic scatter over the file
(CRC32 of (layer,expert)), not the exact GGUF tensor slice. That is intentional and
sufficient — M0d measures the memory-access PATTERN and the real device cost of an
expert-sized scattered cold read, not tensor extraction. `INT4_EXPERT_BYTES` is
16384-aligned (108 * 16384), so every read is page-aligned like the .pgrn plan.
"""

from __future__ import annotations

import fcntl
import os
import time
import zlib
from typing import Any

from bench.m0a.constants import INT4_EXPERT_BYTES

# macOS <sys/fcntl.h>: F_NOCACHE = 48 (reads bypass the unified buffer cache).
F_NOCACHE = getattr(fcntl, "F_NOCACHE", 48)


class StoreError(Exception):
    """Raised for invalid streaming-store inputs."""


def expert_offset(layer: int, expert: int, *, n_slots: int, record_bytes: int) -> int:
    """Deterministic 16384-aligned offset for one expert's scattered cold read."""
    if n_slots < 1:
        raise StoreError("file too small for one record")
    slot = zlib.crc32(f"{layer}:{expert}".encode()) % n_slots
    return slot * record_bytes


class StreamingStore:
    """Real SSD backing store: preadv one expert-sized record per miss, F_NOCACHE on."""

    def __init__(self, path: str, *, record_bytes: int = INT4_EXPERT_BYTES, nocache: bool = True):
        if record_bytes < 1 or record_bytes % 16384 != 0:
            raise StoreError("record_bytes must be a positive multiple of 16384")
        self.path = path
        self.record_bytes = record_bytes
        self._fd = os.open(path, os.O_RDONLY)
        self.file_size = os.fstat(self._fd).st_size
        self.n_slots = self.file_size // record_bytes
        if self.n_slots < 1:
            os.close(self._fd)
            raise StoreError(f"file {path} smaller than one record ({record_bytes})")
        self.nocache = bool(nocache)
        if self.nocache:
            # 1 = enable F_NOCACHE; returns 0 on success
            fcntl.fcntl(self._fd, F_NOCACHE, 1)
        self.reads = 0
        self.bytes_read = 0
        self.seconds = 0.0

    def read_expert(self, layer: int, expert: int) -> bytes:
        """Real cold read of one expert-sized record. Called on a cache miss."""
        offset = expert_offset(layer, expert, n_slots=self.n_slots, record_bytes=self.record_bytes)
        start = time.monotonic()
        data = os.pread(self._fd, self.record_bytes, offset)
        self.seconds += time.monotonic() - start
        self.reads += 1
        self.bytes_read += len(data)
        return data

    def stats(self) -> dict[str, Any]:
        gb_s = (self.bytes_read / 1e9 / self.seconds) if self.seconds > 0 else 0.0
        return {
            "reads": self.reads,
            "bytes_read": self.bytes_read,
            "seconds": round(self.seconds, 6),
            "gb_s": round(gb_s, 4),
            "record_bytes": self.record_bytes,
            "nocache": self.nocache,
        }

    def close(self) -> None:
        if self._fd is not None:
            os.close(self._fd)
            self._fd = None

    def __enter__(self) -> "StreamingStore":
        return self

    def __exit__(self, *exc) -> None:
        self.close()
