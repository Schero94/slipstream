"""Incrementally extract stacked GGUF MoE experts into a no-mmap PGRN store."""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
from pathlib import Path
from typing import Any

from bench.m1.gguf_reader import TensorInfo, find_expert_tensors, read_gguf_file
from bench.m1.pgrn_container import PgrnWriter, RECORD_ALIGN

# Exact contiguous layouts supported by gguf_reader. F32/F16 make the native
# parity fixture architecture-agnostic; production Q4_K/Q5_K/Q6_K remain
# byte-preserving and are never transcoded.
SUPPORTED_EXPERT_TYPES = {0, 1, 12, 13, 14}
_LAYER = re.compile(r"^blk\.(\d+)\.ffn_(gate|up|down)_exps\.weight$")
_ROLES = ("gate", "up", "down")
DEFAULT_MIN_FREE_BYTES = 16 * 1024**3


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb", buffering=1024 * 1024) as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _layout(path: Path, model_sha256: str | None) -> tuple[dict[str, Any], dict[int, dict[str, TensorInfo]]]:
    header = read_gguf_file(path)
    actual_sha = _sha256(path)
    if model_sha256 is not None and actual_sha.lower() != model_sha256.lower():
        raise ValueError(f"source SHA-256 mismatch: expected {model_sha256}, got {actual_sha}")
    roles = find_expert_tensors(header["tensors"])
    by_layer: dict[int, dict[str, TensorInfo]] = {}
    for role, tensors in roles.items():
        for tensor in tensors:
            match = _LAYER.match(tensor.name)
            # Per-expert scale/input_scale sidecars remain small resident tensors;
            # only authoritative stacked weight payloads belong in PGRN.
            if not match:
                continue
            if match.group(2) != role:
                raise ValueError(f"expert role mismatch in {tensor.name}")
            if tensor.ggml_type not in SUPPORTED_EXPERT_TYPES or tensor.nbytes is None:
                raise ValueError(f"unsupported expert GGML type {tensor.ggml_type} in {tensor.name}")
            by_layer.setdefault(int(match.group(1)), {})[role] = tensor
    if not by_layer:
        raise ValueError("GGUF contains no supported stacked expert tensors")
    expert_count = next(
        (int(value) for key, value in header["metadata"].items() if key.endswith(".expert_count")),
        0,
    )
    if expert_count <= 0:
        raise ValueError("GGUF metadata has no authoritative expert_count")
    for layer, layer_roles in by_layer.items():
        if set(layer_roles) != set(_ROLES):
            raise ValueError(f"layer {layer} does not have gate/up/down expert tensors")
        for tensor in layer_roles.values():
            if not tensor.dims or tensor.dims[-1] != expert_count or tensor.nbytes % expert_count:
                raise ValueError(f"tensor {tensor.name} is not evenly stacked by expert")
    header["source_sha256"] = actual_sha
    return header, by_layer


def _plan_from_layout(
    source: Path, header: dict[str, Any], by_layer: dict[int, dict[str, TensorInfo]]
) -> dict[str, Any]:
    expert_count = next(int(v) for k, v in header["metadata"].items() if k.endswith(".expert_count"))
    record_bytes: dict[int, int] = {}
    types: dict[str, dict[str, int]] = {}
    record_layout: dict[str, dict[str, dict[str, int]]] = {}
    for layer, layer_roles in sorted(by_layer.items()):
        cursor = 0
        record_layout[str(layer)] = {}
        types[str(layer)] = {}
        for role in _ROLES:
            tensor = layer_roles[role]
            size = tensor.nbytes // expert_count
            record_layout[str(layer)][role] = {
                "offset": cursor, "nbytes": size, "ggml_type": tensor.ggml_type,
            }
            types[str(layer)][role] = tensor.ggml_type
            cursor += size
        record_bytes[layer] = cursor
    padded_payload = sum(
        ((size + RECORD_ALIGN - 1) // RECORD_ALIGN * RECORD_ALIGN) * expert_count
        for size in record_bytes.values()
    )
    return {
        "source": str(source),
        "source_size": source.stat().st_size,
        "source_sha256": header["source_sha256"],
        "data_offset": header["data_offset"],
        "layers_with_experts": len(by_layer),
        "experts_per_layer": expert_count,
        "expert_count": len(by_layer) * expert_count,
        "max_expert_bytes": max(record_bytes.values()),
        "estimated_output_bytes": RECORD_ALIGN + padded_payload + len(by_layer) * expert_count * 26,
        "ggml_types_by_layer": types,
        "record_layout_by_layer": record_layout,
        "record_bytes_by_layer": {str(k): v for k, v in record_bytes.items()},
    }


def inspect_conversion(path: str | Path, *, model_sha256: str | None = None) -> dict[str, Any]:
    source = Path(path)
    header, by_layer = _layout(source, model_sha256)
    return _plan_from_layout(source, header, by_layer)


def _pread_exact(fd: int, size: int, offset: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < size:
        chunk = os.pread(fd, size - len(chunks), offset + len(chunks))
        if not chunk:
            raise OSError(f"short GGUF read at {offset}: {len(chunks)}/{size} bytes")
        chunks.extend(chunk)
    return bytes(chunks)


def convert(
    path: str | Path,
    output: str | Path,
    *,
    model_sha256: str | None = None,
    min_free_bytes: int = DEFAULT_MIN_FREE_BYTES,
) -> dict[str, Any]:
    source, destination = Path(path), Path(output)
    if destination.exists():
        raise FileExistsError(destination)
    header, by_layer = _layout(source, model_sha256)
    plan = _plan_from_layout(source, header, by_layer)
    if min_free_bytes < 0:
        raise ValueError("min_free_bytes must not be negative")
    free_bytes = shutil.disk_usage(destination.parent).free
    required_bytes = plan["estimated_output_bytes"] + min_free_bytes
    if free_bytes < required_bytes:
        raise OSError(
            "PGRN disk admission refused: "
            f"free={free_bytes}, output={plan['estimated_output_bytes']}, reserve={min_free_bytes}"
        )
    metadata = {
        "model_sha256": header["source_sha256"],
        "source_size": source.stat().st_size,
        "source_format": "GGUF",
        "geometry": {
            "layers_with_experts": plan["layers_with_experts"],
            "experts_per_layer": plan["experts_per_layer"],
        },
        "record_layout_by_layer": plan["record_layout_by_layer"],
        # Compact, canonical native manifest:
        # [layer, gate_type, gate_bytes, up_type, up_bytes, down_type, down_bytes].
        "tensor_directory": [
            [
                layer,
                plan["record_layout_by_layer"][str(layer)]["gate"]["ggml_type"],
                plan["record_layout_by_layer"][str(layer)]["gate"]["nbytes"],
                plan["record_layout_by_layer"][str(layer)]["up"]["ggml_type"],
                plan["record_layout_by_layer"][str(layer)]["up"]["nbytes"],
                plan["record_layout_by_layer"][str(layer)]["down"]["ggml_type"],
                plan["record_layout_by_layer"][str(layer)]["down"]["nbytes"],
            ]
            for layer in sorted(by_layer)
        ],
    }
    fd = os.open(source, os.O_RDONLY)
    written = 0
    try:
        with PgrnWriter(destination, metadata=metadata, expected_count=plan["expert_count"]) as writer:
            for layer, layer_roles in sorted(by_layer.items()):
                for expert in range(plan["experts_per_layer"]):
                    parts = []
                    for role in _ROLES:
                        tensor = layer_roles[role]
                        expert_bytes = tensor.nbytes // plan["experts_per_layer"]
                        absolute = header["data_offset"] + tensor.offset + expert * expert_bytes
                        parts.append(_pread_exact(fd, expert_bytes, absolute))
                    writer.append_expert(layer, expert, 1, b"".join(parts))
                    written += 1
            writer.finish()
    finally:
        os.close(fd)
    return {**plan, "output": str(destination), "experts_written": written}


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--model-sha256")
    parser.add_argument("--min-free-gb", type=float, default=16.0,
                        help="minimum free disk space retained after conversion (default: 16 GiB)")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    if args.dry_run:
        print(json.dumps(inspect_conversion(args.input, model_sha256=args.model_sha256), indent=2))
        return 0
    if args.output is None:
        parser.error("--output is required unless --dry-run is used")
    if args.min_free_gb < 0:
        parser.error("--min-free-gb must not be negative")
    print(json.dumps(convert(
        args.input, args.output, model_sha256=args.model_sha256,
        min_free_bytes=int(args.min_free_gb * 1024**3),
    ), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
