"""`.pgrn` container format (Blueprint Anhang A) — the on-disk engine artifact.

Defines how a model's experts live on disk so the FLB streaming runtime can
locate and load an individual cold expert by (layer, expert) without reading the
whole file. This is the concrete foundation under the tiered hot/warm/cold cache
we simulated: the expert directory gives each expert an offset, byte size,
precision tier (int8/int4/int2), a heat score, and a CRC for crash-safe
integrity. 16 KiB record alignment matches the zero-copy mmap→MTLBuffer plan.

Layout:
  [magic "PGRN1\\0\\0\\0"][version u32][json_len u32][json …]  → padded to 16 KiB
  [expert blob]…                                               each 16 KiB-aligned
  [expert directory: fixed records]                            at expert_dir_offset

Pure Python (struct/json/zlib); tested with synthetic experts, no model needed.
"""

from __future__ import annotations

import json
import os
import struct
import zlib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO, Iterable

MAGIC = b"PGRN1\x00\x00\x00"
VERSION = 1
RECORD_ALIGN = 16384  # 16 KiB
VALID_PRECISION = {0, 1, 2, 3}  # 0=int8, 1=int4, 2=int2, 3=ternary(reserved)

# layer u16, expert u16, precision u8, flags u8, heat f32, offset u64, nbytes u32, crc u32
_DIR_STRUCT = struct.Struct("<HHBBfQII")


class PgrnError(Exception):
    """Raised for malformed containers or invalid write inputs."""


@dataclass(frozen=True)
class ExpertRef:
    layer: int
    expert: int
    precision: int
    flags: int
    heat: float
    offset: int
    nbytes: int
    crc: int


def _align_up(value: int) -> int:
    return (value + RECORD_ALIGN - 1) // RECORD_ALIGN * RECORD_ALIGN


class PgrnWriter:
    """Atomic, one-expert-at-a-time PGRN writer with bounded payload memory."""

    def __init__(self, path: str | Path, *, metadata: dict[str, Any], expected_count: int):
        self.path = Path(path)
        self.partial = Path(str(self.path) + ".partial")
        if self.path.exists():
            raise FileExistsError(self.path)
        if self.partial.exists():
            raise FileExistsError(f"partial output already exists: {self.partial}")
        if expected_count <= 0:
            raise PgrnError("expected_count must be positive")
        self.metadata = metadata
        self.expected_count = expected_count
        self.refs: list[ExpertRef] = []
        self.seen: set[tuple[int, int]] = set()
        self.stream = self.partial.open("x+b")
        self.stream.write(b"\x00" * RECORD_ALIGN)
        self.finished = False

    def append_expert(
        self, layer: int, expert: int, precision: int, data: bytes, heat: float = 0.0
    ) -> ExpertRef:
        if self.finished:
            raise PgrnError("writer is already finished")
        if precision not in VALID_PRECISION:
            raise PgrnError(f"invalid precision {precision} for ({layer},{expert})")
        if not data:
            raise PgrnError(f"empty expert ({layer},{expert})")
        key = (layer, expert)
        if key in self.seen:
            raise PgrnError(f"duplicate expert {key}")
        if self.stream.tell() % RECORD_ALIGN:
            raise PgrnError("internal expert alignment drift")
        ref = ExpertRef(
            layer, expert, precision, 0, float(heat), self.stream.tell(),
            len(data), zlib.crc32(data) & 0xFFFFFFFF,
        )
        self.stream.write(data)
        self.stream.write(b"\x00" * (_align_up(self.stream.tell()) - self.stream.tell()))
        self.refs.append(ref)
        self.seen.add(key)
        return ref

    def finish(self) -> dict[str, Any]:
        if self.finished:
            raise PgrnError("writer is already finished")
        if len(self.refs) != self.expected_count:
            raise PgrnError(f"expected {self.expected_count} experts, got {len(self.refs)}")
        expert_dir_offset = self.stream.tell()
        for ref in self.refs:
            self.stream.write(_DIR_STRUCT.pack(
                ref.layer, ref.expert, ref.precision, ref.flags, ref.heat,
                ref.offset, ref.nbytes, ref.crc,
            ))
        header = {
            "metadata": self.metadata,
            "expert_count": len(self.refs),
            "expert_dir_offset": expert_dir_offset,
        }
        encoded = json.dumps(header, separators=(",", ":")).encode("utf-8")
        if len(MAGIC) + 8 + len(encoded) > RECORD_ALIGN:
            raise PgrnError("header metadata exceeds the 16 KiB header block")
        self.stream.seek(0)
        self.stream.write(MAGIC)
        self.stream.write(struct.pack("<II", VERSION, len(encoded)))
        self.stream.write(encoded)
        self.stream.write(b"\x00" * (RECORD_ALIGN - self.stream.tell()))
        self.stream.flush()
        os.fsync(self.stream.fileno())
        self.stream.close()
        self.finished = True
        with self.partial.open("rb") as verify:
            checked = read_header(verify)
            if len(read_directory(verify, checked)) != self.expected_count:
                raise PgrnError("completed directory verification failed")
        os.replace(self.partial, self.path)
        return header

    def abort(self) -> None:
        if not self.stream.closed:
            self.stream.close()
        if not self.finished:
            self.partial.unlink(missing_ok=True)

    def __enter__(self) -> "PgrnWriter":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        if exc_type is not None or not self.finished:
            self.abort()


def write_container(
    stream: BinaryIO,
    *,
    metadata: dict[str, Any],
    experts: Iterable[tuple[int, int, int, bytes, float]],
) -> dict[str, Any]:
    """Write experts as [(layer, expert, precision, data_bytes, heat), …]."""
    experts = list(experts)
    seen: set[tuple[int, int]] = set()
    refs: list[ExpertRef] = []
    cursor = RECORD_ALIGN  # first blob starts after the reserved header block
    for layer, expert, precision, data, heat in experts:
        if precision not in VALID_PRECISION:
            raise PgrnError(f"invalid precision {precision} for expert ({layer},{expert})")
        key = (layer, expert)
        if key in seen:
            raise PgrnError(f"duplicate expert {key}")
        seen.add(key)
        refs.append(
            ExpertRef(layer, expert, precision, 0, float(heat), cursor, len(data), zlib.crc32(data) & 0xFFFFFFFF)
        )
        cursor = _align_up(cursor + len(data))
    expert_dir_offset = cursor

    header = {"metadata": metadata, "expert_count": len(refs), "expert_dir_offset": expert_dir_offset}
    header_json = json.dumps(header, separators=(",", ":")).encode("utf-8")
    if len(MAGIC) + 8 + len(header_json) > RECORD_ALIGN:
        raise PgrnError("header metadata exceeds the 16 KiB header block")

    stream.write(MAGIC)
    stream.write(struct.pack("<II", VERSION, len(header_json)))
    stream.write(header_json)
    stream.write(b"\x00" * (RECORD_ALIGN - stream.tell()))

    for (layer, expert, precision, data, heat), ref in zip(experts, refs):
        if stream.tell() != ref.offset:
            raise PgrnError("internal layout drift")  # pragma: no cover
        stream.write(data)
        pad = _align_up(stream.tell()) - stream.tell()
        stream.write(b"\x00" * pad)

    if stream.tell() != expert_dir_offset:
        raise PgrnError("internal directory offset drift")  # pragma: no cover
    for ref in refs:
        stream.write(
            _DIR_STRUCT.pack(
                ref.layer, ref.expert, ref.precision, ref.flags, ref.heat, ref.offset, ref.nbytes, ref.crc
            )
        )
    return header


def read_header(stream: BinaryIO) -> dict[str, Any]:
    stream.seek(0)
    magic = stream.read(8)
    if magic != MAGIC:
        raise PgrnError("not a PGRN container (bad magic)")
    version, json_len = struct.unpack("<II", stream.read(8))
    try:
        header = json.loads(stream.read(json_len).decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PgrnError(f"malformed header json: {error}") from error
    header["magic"] = magic.decode("ascii", "replace")
    header["version"] = version
    for field in ("expert_count", "expert_dir_offset"):
        if field not in header:
            raise PgrnError(f"header missing {field}")
    return header


def read_directory(stream: BinaryIO, header: dict[str, Any]) -> list[ExpertRef]:
    stream.seek(header["expert_dir_offset"])
    refs = []
    for _ in range(header["expert_count"]):
        chunk = stream.read(_DIR_STRUCT.size)
        if len(chunk) != _DIR_STRUCT.size:
            raise PgrnError("truncated expert directory")
        layer, expert, precision, flags, heat, offset, nbytes, crc = _DIR_STRUCT.unpack(chunk)
        refs.append(ExpertRef(layer, expert, precision, flags, heat, offset, nbytes, crc))
    return refs


def read_expert(stream: BinaryIO, header: dict[str, Any], layer: int, expert: int) -> bytes:
    """Load one expert's bytes by (layer, expert) — the streaming fetch primitive."""
    for ref in read_directory(stream, header):
        if ref.layer == layer and ref.expert == expert:
            stream.seek(ref.offset)
            data = stream.read(ref.nbytes)
            if len(data) != ref.nbytes:
                raise PgrnError(f"truncated expert blob for ({layer},{expert})")
            if zlib.crc32(data) & 0xFFFFFFFF != ref.crc:
                raise PgrnError(f"CRC mismatch for expert ({layer},{expert}) — corrupt blob")
            return data
    raise PgrnError(f"expert ({layer},{expert}) not found in container")
