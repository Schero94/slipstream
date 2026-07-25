"""Hybrid decoder layer — CPU reference composition (the kernels assemble).

The capstone that proves the reference kernels compose into one coherent Qwen-hybrid
decoder layer, the exact numeric target the Metal port implements end to end:

    x  = rmsnorm(h)
    q,k,v = x·W_q, x·W_k, x·W_v
    S, attn = delta_net_step(S, k, v, q)        # linear-attention decode (Anhang C.1)
    h  = h + attn                                # residual
    y  = rmsnorm(h)
    ffn = moe_ffn(y, router, experts, top_k)     # top-k routing + expert compute
    h  = h + ffn                                 # residual

The full-attention layers swap `delta_net_step` for `quest_attention` (Anhang C.3);
prefill swaps it for `delta_net_prefill_chunked` (Anhang C.2). Experts are callables so
the layer runs equally on fp weights or the int4 `moe_expert` kernel on streamed bytes.
This is a single-token decode step on synthetic weights, verified by properties.
"""

from __future__ import annotations

from typing import Any, Callable

import numpy as np

from bench.m1.deltanet_ref import delta_net_step, rmsnorm
from bench.m1.moe_expert import expert_forward

Expert = Callable[[np.ndarray], np.ndarray]


class LayerError(Exception):
    """Raised for invalid layer inputs."""


def _softmax(z: np.ndarray) -> np.ndarray:
    z = z - np.max(z)
    e = np.exp(z)
    return e / e.sum()


def moe_ffn(
    x: np.ndarray, router_w: np.ndarray, experts: list[Expert], *, top_k: int
) -> tuple[np.ndarray, dict[str, Any]]:
    """Top-k MoE: route x, softmax-gate the selected experts, sum their outputs."""
    n_exp = len(experts)
    if router_w.shape != (x.shape[0], n_exp):
        raise LayerError(f"router_w shape {router_w.shape} != {(x.shape[0], n_exp)}")
    if not 1 <= top_k <= n_exp:
        raise LayerError(f"top_k {top_k} out of range 1..{n_exp}")
    logits = x @ router_w
    selected = sorted(np.argsort(-logits, kind="stable")[:top_k].tolist())
    gates = _softmax(logits[selected])
    out = np.zeros_like(x, dtype=np.float64)
    for gate, e in zip(gates, selected):
        out = out + gate * np.asarray(experts[e](x), dtype=np.float64)
    return out, {"selected": selected, "gates": gates.tolist()}


def make_quantized_expert(gate_q, up_q, down_q, *, group_size: int) -> Expert:
    """Wrap three int4-quantized weight bundles into an expert callable (moe_expert kernel)."""
    return lambda x: expert_forward(x, gate_q, up_q, down_q, group_size=group_size)


def hybrid_decode_layer(
    h: np.ndarray,
    S: np.ndarray,
    attn: dict[str, Any],
    router_w: np.ndarray,
    experts: list[Expert],
    *,
    top_k: int,
) -> tuple[np.ndarray, np.ndarray]:
    """One hybrid decoder layer decode step. Returns (h_out, updated attention state S)."""
    d = h.shape[0]
    if S.shape != (d, d):
        raise LayerError(f"S shape {S.shape} != {(d, d)}")
    x = rmsnorm(h)
    q = x @ attn["w_q"]
    k = x @ attn["w_k"]
    v = x @ attn["w_v"]
    S, attn_out = delta_net_step(S, k, v, q, alpha=attn["alpha"], beta=attn["beta"])
    h = h + attn_out
    y = rmsnorm(h)
    ffn, _ = moe_ffn(y, router_w, experts, top_k=top_k)
    h = h + ffn
    return h, S
