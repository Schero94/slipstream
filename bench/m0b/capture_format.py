"""Fail-closed loader for immutable M0b Q/K capture artifacts."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
from typing import Mapping
import zlib

import numpy as np

from .constants import CONTEXT_TOKENS, DECODE_STEPS, HEAD_DIM, KV_HEADS, LAYERS, Q_HEADS


HEX_40 = re.compile(r"[0-9a-f]{40}")
HEX_64 = re.compile(r"[0-9a-f]{64}")
ROLES = ("k_base", "q_steps", "k_steps")


class CaptureError(ValueError):
    """Raised when a capture is incomplete, unsafe, or inconsistent."""


@dataclass(frozen=True)
class LayerCapture:
    layer: int
    k_base: np.memmap
    q_steps: np.memmap
    k_steps: np.memmap


@dataclass(frozen=True)
class Capture:
    root: Path
    model_sha256: str
    llama_cpp_sha: str
    context_tokens: int
    prompt_tokens: int
    decode_steps: int
    layers: tuple[int, ...]
    positions: tuple[int, ...]
    q_heads: int
    kv_heads: int
    head_dim: int
    layer_captures: tuple[LayerCapture, ...]

    def layer(self, layer: int) -> LayerCapture:
        for capture in self.layer_captures:
            if capture.layer == layer:
                return capture
        raise KeyError(layer)


def _mapping(value: object, name: str) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise CaptureError(f"{name} must be an object")
    return value


def _integer(value: object, name: str, *, positive: bool = True) -> int:
    if type(value) is not int or (positive and value <= 0):
        raise CaptureError(f"{name} must be a positive integer")
    return value


def _hex(value: object, pattern: re.Pattern[str], name: str) -> str:
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise CaptureError(f"invalid {name}")
    return value


def _safe_file(root: Path, value: object) -> Path:
    if not isinstance(value, str) or not value:
        raise CaptureError("array path must be a non-empty string")
    relative = Path(value)
    if relative.is_absolute() or ".." in relative.parts or relative == Path("."):
        raise CaptureError("array path must stay below the capture root")
    unresolved = root / relative
    if unresolved.is_symlink():
        raise CaptureError("array path may not be a symlink")
    path = unresolved.resolve()
    if not path.is_relative_to(root) or not path.is_file():
        raise CaptureError("array path is missing or unsafe")
    return path


def _hashes(path: Path) -> tuple[int, str, str]:
    byte_count = 0
    crc = 0
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            byte_count += len(chunk)
            crc = zlib.crc32(chunk, crc)
            digest.update(chunk)
    return byte_count, f"{crc & 0xffffffff:08x}", digest.hexdigest()


def load_capture(
    root: Path,
    *,
    expected_model_sha256: str,
    allow_tiny: bool = False,
) -> Capture:
    """Validate every byte of a completed capture, then return read-only memmaps."""

    root = Path(root)
    if root.is_symlink() or not root.is_dir():
        raise CaptureError("capture root must be a regular directory")
    root = root.resolve()
    completion = root / "capture.complete.json"
    if completion.is_symlink() or not completion.is_file():
        raise CaptureError("atomic completion record is missing")
    try:
        document = json.loads(completion.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise CaptureError(f"invalid completion record: {error}") from error
    metadata = _mapping(document, "capture")
    if metadata.get("schema") != 1:
        raise CaptureError("unsupported capture schema")

    model_sha = _hex(metadata.get("model_sha256"), HEX_64, "model SHA")
    expected_sha = _hex(expected_model_sha256, HEX_64, "expected model SHA")
    if model_sha != expected_sha:
        raise CaptureError("model SHA does not match the admitted artifact")
    llama_sha = _hex(metadata.get("llama_cpp_sha"), HEX_40, "llama.cpp SHA")

    context_tokens = _integer(metadata.get("context_tokens"), "context_tokens")
    prompt_tokens = _integer(metadata.get("prompt_tokens"), "prompt_tokens")
    decode_steps = _integer(metadata.get("decode_steps"), "decode_steps")
    raw_layers = metadata.get("layers")
    raw_positions = metadata.get("positions")
    if not isinstance(raw_layers, list) or not all(type(item) is int for item in raw_layers):
        raise CaptureError("layers must be an integer list")
    if not isinstance(raw_positions, list) or not all(type(item) is int for item in raw_positions):
        raise CaptureError("positions must be an integer list")
    layers = tuple(raw_layers)
    positions = tuple(raw_positions)

    if allow_tiny:
        if context_tokens != prompt_tokens or layers != LAYERS:
            raise CaptureError("tiny capture geometry is inconsistent")
    else:
        if metadata.get("evidence") is not True:
            raise CaptureError("production capture must be marked as evidence")
        if context_tokens not in CONTEXT_TOKENS or prompt_tokens != context_tokens:
            raise CaptureError("invalid production context geometry")
        if decode_steps != DECODE_STEPS or layers != LAYERS:
            raise CaptureError("invalid production step or layer geometry")
    expected_positions = tuple(range(prompt_tokens, prompt_tokens + decode_steps))
    if len(positions) != decode_steps or positions != expected_positions:
        raise CaptureError("decode positions are missing, duplicated, or noncontiguous")

    geometry = _mapping(metadata.get("geometry"), "geometry")
    q_heads = _integer(geometry.get("q_heads"), "q_heads")
    kv_heads = _integer(geometry.get("kv_heads"), "kv_heads")
    head_dim = _integer(geometry.get("head_dim"), "head_dim")
    if q_heads % kv_heads != 0:
        raise CaptureError("invalid GQA head mapping")
    if not allow_tiny and (q_heads, kv_heads, head_dim) != (Q_HEADS, KV_HEADS, HEAD_DIM):
        raise CaptureError("production Qwen geometry does not match")

    raw_arrays = metadata.get("arrays")
    if not isinstance(raw_arrays, list):
        raise CaptureError("arrays must be a list")
    expected_shapes = {
        "k_base": (kv_heads, context_tokens, head_dim),
        "q_steps": (decode_steps, q_heads, head_dim),
        "k_steps": (decode_steps, kv_heads, head_dim),
    }
    expected_dtypes = {"k_base": np.dtype("<f2"), "q_steps": np.dtype("<f4"), "k_steps": np.dtype("<f2")}
    validated: dict[tuple[int, str], tuple[Path, np.dtype, tuple[int, ...]]] = {}
    for index, raw in enumerate(raw_arrays):
        item = _mapping(raw, f"array[{index}]")
        role = item.get("role")
        layer = item.get("layer")
        if role not in ROLES or type(layer) is not int or layer not in layers:
            raise CaptureError("array role or layer is invalid")
        key = (layer, role)
        if key in validated:
            raise CaptureError("duplicate layer array")
        raw_shape = item.get("shape")
        if not isinstance(raw_shape, list) or not all(type(value) is int and value > 0 for value in raw_shape):
            raise CaptureError("array shape is invalid")
        shape = tuple(raw_shape)
        if shape != expected_shapes[role]:
            raise CaptureError("array shape does not match capture geometry")
        try:
            dtype = np.dtype(item.get("dtype"))
        except (TypeError, ValueError) as error:
            raise CaptureError("array dtype is invalid") from error
        if dtype != expected_dtypes[role] or dtype.str != expected_dtypes[role].str:
            raise CaptureError("array dtype does not match its role")
        path = _safe_file(root, item.get("path"))
        declared_bytes = _integer(item.get("byte_count"), "byte_count")
        expected_bytes = int(np.prod(shape, dtype=np.int64)) * dtype.itemsize
        if declared_bytes != expected_bytes:
            raise CaptureError("declared byte count does not match shape")
        declared_crc = _hex(item.get("crc32"), re.compile(r"[0-9a-f]{8}"), "CRC32")
        declared_sha = _hex(item.get("sha256"), HEX_64, "array SHA")
        actual_bytes, actual_crc, actual_sha = _hashes(path)
        if (actual_bytes, actual_crc, actual_sha) != (declared_bytes, declared_crc, declared_sha):
            raise CaptureError("array byte count, CRC32, or SHA-256 mismatch")
        validated[key] = (path, dtype, shape)

    expected_keys = {(layer, role) for layer in layers for role in ROLES}
    if set(validated) != expected_keys:
        raise CaptureError("capture is missing a required layer array")

    layer_captures = []
    for layer in layers:
        arrays = {}
        for role in ROLES:
            path, dtype, shape = validated[(layer, role)]
            arrays[role] = np.memmap(path, dtype=dtype, mode="r", shape=shape, order="C")
        layer_captures.append(LayerCapture(layer=layer, **arrays))
    return Capture(
        root=root,
        model_sha256=model_sha,
        llama_cpp_sha=llama_sha,
        context_tokens=context_tokens,
        prompt_tokens=prompt_tokens,
        decode_steps=decode_steps,
        layers=layers,
        positions=positions,
        q_heads=q_heads,
        kv_heads=kv_heads,
        head_dim=head_dim,
        layer_captures=tuple(layer_captures),
    )
