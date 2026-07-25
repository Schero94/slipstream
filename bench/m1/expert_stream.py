"""Expert streaming loader — the FLB data-movement layer.

Sits between the `.pgrn` container (on-disk experts) and the compute path: it
resolves an expert by (layer, expert) through the cached directory, serves it
from an LRU resident cache on a hit, or loads it from the container on a miss
(CRC-checked), evicting the least-recently-used expert when the cache is full.
`prefetch` warms experts ahead of demand (the coupling predictor feeds it).

This is the real mechanism the tiered-cache simulator modeled — locate → load →
cache → evict — now operating on the real container format. The Metal compute
kernels will sit on top of the bytes this returns. Pure Python, tested on a
synthetic container; no model, no Metal, no disk gate.
"""

from __future__ import annotations

import zlib
from collections import OrderedDict
from typing import Any, BinaryIO

from bench.m1.pgrn_container import ExpertRef, PgrnError, read_directory


class ExpertStreamer:
    def __init__(self, stream: BinaryIO, header: dict[str, Any], *, capacity: int) -> None:
        if capacity < 1:
            raise PgrnError("capacity must be >= 1")
        self._stream = stream
        self._index: dict[tuple[int, int], ExpertRef] = {
            (r.layer, r.expert): r for r in read_directory(stream, header)
        }
        self._capacity = capacity
        self._cache: "OrderedDict[tuple[int, int], bytes]" = OrderedDict()
        self.hits = 0
        self.misses = 0
        self.bytes_streamed = 0
        self.prefetch_bytes = 0

    def _load(self, key: tuple[int, int]) -> bytes:
        ref = self._index[key]
        self._stream.seek(ref.offset)
        data = self._stream.read(ref.nbytes)
        if len(data) != ref.nbytes:
            raise PgrnError(f"truncated expert blob for {key}")
        if zlib.crc32(data) & 0xFFFFFFFF != ref.crc:
            raise PgrnError(f"CRC mismatch for expert {key} — corrupt blob")
        return data

    def _insert(self, key: tuple[int, int], data: bytes) -> None:
        self._cache[key] = data
        self._cache.move_to_end(key)
        while len(self._cache) > self._capacity:
            self._cache.popitem(last=False)

    def get_expert(self, layer: int, expert: int) -> bytes:
        key = (layer, expert)
        if key not in self._index:
            raise PgrnError(f"expert {key} not in container")
        if key in self._cache:
            self._cache.move_to_end(key)
            self.hits += 1
            return self._cache[key]
        data = self._load(key)
        self.misses += 1
        self.bytes_streamed += self._index[key].nbytes
        self._insert(key, data)
        return data

    def prefetch(self, keys) -> None:
        for key in keys:
            key = tuple(key)
            if key in self._index and key not in self._cache:
                data = self._load(key)
                self.prefetch_bytes += self._index[key].nbytes
                self._insert(key, data)

    def stats(self) -> dict[str, Any]:
        total = self.hits + self.misses
        return {
            "hits": self.hits,
            "misses": self.misses,
            "hit_rate": (self.hits / total) if total else 0.0,
            "bytes_streamed": self.bytes_streamed,
            "prefetch_bytes": self.prefetch_bytes,
            "resident": len(self._cache),
            "capacity": self._capacity,
        }
