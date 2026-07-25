"""Versioned, checksummed wire format for Peregrine M0a routing traces."""

from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path
import struct
from typing import BinaryIO, Iterable, Iterator
from uuid import UUID
import zlib


MAGIC = b"PGRRT01\x00"
VERSION = 1
ENDIAN_MARKER = 0x01020304
PHASE_PROMPT = 0
PHASE_DECODE = 1
UNUSED_EXPERT = 0xFFFF

HEADER_STRUCT = struct.Struct("<8sIIIIHHHH16sQ32sI8s")
RECORD_PREFIX = struct.Struct("<QIiHBB10H")
RECORD_STRUCT = struct.Struct("<QIiHBB10HI")


class RoutingFormatError(ValueError):
    """Raised when routing bytes violate the v1 compatibility contract."""


def _crc32(data: bytes) -> int:
    return zlib.crc32(data) & 0xFFFFFFFF


@dataclass(frozen=True)
class RoutingHeader:
    layer_count: int
    expert_count: int
    top_k: int
    flags: int
    session_id: UUID
    start_time_ns: int
    model_sha256: bytes

    def validate(self) -> None:
        if not 1 <= self.layer_count <= 256:
            raise RoutingFormatError("layer_count must fit the u8 record field")
        if not 1 <= self.expert_count < UNUSED_EXPERT:
            raise RoutingFormatError("expert_count is outside the v1 range")
        if not 1 <= self.top_k <= 10 or self.top_k > self.expert_count:
            raise RoutingFormatError("top_k must be within 1..10 and expert_count")
        if not 0 <= self.flags <= 0xFFFF:
            raise RoutingFormatError("flags do not fit u16")
        if not 0 <= self.start_time_ns <= 0xFFFFFFFFFFFFFFFF:
            raise RoutingFormatError("start_time_ns does not fit u64")
        if not isinstance(self.session_id, UUID):
            raise RoutingFormatError("session_id must be a UUID")
        if len(self.model_sha256) != 32:
            raise RoutingFormatError("model_sha256 must contain 32 raw bytes")

    def to_bytes(self) -> bytes:
        self.validate()
        provisional = HEADER_STRUCT.pack(
            MAGIC,
            VERSION,
            HEADER_STRUCT.size,
            RECORD_STRUCT.size,
            ENDIAN_MARKER,
            self.layer_count,
            self.expert_count,
            self.top_k,
            self.flags,
            self.session_id.bytes,
            self.start_time_ns,
            self.model_sha256,
            0,
            b"\x00" * 8,
        )
        crc = _crc32(provisional[:88])
        return provisional[:88] + struct.pack("<I", crc) + provisional[92:]


@dataclass(frozen=True)
class RoutingRecord:
    session_id: UUID
    batch_id: int
    token_pos: int
    token_id: int
    sequence_id: int
    phase: int
    layer: int
    experts: tuple[int, ...]

    def validate(self, header: RoutingHeader) -> None:
        if self.session_id != header.session_id:
            raise RoutingFormatError("record session_id differs from header")
        if not 0 <= self.batch_id <= 0xFFFFFFFFFFFFFFFF:
            raise RoutingFormatError("batch_id does not fit u64")
        if not 0 <= self.token_pos <= 0xFFFFFFFF:
            raise RoutingFormatError("token_pos does not fit u32")
        if not -(2**31) <= self.token_id < 2**31:
            raise RoutingFormatError("token_id does not fit i32")
        if not 0 <= self.sequence_id <= 0xFFFF:
            raise RoutingFormatError("sequence_id does not fit u16")
        if self.phase not in (PHASE_PROMPT, PHASE_DECODE):
            raise RoutingFormatError("record phase is invalid")
        if not 0 <= self.layer < header.layer_count:
            raise RoutingFormatError("record layer is outside header geometry")
        if len(self.experts) != header.top_k:
            raise RoutingFormatError("record expert count differs from header top_k")
        if len(set(self.experts)) != len(self.experts):
            raise RoutingFormatError("record contains duplicate selected experts")
        if any(expert < 0 or expert >= header.expert_count for expert in self.experts):
            raise RoutingFormatError("record contains an out-of-range expert")

    def to_bytes(self, header: RoutingHeader) -> bytes:
        self.validate(header)
        wire_experts = self.experts + (UNUSED_EXPERT,) * (10 - header.top_k)
        prefix = RECORD_PREFIX.pack(
            self.batch_id,
            self.token_pos,
            self.token_id,
            self.sequence_id,
            self.phase,
            self.layer,
            *wire_experts,
        )
        return prefix + struct.pack("<I", _crc32(prefix))


def read_header(stream: BinaryIO) -> RoutingHeader:
    data = stream.read(HEADER_STRUCT.size)
    if len(data) != HEADER_STRUCT.size:
        raise RoutingFormatError("routing header is truncated")

    (
        magic,
        version,
        header_bytes,
        record_bytes,
        endian_marker,
        layer_count,
        expert_count,
        top_k,
        flags,
        session_bytes,
        start_time_ns,
        model_sha256,
        stored_crc,
        reserved,
    ) = HEADER_STRUCT.unpack(data)

    if magic != MAGIC:
        raise RoutingFormatError("routing header magic is invalid")
    if version != VERSION:
        raise RoutingFormatError("routing format version is unsupported")
    if header_bytes != HEADER_STRUCT.size or record_bytes != RECORD_STRUCT.size:
        raise RoutingFormatError("routing wire sizes are incompatible")
    if endian_marker != ENDIAN_MARKER:
        raise RoutingFormatError("routing endian marker is invalid")
    if stored_crc != _crc32(data[:88]):
        raise RoutingFormatError("routing header CRC does not match")
    if reserved != b"\x00" * 8:
        raise RoutingFormatError("routing header reserved bytes are nonzero")

    header = RoutingHeader(
        layer_count=layer_count,
        expert_count=expert_count,
        top_k=top_k,
        flags=flags,
        session_id=UUID(bytes=session_bytes),
        start_time_ns=start_time_ns,
        model_sha256=model_sha256,
    )
    header.validate()
    return header


def _record_from_bytes(data: bytes, header: RoutingHeader) -> RoutingRecord:
    if len(data) != RECORD_STRUCT.size:
        raise RoutingFormatError("routing file has a trailing partial record")
    values = RECORD_STRUCT.unpack(data)
    if values[-1] != _crc32(data[: RECORD_PREFIX.size]):
        raise RoutingFormatError("routing record CRC does not match")

    wire_experts = values[6:16]
    active = tuple(wire_experts[: header.top_k])
    inactive = wire_experts[header.top_k :]
    if any(expert != UNUSED_EXPERT for expert in inactive):
        raise RoutingFormatError("unused routing expert slots are not 0xFFFF")

    record = RoutingRecord(
        session_id=header.session_id,
        batch_id=values[0],
        token_pos=values[1],
        token_id=values[2],
        sequence_id=values[3],
        phase=values[4],
        layer=values[5],
        experts=active,
    )
    record.validate(header)
    return record


def iter_records(path: Path) -> Iterator[RoutingRecord]:
    with path.open("rb") as stream:
        header = read_header(stream)
        while True:
            data = stream.read(RECORD_STRUCT.size)
            if not data:
                return
            yield _record_from_bytes(data, header)


def write_fixture(
    path: Path,
    header: RoutingHeader,
    records: Iterable[RoutingRecord],
) -> None:
    header_bytes = header.to_bytes()
    with path.open("wb") as stream:
        stream.write(header_bytes)
        for record in records:
            stream.write(record.to_bytes(header))
        stream.flush()
        os.fsync(stream.fileno())
