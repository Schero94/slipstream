"""Exact float32 attention and Quest block-recall evaluation."""

from __future__ import annotations

from dataclasses import dataclass
import math
from numbers import Real
from typing import Iterable

import numpy as np


class RecallError(ValueError):
    """Raised when recall inputs or geometry are invalid."""


@dataclass(frozen=True)
class RecallCell:
    query_head: int
    kv_head: int
    budget: int
    covered_mass: float
    token_count: int
    block_count: int


def _positive_real(value: object, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, Real):
        raise RecallError(f"{name} must be a finite positive real")
    result = float(value)
    if not math.isfinite(result) or result <= 0:
        raise RecallError(f"{name} must be a finite positive real")
    return result


def _positive_integer(value: object, name: str) -> int:
    if type(value) is not int or value <= 0:
        raise RecallError(f"{name} must be a positive integer")
    return value


def _q_and_keys(q: np.ndarray, keys: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    query = np.asarray(q)
    key_array = np.asarray(keys)
    if query.ndim != 1 or key_array.ndim != 2 or key_array.shape[0] == 0:
        raise RecallError("query must be 1-D and keys must be a nonempty 2-D array")
    if query.shape[0] != key_array.shape[1]:
        raise RecallError("query and key dimensions do not match")
    if query.dtype.kind not in "fc" or key_array.dtype.kind not in "fc":
        raise RecallError("query and keys must be floating-point arrays")
    query_f32 = np.asarray(query, dtype=np.float32)
    if not np.isfinite(query_f32).all():
        raise RecallError("query contains a non-finite value")
    return query_f32, key_array


def exact_attention_probabilities(
    q: np.ndarray,
    keys: np.ndarray,
    *,
    scale: float = 1.0,
    chunk_tokens: int = 8192,
) -> np.ndarray:
    """Return stable float32 causal attention probabilities for one query head."""

    query, key_array = _q_and_keys(q, keys)
    scale_f32 = np.float32(_positive_real(scale, "scale"))
    chunk_tokens = _positive_integer(chunk_tokens, "chunk_tokens")
    scores = np.empty(key_array.shape[0], dtype=np.float32)
    for start in range(0, key_array.shape[0], chunk_tokens):
        stop = min(start + chunk_tokens, key_array.shape[0])
        block = np.asarray(key_array[start:stop], dtype=np.float32)
        if not np.isfinite(block).all():
            raise RecallError("keys contain a non-finite value")
        scores[start:stop] = block @ query
    scores *= scale_f32
    maximum = np.max(scores)
    scores -= maximum
    np.exp(scores, out=scores)
    denominator = np.sum(scores, dtype=np.float32)
    if not np.isfinite(denominator) or denominator <= 0:
        raise RecallError("stable softmax normalization failed")
    scores /= denominator
    return scores


def quest_block_bounds(
    q: np.ndarray,
    keys: np.ndarray,
    *,
    block_size: int,
    scale: float = 1.0,
) -> np.ndarray:
    """Return Quest min/max upper bounds for every consecutive key block."""

    query, key_array = _q_and_keys(q, keys)
    block_size = _positive_integer(block_size, "block_size")
    scale_f32 = np.float32(_positive_real(scale, "scale"))
    minima, maxima = _block_extrema(key_array, block_size)
    return _bounds_from_extrema(query, minima, maxima, scale_f32)


def _block_extrema(key_array: np.ndarray, block_size: int) -> tuple[np.ndarray, np.ndarray]:
    token_count, dimension = key_array.shape
    full_count = token_count // block_size
    minimum_parts = []
    maximum_parts = []
    if full_count:
        full = key_array[: full_count * block_size].reshape(full_count, block_size, dimension)
        minimum_parts.append(np.asarray(np.min(full, axis=1), dtype=np.float32))
        maximum_parts.append(np.asarray(np.max(full, axis=1), dtype=np.float32))
    if full_count * block_size < token_count:
        partial = key_array[full_count * block_size :]
        minimum_parts.append(np.asarray(np.min(partial, axis=0, keepdims=True), dtype=np.float32))
        maximum_parts.append(np.asarray(np.max(partial, axis=0, keepdims=True), dtype=np.float32))
    minima = np.concatenate(minimum_parts, axis=0)
    maxima = np.concatenate(maximum_parts, axis=0)
    if not np.isfinite(minima).all() or not np.isfinite(maxima).all():
        raise RecallError("keys contain a non-finite value")
    return minima, maxima


def _bounds_from_extrema(
    query: np.ndarray,
    minima: np.ndarray,
    maxima: np.ndarray,
    scale: np.float32,
) -> np.ndarray:
    contributions = np.where(query >= 0, maxima * query, minima * query)
    bounds = np.sum(contributions, axis=1, dtype=np.float32)
    bounds *= scale
    if not np.isfinite(bounds).all():
        raise RecallError("Quest bound computation produced a non-finite value")
    return bounds


def _group_probabilities(
    queries: np.ndarray,
    keys: np.ndarray,
    *,
    scale: np.float32,
    chunk_tokens: int,
) -> np.ndarray:
    """Score all query heads sharing one KV head in a single chunked matmul."""

    token_count = keys.shape[0]
    scores = np.empty((token_count, queries.shape[0]), dtype=np.float32)
    query_transpose = np.asarray(queries, dtype=np.float32).T
    if not np.isfinite(query_transpose).all():
        raise RecallError("queries contain a non-finite value")
    for start in range(0, token_count, chunk_tokens):
        stop = min(start + chunk_tokens, token_count)
        block = np.asarray(keys[start:stop], dtype=np.float32)
        if not np.isfinite(block).all():
            raise RecallError("keys contain a non-finite value")
        scores[start:stop] = block @ query_transpose
    scores *= scale
    scores -= np.max(scores, axis=0, keepdims=True)
    np.exp(scores, out=scores)
    denominator = np.sum(scores, axis=0, dtype=np.float32, keepdims=True)
    if not np.isfinite(denominator).all() or np.any(denominator <= 0):
        raise RecallError("stable softmax normalization failed")
    scores /= denominator
    return scores


def select_blocks(bounds: np.ndarray, *, budget: int) -> np.ndarray:
    """Select top-bound block indices with deterministic position tie breaking."""

    values = np.asarray(bounds)
    budget = _positive_integer(budget, "budget")
    if values.ndim != 1 or values.size == 0 or values.dtype.kind not in "fc":
        raise RecallError("bounds must be a nonempty floating-point vector")
    if not np.isfinite(values).all():
        raise RecallError("bounds contain a non-finite value")
    if budget >= values.size:
        return np.arange(values.size, dtype=np.int64)
    candidate = np.argpartition(-values, budget - 1)[:budget]
    threshold = np.min(values[candidate])
    above = np.flatnonzero(values > threshold)
    equal = np.flatnonzero(values == threshold)
    remaining = budget - above.size
    if remaining < 0 or remaining > equal.size:
        raise RecallError("partial selection invariant failed")
    selected = np.concatenate((above, equal[:remaining])).astype(np.int64, copy=False)
    selected.sort()
    return selected


def covered_mass(probabilities: np.ndarray, selected_blocks: np.ndarray, *, block_size: int) -> float:
    """Sum exact attention mass covered by selected consecutive blocks."""

    values = np.asarray(probabilities)
    selected = np.asarray(selected_blocks)
    block_size = _positive_integer(block_size, "block_size")
    if values.ndim != 1 or values.size == 0 or values.dtype.kind not in "fc":
        raise RecallError("probabilities must be a nonempty floating-point vector")
    if not np.isfinite(values).all() or np.any(values < 0):
        raise RecallError("probabilities must be finite and non-negative")
    if selected.ndim != 1 or selected.dtype.kind not in "iu":
        raise RecallError("selected blocks must be an integer vector")
    block_count = (values.size + block_size - 1) // block_size
    selected_i64 = selected.astype(np.int64, copy=False)
    if np.any(selected_i64 < 0) or np.any(selected_i64 >= block_count):
        raise RecallError("selected block is out of range")
    if np.unique(selected_i64).size != selected_i64.size:
        raise RecallError("selected blocks contain duplicates")
    total = np.float32(0.0)
    for block in selected_i64:
        start = int(block) * block_size
        stop = min(start + block_size, values.size)
        total += np.sum(values[start:stop], dtype=np.float32)
    return float(total)


def evaluate_gqa_step(
    queries: np.ndarray,
    keys: np.ndarray,
    *,
    budgets: Iterable[int],
    block_size: int,
    scale: float,
    valid_tokens: int | None = None,
    chunk_tokens: int = 8192,
) -> tuple[RecallCell, ...]:
    """Evaluate every query head against its GQA KV head for one decode step."""

    query_array = np.asarray(queries)
    key_array = np.asarray(keys)
    if query_array.ndim != 2 or key_array.ndim != 3:
        raise RecallError("GQA queries and keys must be 2-D and 3-D")
    q_heads, dimension = query_array.shape
    kv_heads, token_count, key_dimension = key_array.shape
    if dimension != key_dimension or q_heads == 0 or kv_heads == 0 or q_heads % kv_heads:
        raise RecallError("invalid GQA geometry")
    if valid_tokens is None:
        valid_tokens = token_count
    if type(valid_tokens) is not int or not 0 < valid_tokens <= token_count:
        raise RecallError("valid_tokens is outside the key range")
    budgets_tuple = tuple(budgets)
    if not budgets_tuple or any(type(value) is not int or value <= 0 for value in budgets_tuple):
        raise RecallError("budgets must contain positive integers")
    if len(set(budgets_tuple)) != len(budgets_tuple):
        raise RecallError("budgets contain duplicates")
    block_size = _positive_integer(block_size, "block_size")
    chunk_tokens = _positive_integer(chunk_tokens, "chunk_tokens")
    scale_f32 = np.float32(_positive_real(scale, "scale"))
    group_size = q_heads // kv_heads
    cells = []
    for kv_head in range(kv_heads):
        causal_keys = key_array[kv_head, :valid_tokens]
        first_query_head = kv_head * group_size
        group_queries = np.asarray(
            query_array[first_query_head : first_query_head + group_size],
            dtype=np.float32,
        )
        probabilities = _group_probabilities(
            group_queries,
            causal_keys,
            scale=scale_f32,
            chunk_tokens=chunk_tokens,
        )
        minima, maxima = _block_extrema(causal_keys, block_size)
        for local_head in range(group_size):
            query_head = first_query_head + local_head
            bounds = _bounds_from_extrema(group_queries[local_head], minima, maxima, scale_f32)
            for budget in budgets_tuple:
                selected = select_blocks(bounds, budget=budget)
                cells.append(
                    RecallCell(
                        query_head=query_head,
                        kv_head=kv_head,
                        budget=budget,
                        covered_mass=covered_mass(
                            probabilities[:, local_head],
                            selected,
                            block_size=block_size,
                        ),
                        token_count=valid_tokens,
                        block_count=bounds.size,
                    )
                )
    return tuple(cells)
