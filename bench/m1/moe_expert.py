"""MoE expert compute — CPU reference kernel (the compute on streamed expert bytes).

`expert_stream` locates and delivers an expert's quantized bytes; THIS is what the
runtime computes with them: dequantize the int4 weights and run the SwiGLU FFN
(gate/up/down) that a routed expert applies to a token. The eventual Metal kernel
sits exactly here — dequant into an MTLBuffer, then GEMV — and is validated against
this reference.

Scope, honest: this is a generic symmetric int4 GROUP quantization (one scale per
group of weights), the clean reference form. The exact GGUF Q4_K block layout with
its super-block scales is a different packing handled by the converter; it is not
re-implemented here. The FFN math and the streamed-bytes→compute path are the point.
"""

from __future__ import annotations

import numpy as np


class MoeError(Exception):
    """Raised for invalid MoE-expert inputs."""


def quant_int4_groups(x: np.ndarray, *, group_size: int) -> tuple[np.ndarray, np.ndarray]:
    """Symmetric int4 group quantization. Returns (codes int8 in [-8,7], per-group scale)."""
    x = np.asarray(x, dtype=np.float32).reshape(-1)
    if group_size < 1:
        raise MoeError("group_size must be >= 1")
    if x.size % group_size != 0:
        raise MoeError(f"group_size {group_size} must divide length {x.size}")
    groups = x.reshape(-1, group_size)
    amax = np.max(np.abs(groups), axis=1)
    scales = np.where(amax == 0.0, 1.0, amax / 7.0)          # 7 = max positive int4 level
    codes = np.rint(groups / scales[:, None]).clip(-8, 7).astype(np.int8)
    return codes, scales.astype(np.float32)


def dequant_int4_groups(codes: np.ndarray, scales: np.ndarray, *, group_size: int) -> np.ndarray:
    """Inverse of quant_int4_groups. Returns a flat float32 array."""
    codes = np.asarray(codes, dtype=np.float32).reshape(-1, group_size)
    if codes.shape[0] != scales.shape[0]:
        raise MoeError("scales count does not match number of groups")
    return (codes * np.asarray(scales, dtype=np.float32)[:, None]).reshape(-1)


def swiglu_expert(x: np.ndarray, w_gate: np.ndarray, w_up: np.ndarray, w_down: np.ndarray) -> np.ndarray:
    """SwiGLU expert FFN for one token: (silu(x@w_gate) * (x@w_up)) @ w_down.

    x is (d,); w_gate,w_up are (d, inter); w_down is (inter, d). Returns (d,).
    """
    x = np.asarray(x, dtype=np.float64)
    d = x.shape[0]
    if w_gate.shape[0] != d or w_up.shape != w_gate.shape or w_down.shape != (w_gate.shape[1], d):
        raise MoeError(
            f"shape mismatch: x={x.shape}, w_gate={w_gate.shape}, w_up={w_up.shape}, w_down={w_down.shape}"
        )
    g = x @ w_gate
    silu = g / (1.0 + np.exp(-g))
    return (silu * (x @ w_up)) @ w_down


def expert_forward(x, gate_q, up_q, down_q, *, group_size: int) -> np.ndarray:
    """Dequantize the three int4-quantized weight matrices, then run the SwiGLU FFN.

    Each *_q is a (codes, scales, shape) tuple as produced from a weight matrix.
    """
    def _de(bundle):
        codes, scales, shape = bundle
        return dequant_int4_groups(codes, scales, group_size=group_size).reshape(shape)

    return swiglu_expert(x, _de(gate_q), _de(up_q), _de(down_q))
