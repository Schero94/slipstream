"""ctypes bindings for libpgrn_host.dylib (portable PGRN C core)."""
from __future__ import annotations

import ctypes
from pathlib import Path
from typing import List, Optional, Sequence, Tuple


def _default_lib() -> Path:
    return Path(__file__).resolve().parent / "native" / "build" / "libpgrn_host.dylib"


def pack_key(layer: int, expert: int) -> int:
    return (int(layer) << 16) | int(expert)


def unpack_key(key: int) -> Tuple[int, int]:
    return (int(key) >> 16) & 0xFFFF, int(key) & 0xFFFF


class Ref(ctypes.Structure):
    _fields_ = [
        ("layer", ctypes.c_uint16),
        ("expert", ctypes.c_uint16),
        ("precision", ctypes.c_uint8),
        ("flags", ctypes.c_uint8),
        ("heat", ctypes.c_float),
        ("offset", ctypes.c_uint64),
        ("nbytes", ctypes.c_uint32),
        ("crc32", ctypes.c_uint32),
    ]


class StreamParams(ctypes.Structure):
    """Mirrors pgr_stream_params in peregrine_stream.h (natural C alignment)."""

    _fields_ = [
        ("slot_bytes", ctypes.c_size_t),
        ("capacity", ctypes.c_int),
        ("clox_k", ctypes.c_int),
        ("hot_capacity", ctypes.c_int),
        ("promote_hits", ctypes.c_uint8),
        ("demote_idle_epochs", ctypes.c_uint64),
        ("cooldown_epochs", ctypes.c_uint64),
        ("io_width", ctypes.c_int),
    ]


LOADER = ctypes.CFUNCTYPE(
    ctypes.c_int,
    ctypes.c_void_p,
    ctypes.c_long,
    ctypes.c_void_p,
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_size_t),
    ctypes.c_char_p,
    ctypes.c_size_t,
)


def _bind_common(lib: ctypes.CDLL) -> None:
    if getattr(lib, "_pgrn_bound", False):
        return
    lib.pgrn_open.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
    lib.pgrn_open.restype = ctypes.c_void_p
    lib.pgrn_close.argtypes = [ctypes.c_void_p]
    lib.pgrn_close.restype = None
    lib.pgrn_count.argtypes = [ctypes.c_void_p]
    lib.pgrn_count.restype = ctypes.c_size_t
    lib.pgrn_experts_per_layer.argtypes = [ctypes.c_void_p]
    lib.pgrn_experts_per_layer.restype = ctypes.c_uint32
    lib.pgrn_max_expert_bytes.argtypes = [ctypes.c_void_p]
    lib.pgrn_max_expert_bytes.restype = ctypes.c_size_t
    lib.pgrn_error.argtypes = [ctypes.c_void_p]
    lib.pgrn_error.restype = ctypes.c_char_p
    lib.pgrn_find.argtypes = [
        ctypes.c_void_p,
        ctypes.c_uint16,
        ctypes.c_uint16,
        ctypes.c_void_p,
    ]
    lib.pgrn_find.restype = ctypes.c_int
    lib.pgrn_read_expert_mt.argtypes = [
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.c_char_p,
        ctypes.c_size_t,
    ]
    lib.pgrn_read_expert_mt.restype = ctypes.c_int

    lib.pgr_stream_new_loader.argtypes = [
        ctypes.c_size_t,
        ctypes.c_int,
        ctypes.c_int,
        LOADER,
        ctypes.c_void_p,
    ]
    lib.pgr_stream_new_loader.restype = ctypes.c_void_p
    lib.pgr_stream_new_loader_tier.argtypes = [
        ctypes.POINTER(StreamParams),
        LOADER,
        ctypes.c_void_p,
    ]
    lib.pgr_stream_new_loader_tier.restype = ctypes.c_void_p
    if hasattr(lib, "pgr_stream_hot_hits"):
        lib.pgr_stream_hot_hits.argtypes = [ctypes.c_void_p]
        lib.pgr_stream_hot_hits.restype = ctypes.c_long
    if hasattr(lib, "pgr_stream_warm_hits"):
        lib.pgr_stream_warm_hits.argtypes = [ctypes.c_void_p]
        lib.pgr_stream_warm_hits.restype = ctypes.c_long
    lib.pgr_stream_batch_begin.argtypes = [ctypes.c_void_p]
    lib.pgr_stream_batch_begin.restype = None
    lib.pgr_stream_get_many.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_long),
        ctypes.POINTER(ctypes.c_float),
        ctypes.c_int,
        ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(ctypes.c_size_t),
        ctypes.POINTER(ctypes.c_int),
    ]
    lib.pgr_stream_get_many.restype = ctypes.c_int
    if hasattr(lib, "pgr_stream_prefetch_many"):
        lib.pgr_stream_prefetch_many.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_long),
            ctypes.POINTER(ctypes.c_float),
            ctypes.c_int,
        ]
        lib.pgr_stream_prefetch_many.restype = ctypes.c_int
    lib.pgr_stream_hits.argtypes = [ctypes.c_void_p]
    lib.pgr_stream_hits.restype = ctypes.c_long
    lib.pgr_stream_misses.argtypes = [ctypes.c_void_p]
    lib.pgr_stream_misses.restype = ctypes.c_long
    lib.pgr_stream_high_water_bytes.argtypes = [ctypes.c_void_p]
    lib.pgr_stream_high_water_bytes.restype = ctypes.c_size_t
    lib.pgr_stream_error.argtypes = [ctypes.c_void_p]
    lib.pgr_stream_error.restype = ctypes.c_char_p
    if hasattr(lib, "pgr_stream_lock_resident"):
        lib.pgr_stream_lock_resident.argtypes = [ctypes.c_void_p]
        lib.pgr_stream_lock_resident.restype = ctypes.c_int
    if hasattr(lib, "pgr_stream_touch_resident"):
        lib.pgr_stream_touch_resident.argtypes = [ctypes.c_void_p]
        lib.pgr_stream_touch_resident.restype = ctypes.c_size_t
    if hasattr(lib, "pgr_stream_resident_locked"):
        lib.pgr_stream_resident_locked.argtypes = [ctypes.c_void_p]
        lib.pgr_stream_resident_locked.restype = ctypes.c_int
    lib.pgr_stream_free.argtypes = [ctypes.c_void_p]
    lib.pgr_stream_free.restype = None
    lib._pgrn_bound = True


class PgrnFile:
    def __init__(self, lib: ctypes.CDLL, handle: ctypes.c_void_p):
        self._lib = lib
        self._h = handle

    @classmethod
    def open(cls, path: Path | str, lib_path: Optional[Path] = None) -> "PgrnFile":
        lib = ctypes.CDLL(str(lib_path or _default_lib()))
        _bind_common(lib)
        handle = lib.pgrn_open(str(path).encode(), None)
        if not handle:
            raise OSError(f"pgrn_open failed for {path}")
        return cls(lib, ctypes.c_void_p(handle))

    def close(self) -> None:
        if self._h:
            self._lib.pgrn_close(self._h)
            self._h = None

    def count(self) -> int:
        return int(self._lib.pgrn_count(self._h))

    def experts_per_layer(self) -> int:
        return int(self._lib.pgrn_experts_per_layer(self._h))

    def max_expert_bytes(self) -> int:
        return int(self._lib.pgrn_max_expert_bytes(self._h))

    def read_expert(self, layer: int, expert: int) -> bytes:
        ref = Ref()
        # pgrn_find returns 1 on hit, 0 on miss (not an errno-style code).
        if self._lib.pgrn_find(self._h, layer, expert, ctypes.byref(ref)) != 1:
            raise KeyError(f"missing {layer}/{expert}")
        buf = ctypes.create_string_buffer(ref.nbytes)
        err = ctypes.create_string_buffer(256)
        rc = self._lib.pgrn_read_expert_mt(
            self._h, ctypes.byref(ref), buf, ref.nbytes, err, 256
        )
        if rc != 0:
            raise OSError(err.value.decode() or f"read failed rc={rc}")
        return buf.raw

    def open_stream(
        self,
        *,
        capacity: int = 32,
        io_width: int = 1,
        hot_capacity: int = 0,
        clox_k: int = 2,
    ) -> "PgrnStream":
        return PgrnStream(
            self,
            capacity=capacity,
            io_width=io_width,
            hot_capacity=hot_capacity,
            clox_k=clox_k,
        )


class PgrnStream:
    """Fixed-slot expert cache backed by an open PgrnFile."""

    def __init__(
        self,
        file: PgrnFile,
        *,
        capacity: int = 32,
        io_width: int = 1,
        hot_capacity: int = 0,
        clox_k: int = 2,
    ):
        self._file = file
        self._lib = file._lib
        slot = file.max_expert_bytes()
        self.capacity = int(capacity)
        self.io_width = max(1, int(io_width))
        self.hot_capacity = max(0, min(int(hot_capacity), self.capacity))

        @LOADER
        def loader(user, key, dst, dst_cap, loaded, error, error_cap):
            layer, expert = unpack_key(int(key))
            ref = Ref()
            if self._lib.pgrn_find(self._file._h, layer, expert, ctypes.byref(ref)) != 1:
                if error and error_cap:
                    msg = f"missing {layer}/{expert}".encode()
                    ctypes.memmove(error, msg, min(len(msg), error_cap - 1))
                return 1  # PGR_STREAM_INVALID
            if ref.nbytes > dst_cap:
                return 4  # PGR_STREAM_OVERFLOW
            err = ctypes.create_string_buffer(256)
            rc = self._lib.pgrn_read_expert_mt(
                self._file._h, ctypes.byref(ref), dst, ref.nbytes, err, 256
            )
            if rc != 0:
                if error and error_cap and err.value:
                    ctypes.memmove(error, err, min(len(err.value), error_cap - 1))
                return 2  # PGR_STREAM_IO
            loaded[0] = ref.nbytes
            return 0

        # Keep callback alive for the stream lifetime.
        self._loader = loader
        params = StreamParams(
            slot_bytes=slot,
            capacity=self.capacity,
            clox_k=int(clox_k),
            hot_capacity=self.hot_capacity,
            promote_hits=3,
            demote_idle_epochs=64,
            cooldown_epochs=16,
            io_width=self.io_width,
        )
        handle = self._lib.pgr_stream_new_loader_tier(
            ctypes.byref(params), loader, None
        )
        if not handle:
            raise OSError("pgr_stream_new_loader_tier failed")
        self._h = ctypes.c_void_p(handle)

    def close(self) -> None:
        if self._h:
            self._lib.pgr_stream_free(self._h)
            self._h = None

    def batch_begin(self) -> None:
        self._lib.pgr_stream_batch_begin(self._h)

    def get_many(
        self, layer: int, experts: Sequence[int]
    ) -> Tuple[List[memoryview], List[int]]:
        """Fetch distinct experts for one layer. Returns (pinned views, hit flags)."""
        if not experts:
            return [], []
        # Deduplicate preserving order — get_many requires distinct keys.
        seen = set()
        uniq: List[int] = []
        for e in experts:
            if e in seen:
                continue
            seen.add(e)
            uniq.append(int(e))
        n = len(uniq)
        keys = (ctypes.c_long * n)(*[pack_key(layer, e) for e in uniq])
        data = (ctypes.c_void_p * n)()
        sizes = (ctypes.c_size_t * n)()
        hits = (ctypes.c_int * n)()
        self.batch_begin()
        rc = self._lib.pgr_stream_get_many(
            self._h, keys, None, n, data, sizes, hits
        )
        if rc != 0:
            err = self._lib.pgr_stream_error(self._h)
            raise OSError(err.decode() if err else f"get_many rc={rc}")
        views: List[memoryview] = []
        hit_flags: List[int] = []
        for i in range(n):
            buf = (ctypes.c_char * sizes[i]).from_address(data[i])
            views.append(memoryview(buf))
            hit_flags.append(int(hits[i]))
        return views, hit_flags

    def prefetch_many(self, layer: int, experts: Sequence[int]) -> int:
        """Speculative COLD warm; no pins, never errors on mispredict. Returns warmed count."""
        fn = getattr(self._lib, "pgr_stream_prefetch_many", None)
        if fn is None or not experts:
            return 0
        seen = set()
        uniq: List[int] = []
        for e in experts:
            if e in seen:
                continue
            seen.add(e)
            uniq.append(int(e))
        n = len(uniq)
        if n <= 0:
            return 0
        keys = (ctypes.c_long * n)(*[pack_key(layer, e) for e in uniq])
        warmed = int(fn(self._h, keys, None, n))
        return max(0, warmed)

    def hits(self) -> int:
        return int(self._lib.pgr_stream_hits(self._h))

    def misses(self) -> int:
        return int(self._lib.pgr_stream_misses(self._h))

    def high_water_bytes(self) -> int:
        return int(self._lib.pgr_stream_high_water_bytes(self._h))

    def hot_hits(self) -> int:
        fn = getattr(self._lib, "pgr_stream_hot_hits", None)
        return int(fn(self._h)) if fn else 0

    def warm_hits(self) -> int:
        fn = getattr(self._lib, "pgr_stream_warm_hits", None)
        return int(fn(self._h)) if fn else 0

    def touch_resident(self) -> int:
        """Page-fault occupied slot bytes. Returns bytes touched (0 if unsupported)."""
        fn = getattr(self._lib, "pgr_stream_touch_resident", None)
        if fn is None or self._h is None:
            return 0
        return int(fn(self._h))

    def lock_resident(self) -> bool:
        """Best-effort mlock of occupied owned slots. False if unsupported/failed."""
        fn = getattr(self._lib, "pgr_stream_lock_resident", None)
        if fn is None or self._h is None:
            return False
        return int(fn(self._h)) == 0

    def resident_locked(self) -> bool:
        fn = getattr(self._lib, "pgr_stream_resident_locked", None)
        if fn is None or self._h is None:
            return False
        return int(fn(self._h)) == 1
