"""Quest sparse attention — CPU reference kernel (Blueprint Anhang C.3).

This is the M0b kernel: for the hybrid's full-attention layers, estimate each
128-block's relevance with a min/max upper bound on q·k, keep the top-B blocks,
and attend only over their keys. It answers "does top-B block selection recall
the attention mass?" — verifiable here against exact dense attention as ground
truth (no external oracle needed).

Stage 1 (Quest estimate): ub(b) = Σ_d max(q_d·minK_d, q_d·maxK_d) upper-bounds the
largest q·k in block b. Stage 2: attend over the selected blocks with an online
softmax. `quest_recall` reports the fraction of the exact softmax mass captured by
the selected blocks — the metric M0b gates (>=95% green, <85% reject).
"""

from __future__ import annotations

import numpy as np


class QuestError(Exception):
    """Raised for invalid Quest inputs."""


def dense_attention(q: np.ndarray, K: np.ndarray, V: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Exact single-query attention. Returns (output, softmax weights)."""
    if q.ndim != 1 or K.ndim != 2 or K.shape[1] != q.shape[0]:
        raise QuestError("shape mismatch between q and K")
    scale = 1.0 / np.sqrt(q.shape[0])
    scores = (K @ q) * scale
    scores = scores - scores.max()
    weights = np.exp(scores)
    weights = weights / weights.sum()
    return weights @ V, weights


def block_bounds(K: np.ndarray, *, block_size: int) -> tuple[np.ndarray, np.ndarray]:
    """Per-block per-dim min and max over keys. Returns (mins, maxs), each (n_blocks, d)."""
    if block_size < 1:
        raise QuestError("block_size must be >= 1")
    n = K.shape[0]
    n_blocks = (n + block_size - 1) // block_size
    d = K.shape[1]
    mins = np.empty((n_blocks, d))
    maxs = np.empty((n_blocks, d))
    for b in range(n_blocks):
        chunk = K[b * block_size:(b + 1) * block_size]
        mins[b] = chunk.min(axis=0)
        maxs[b] = chunk.max(axis=0)
    return mins, maxs


def quest_scores(q: np.ndarray, mins: np.ndarray, maxs: np.ndarray) -> np.ndarray:
    """ub(b) = Σ_d max(q_d·min_d, q_d·max_d) — an upper bound on max q·k in the block."""
    return np.maximum(q * mins, q * maxs).sum(axis=1)


def quest_select(scores: np.ndarray, top_b: int) -> list[int]:
    if top_b < 1:
        raise QuestError("top_b must be >= 1")
    order = np.argsort(-scores, kind="stable")
    return list(order[:top_b])


def _selected_key_indices(selected_blocks, block_size: int, n: int) -> list[int]:
    idx: list[int] = []
    for b in selected_blocks:
        idx.extend(range(b * block_size, min((b + 1) * block_size, n)))
    return idx


def quest_recall(q: np.ndarray, K: np.ndarray, dense_weights: np.ndarray, *, block_size: int, top_b: int) -> float:
    """Fraction of the exact softmax mass held by the top-B Quest-selected blocks."""
    mins, maxs = block_bounds(K, block_size=block_size)
    selected = quest_select(quest_scores(q, mins, maxs), top_b)
    idx = _selected_key_indices(selected, block_size, K.shape[0])
    return float(dense_weights[idx].sum())


def quest_attention(q: np.ndarray, K: np.ndarray, V: np.ndarray, *, block_size: int, top_b: int) -> np.ndarray:
    """Attend over only the top-B blocks' keys (dense fallback if it selects all)."""
    mins, maxs = block_bounds(K, block_size=block_size)
    selected = quest_select(quest_scores(q, mins, maxs), top_b)
    idx = _selected_key_indices(selected, block_size, K.shape[0])
    out, _ = dense_attention(q, K[idx], V[idx])
    return out
