"""GGUF header + tensor-directory reader — the converter's front-end.

The converter (GGUF → .pgrn) needs to locate every expert weight tensor inside the
model file: its name, shape, quant type, and byte offset. This reads exactly that —
the GGUF magic/version, the metadata key/values, and the tensor directory — WITHOUT
reading the multi-GB tensor payload, so it runs against the real baseline model by
touching only its header. It then maps the Qwen-MoE expert tensors to the per-expert
layout the .pgrn container expects, confirming the model's real geometry.

GGUF spec: little-endian; magic 'GGUF', u32 version, u64 tensor_count, u64 kv_count,
then kv_count metadata entries, then tensor_count tensor infos (name, n_dims, dims,
ggml type, offset). Tensor data follows, aligned to `general.alignment` (default 32).
"""

from __future__ import annotations

import struct
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO

GGUF_MAGIC = b"GGUF"

# GGUF metadata value type enum
_U8, _I8, _U16, _I16, _U32, _I32, _F32, _BOOL, _STR, _ARR, _U64, _I64, _F64 = range(13)

_SCALAR = {
    _U8: ("<B", 1), _I8: ("<b", 1), _U16: ("<H", 2), _I16: ("<h", 2),
    _U32: ("<I", 4), _I32: ("<i", 4), _F32: ("<f", 4), _BOOL: ("<?", 1),
    _U64: ("<Q", 8), _I64: ("<q", 8), _F64: ("<d", 8),
}


class GgufError(Exception):
    """Raised for malformed GGUF input."""


@dataclass(frozen=True)
class TensorInfo:
    name: str
    dims: tuple[int, ...]
    ggml_type: int
    offset: int
    nbytes: int | None


# Exact upstream ggml block geometry for the source layouts admitted by the
# first native streaming converter increment.
_GGML_BLOCK = {
    0: (1, 4),      # F32
    1: (1, 2),      # F16
    12: (256, 144), # Q4_K
    13: (256, 176), # Q5_K
    14: (256, 210), # Q6_K
}


def ggml_nbytes(dims: tuple[int, ...], ggml_type: int) -> int:
    """Return exact contiguous GGML bytes for the supported source tensor."""
    if ggml_type not in _GGML_BLOCK:
        raise GgufError(f"unsupported GGML type {ggml_type}")
    elements = 1
    for dim in dims:
        if dim <= 0:
            raise GgufError(f"invalid tensor dimension {dim}")
        elements *= dim
    block, type_bytes = _GGML_BLOCK[ggml_type]
    if elements % block:
        raise GgufError(f"tensor element count {elements} is not divisible by block {block}")
    return elements // block * type_bytes


def _read(stream: BinaryIO, fmt: str) -> Any:
    size = struct.calcsize(fmt)
    data = stream.read(size)
    if len(data) != size:
        raise GgufError("unexpected end of file")
    return struct.unpack(fmt, data)[0]


def _read_string(stream: BinaryIO) -> str:
    n = _read(stream, "<Q")
    raw = stream.read(n)
    if len(raw) != n:
        raise GgufError("truncated string")
    return raw.decode("utf-8", errors="strict")


def _read_metadata_value(stream: BinaryIO, vtype: int) -> Any:
    if vtype in _SCALAR:
        fmt, _ = _SCALAR[vtype]
        return _read(stream, fmt)
    if vtype == _STR:
        return _read_string(stream)
    if vtype == _ARR:
        elem_type = _read(stream, "<I")
        length = _read(stream, "<Q")
        return [_read_metadata_value(stream, elem_type) for _ in range(length)]
    raise GgufError(f"unknown metadata value type {vtype}")


def read_gguf_header(stream: BinaryIO) -> dict[str, Any]:
    """Parse magic/version/counts, all metadata KVs, and the tensor directory."""
    magic = stream.read(4)
    if magic != GGUF_MAGIC:
        raise GgufError(f"bad magic {magic!r}, expected {GGUF_MAGIC!r}")
    version = _read(stream, "<I")
    if version not in (2, 3):
        raise GgufError(f"unsupported GGUF version {version}")
    tensor_count = _read(stream, "<Q")
    kv_count = _read(stream, "<Q")

    metadata: dict[str, Any] = {}
    for _ in range(kv_count):
        key = _read_string(stream)
        vtype = _read(stream, "<I")
        metadata[key] = _read_metadata_value(stream, vtype)

    tensors: list[TensorInfo] = []
    for _ in range(tensor_count):
        name = _read_string(stream)
        n_dims = _read(stream, "<I")
        dims = tuple(_read(stream, "<Q") for _ in range(n_dims))
        ggml_type = _read(stream, "<I")
        offset = _read(stream, "<Q")
        tensors.append(TensorInfo(
            name=name,
            dims=dims,
            ggml_type=ggml_type,
            offset=offset,
            nbytes=ggml_nbytes(dims, ggml_type) if ggml_type in _GGML_BLOCK else None,
        ))

    alignment = int(metadata.get("general.alignment", 32))
    if alignment <= 0 or alignment & (alignment - 1):
        raise GgufError(f"invalid GGUF alignment {alignment}")
    directory_end = stream.tell()
    data_offset = (directory_end + alignment - 1) // alignment * alignment
    return {
        "version": version,
        "metadata": metadata,
        "tensors": tensors,
        "data_offset": data_offset,
        "alignment": alignment,
    }


def find_expert_tensors(tensors: list[TensorInfo]) -> dict[str, list[TensorInfo]]:
    """Group the Qwen-MoE stacked expert tensors (ffn_{gate,up,down}_exps) by role."""
    roles: dict[str, list[TensorInfo]] = {"gate": [], "up": [], "down": []}
    for t in tensors:
        if "_exps" not in t.name:
            continue
        if "ffn_gate_exps" in t.name:
            roles["gate"].append(t)
        elif "ffn_up_exps" in t.name:
            roles["up"].append(t)
        elif "ffn_down_exps" in t.name:
            roles["down"].append(t)
    return roles


def summarize_expert_geometry(header: dict[str, Any]) -> dict[str, Any]:
    """Derive (layers, experts-per-layer) from metadata (authoritative) + expert tensors."""
    roles = find_expert_tensors(header["tensors"])
    n_layers = len(roles["gate"])
    metadata = header["metadata"]
    # Metadata is authoritative for the expert count; the stacked-tensor dim order is
    # ambiguous, so never guess it from dims. `<arch>.expert_count` holds the routed count.
    experts_per_layer = None
    for key, value in metadata.items():
        if key.endswith(".expert_count"):
            experts_per_layer = value
            break
    return {
        "layers_with_experts": n_layers,
        "experts_per_layer": experts_per_layer,
        "expert_tensor_dims": list(roles["gate"][0].dims) if roles["gate"] else None,
        "roles_present": {k: len(v) for k, v in roles.items()},
    }


def read_gguf_file(path: str | Path) -> dict[str, Any]:
    with open(path, "rb") as fh:
        return read_gguf_header(fh)


def main(argv: list[str] | None = None) -> int:
    import argparse
    import json

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    args = parser.parse_args(argv)
    header = read_gguf_file(args.path)
    geom = summarize_expert_geometry(header)
    print(json.dumps({
        "version": header["version"],
        "tensor_count": len(header["tensors"]),
        "metadata_keys": len(header["metadata"]),
        "expert_geometry": geom,
    }, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
