"""Gated DeltaNet decode — CPU reference kernel (Blueprint Anhang C.1).

The recurrent decode step of the Qwen hybrid's linear-attention layers, in plain
numpy. This is the "CPU-Referenz zuerst, byte-identisch, dann Metal" foundation:
the eventual Metal kernel is validated against THIS implementation. The delta
rule maintains an associative state S (values x keys); each step decays S,
writes the prediction error for the current (k, v) pair, and reads with q.

Verified here by properties, not yet by an fla/transformers oracle (that needs
torch + the flash-linear-attention kernels; deferred, noted honestly). The
load-bearing invariant IS tested: with a unit key, alpha=1 and beta=1, the
updated state reproduces the written value (S @ k̂ == v).
"""

from __future__ import annotations

import numpy as np

EPS = 1e-6


class DeltaNetError(Exception):
    """Raised for invalid kernel inputs."""


def l2norm(x: np.ndarray) -> np.ndarray:
    return x / (np.linalg.norm(x) + EPS)


def silu(x: np.ndarray) -> np.ndarray:
    return x / (1.0 + np.exp(-x))


def rmsnorm(x: np.ndarray, weight: np.ndarray | None = None, eps: float = EPS) -> np.ndarray:
    rms = np.sqrt(np.mean(x * x) + eps)
    out = x / rms
    return out if weight is None else out * weight


def delta_net_step(
    S: np.ndarray, k: np.ndarray, v: np.ndarray, q: np.ndarray, *, alpha: float, beta: float
) -> tuple[np.ndarray, np.ndarray]:
    """One decode step. S is (d_v, d_k); k,q are (d_k,); v is (d_v,).

    S <- alpha * S ; err = v - S @ k̂ ; S <- S + beta * outer(err, k̂) ; o = S @ q
    """
    if S.ndim != 2:
        raise DeltaNetError("S must be 2-D (d_v, d_k)")
    d_v, d_k = S.shape
    if k.shape != (d_k,) or q.shape != (d_k,) or v.shape != (d_v,):
        raise DeltaNetError(f"shape mismatch: S={S.shape}, k={k.shape}, v={v.shape}, q={q.shape}")
    k_hat = l2norm(k)
    S = alpha * S
    err = v - S @ k_hat
    S = S + beta * np.outer(err, k_hat)
    o = S @ q
    return S, o


def delta_net_decode(
    ks: np.ndarray,
    vs: np.ndarray,
    qs: np.ndarray,
    *,
    alpha: float,
    beta: float,
    S0: np.ndarray | None = None,
) -> tuple[np.ndarray, np.ndarray]:
    """Run T decode steps. ks,qs are (T, d_k); vs is (T, d_v). Returns (outputs, final S)."""
    if ks.ndim != 2 or vs.ndim != 2 or qs.ndim != 2:
        raise DeltaNetError("ks, vs, qs must be 2-D (T, d)")
    T, d_k = ks.shape
    if qs.shape != (T, d_k) or vs.shape[0] != T:
        raise DeltaNetError("inconsistent sequence shapes")
    d_v = vs.shape[1]
    S = np.zeros((d_v, d_k)) if S0 is None else S0
    if S.shape != (d_v, d_k):
        raise DeltaNetError(f"S0 shape {S.shape} != {(d_v, d_k)}")
    outs = np.empty((T, d_v))
    for t in range(T):
        S, o = delta_net_step(S, ks[t], vs[t], qs[t], alpha=alpha, beta=beta)
        outs[t] = o
    return outs, S
