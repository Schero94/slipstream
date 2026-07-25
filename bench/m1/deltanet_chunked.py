"""Gated DeltaNet chunkwise prefill — CPU reference kernel (Blueprint Anhang C.2).

The prefill counterpart to the recurrent decode (`deltanet_ref.delta_net_decode`).
Prefill processes a whole prompt at once, so it must NOT be a token-at-a-time loop:
it blocks the sequence into chunks and, within each chunk, replaces the sequential
recurrence with matrix operations (the WY / UT-transform of the delta rule), carrying
one associative state S across chunk boundaries. This is the data layout the eventual
Metal prefill kernel targets — and it is verified byte-close against the recurrence
here (that recurrence is the ground truth, so no external oracle is needed).

Derivation (scalar decay alpha, scalar beta). With w_i = l2norm(k_i) and
u_t = beta*(v_t - alpha*S_{t-1} w_t), the recurrence S_t = alpha*S_{t-1} + u_t w_t^T
unrolls to S_t = alpha^t S_0 + Σ_{i<=t} alpha^{t-i} u_i w_i^T. Substituting back,
the intra-chunk U solves a unit-lower-triangular system
    (I + M) U = beta*(V - decay∘(S_0 W^T)),   M[t,i] = beta*alpha^{t-i}*(w_i·w_t), i<t
by forward substitution, then
    O = decay∘(Q S_0^T) + (lower_incl(alpha^{t-i}*(w_i·q_t))) U
    S_C = alpha^C S_0 + (alpha^{C-1-i} u_i)^T W .
"""

from __future__ import annotations

import numpy as np

from bench.m1.deltanet_ref import DeltaNetError, l2norm


def _prefill_chunk(
    K: np.ndarray, V: np.ndarray, Q: np.ndarray, S0: np.ndarray, alpha: float, beta: float
) -> tuple[np.ndarray, np.ndarray]:
    C, d_k = K.shape
    d_v = V.shape[1]
    W = np.stack([l2norm(K[t]) for t in range(C)])          # (C, d_k) normalized keys
    idx = np.arange(C)
    diff = idx[:, None] - idx[None, :]                       # p - q
    DP = np.where(diff >= 0, np.power(float(alpha), diff.astype(float)), 0.0)  # alpha^(p-q), p>=q

    G = W @ W.T                                              # (C,C) w_p·w_q
    QW = Q @ W.T                                             # (C,C) q_p·w_q
    strict_lower = np.tril(np.ones((C, C)), k=-1)
    incl_lower = np.tril(np.ones((C, C)), k=0)

    M = beta * (DP * G) * strict_lower                       # strictly lower
    pow_step = np.power(float(alpha), idx + 1)               # alpha^(p+1) = alpha^t (1-indexed)
    S0W = Q @ S0.T                                           # (C, d_v): row p = S0 @ q_p (inter-chunk read)
    # RHS[p] = beta*(V[p] - alpha^(p+1) * (S0 @ w_p))
    S0_wp = W @ S0.T                                         # (C, d_v): row p = S0 @ w_p
    RHS = beta * (V - pow_step[:, None] * S0_wp)

    # forward substitution for unit-lower-triangular (I + M) U = RHS
    U = np.empty((C, d_v))
    for p in range(C):
        acc = RHS[p].copy()
        if p:
            acc -= M[p, :p] @ U[:p]
        U[p] = acc

    P = (DP * QW) * incl_lower                                # alpha^(p-q)*(q_p·w_q), q<=p
    O = pow_step[:, None] * S0W + P @ U                       # (C, d_v)

    # carry state: S_C = alpha^C S0 + Σ_i alpha^(C-1-i) u_i w_i^T
    pow_carry = np.power(float(alpha), (C - 1 - idx).astype(float))  # alpha^(C-1-i)
    S_C = (float(alpha) ** C) * S0 + (pow_carry[:, None] * U).T @ W
    return O, S_C


def delta_net_prefill_chunked(
    ks: np.ndarray,
    vs: np.ndarray,
    qs: np.ndarray,
    *,
    alpha: float,
    beta: float,
    chunk_size: int,
    S0: np.ndarray | None = None,
) -> tuple[np.ndarray, np.ndarray]:
    """Chunkwise prefill. ks,qs are (T,d_k); vs is (T,d_v). Returns (outputs, final S)."""
    if ks.ndim != 2 or vs.ndim != 2 or qs.ndim != 2:
        raise DeltaNetError("ks, vs, qs must be 2-D (T, d)")
    if chunk_size < 1:
        raise DeltaNetError("chunk_size must be >= 1")
    T, d_k = ks.shape
    if qs.shape != (T, d_k) or vs.shape[0] != T:
        raise DeltaNetError("inconsistent sequence shapes")
    d_v = vs.shape[1]
    S = np.zeros((d_v, d_k)) if S0 is None else S0
    if S.shape != (d_v, d_k):
        raise DeltaNetError(f"S0 shape {S.shape} != {(d_v, d_k)}")

    outs = np.empty((T, d_v))
    for start in range(0, T, chunk_size):
        end = min(start + chunk_size, T)
        O, S = _prefill_chunk(ks[start:end], vs[start:end], qs[start:end], S, alpha, beta)
        outs[start:end] = O
    return outs, S
