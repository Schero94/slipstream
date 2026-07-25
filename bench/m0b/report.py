"""Complete M0b recall aggregation, decision bands, and atomic reports."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import re
import tempfile
from typing import Iterable, Mapping, Sequence

import numpy as np

from .constants import BUDGETS, CONTEXT_TOKENS, DECODE_STEPS, LAYERS, Q_HEADS


HEX_40 = re.compile(r"[0-9a-f]{40}")
HEX_64 = re.compile(r"[0-9a-f]{64}")


class ReportError(ValueError):
    """Raised when recall evidence is incomplete or unsafe to publish."""


@dataclass(frozen=True)
class Measurement:
    context: int
    layer: int
    step: int
    head: int
    budget: int
    covered_mass: float

    @property
    def key(self) -> tuple[int, int, int, int, int]:
        return self.context, self.layer, self.step, self.head, self.budget


def decide(macro_mean_by_budget: Mapping[int, float]) -> str:
    """Apply the user-approved strict B=64 primary decision contract."""

    try:
        at_64 = float(macro_mean_by_budget[64])
        at_128 = float(macro_mean_by_budget[128])
    except (KeyError, TypeError, ValueError) as error:
        raise ReportError("decision requires B=64 and B=128 macro means") from error
    if not all(math.isfinite(value) and 0.0 <= value <= 1.0 for value in (at_64, at_128)):
        raise ReportError("decision masses must be finite values in [0, 1]")
    if at_64 >= 0.95:
        return "green"
    if at_128 >= 0.95:
        return "yellow"
    return "red"


def _axis(values: Sequence[int], name: str) -> tuple[int, ...]:
    result = tuple(values)
    if not result or any(type(value) is not int or value < 0 for value in result) or len(set(result)) != len(result):
        raise ReportError(f"{name} must contain unique non-negative integers")
    return result


def _stats(values: Sequence[float]) -> dict[str, float | int]:
    array = np.asarray(values, dtype=np.float64)
    if array.ndim != 1 or array.size == 0 or not np.isfinite(array).all():
        raise ReportError("cannot aggregate empty or non-finite evidence")
    return {
        "count": int(array.size),
        "mean": float(np.mean(array, dtype=np.float64)),
        "median": float(np.median(array)),
        "p05": float(np.percentile(array, 5)),
        "worst": float(np.min(array)),
    }


def _budget_stats(cells: Sequence[Measurement], budgets: tuple[int, ...]) -> dict[str, dict[str, float | int]]:
    return {
        str(budget): _stats([cell.covered_mass for cell in cells if cell.budget == budget])
        for budget in budgets
    }


def _grouped(
    cells: Sequence[Measurement],
    budgets: tuple[int, ...],
    attribute: str,
    values: tuple[int, ...],
) -> dict[str, dict[str, dict[str, float | int]]]:
    return {
        str(value): _budget_stats([cell for cell in cells if getattr(cell, attribute) == value], budgets)
        for value in values
    }


def _validate_identities(
    capture_identities: Mapping[str, str],
    contexts: tuple[int, ...],
    upstream: Mapping[str, str],
) -> tuple[dict[str, str], dict[str, str]]:
    identities = dict(capture_identities)
    if set(identities) != {str(context) for context in contexts}:
        raise ReportError("capture identities do not cover every context")
    if any(not isinstance(value, str) or HEX_64.fullmatch(value) is None for value in identities.values()):
        raise ReportError("capture identities must be lowercase SHA-256 values")
    refs = dict(upstream)
    if set(refs) != {"main", "dev"}:
        raise ReportError("upstream must contain main and dev")
    if any(not isinstance(value, str) or HEX_40.fullmatch(value) is None for value in refs.values()):
        raise ReportError("upstream refs must be lowercase Git SHAs")
    return identities, refs


def build_report(
    measurements: Iterable[Measurement],
    *,
    capture_identities: Mapping[str, str],
    upstream: Mapping[str, str],
    contexts: Sequence[int] = CONTEXT_TOKENS,
    layers: Sequence[int] = LAYERS,
    steps: Sequence[int] = tuple(range(DECODE_STEPS)),
    heads: Sequence[int] = tuple(range(Q_HEADS)),
    budgets: Sequence[int] = BUDGETS,
) -> dict[str, object]:
    """Validate the complete Cartesian evidence grid and produce immutable aggregates."""

    contexts_t = _axis(contexts, "contexts")
    layers_t = _axis(layers, "layers")
    steps_t = _axis(steps, "steps")
    heads_t = _axis(heads, "heads")
    budgets_t = _axis(budgets, "budgets")
    if 64 not in budgets_t or 128 not in budgets_t:
        raise ReportError("budgets must include 64 and 128")
    identities, refs = _validate_identities(capture_identities, contexts_t, upstream)
    cells = tuple(measurements)
    if not all(isinstance(cell, Measurement) for cell in cells):
        raise ReportError("all evidence rows must be Measurement instances")
    expected = {
        (context, layer, step, head, budget)
        for context in contexts_t
        for layer in layers_t
        for step in steps_t
        for head in heads_t
        for budget in budgets_t
    }
    observed = [cell.key for cell in cells]
    if len(observed) != len(set(observed)):
        raise ReportError("duplicate recall measurement")
    if set(observed) != expected:
        raise ReportError("recall evidence grid is incomplete or contains unexpected cells")
    for cell in cells:
        if not math.isfinite(cell.covered_mass) or not 0.0 <= cell.covered_mass <= 1.0:
            raise ReportError("covered mass must be finite and in [0, 1]")
    cells = tuple(sorted(cells, key=lambda cell: cell.key))
    overall = _budget_stats(cells, budgets_t)
    means = {budget: float(overall[str(budget)]["mean"]) for budget in budgets_t}
    warnings = [
        {**asdict(cell), "reason": "severe B=128 tail below 0.85"}
        for cell in cells
        if cell.budget == 128 and cell.covered_mass < 0.85
    ]
    return {
        "schema": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "numpy_version": np.__version__,
        "capture_identities": identities,
        "upstream": refs,
        "geometry": {
            "contexts": list(contexts_t),
            "layers": list(layers_t),
            "steps": list(steps_t),
            "heads": list(heads_t),
            "budgets": list(budgets_t),
        },
        "decision": {
            "band": decide(means),
            "primary": "macro-mean covered attention mass at B=64",
            "green_threshold": 0.95,
            "macro_mean_by_budget": {str(key): value for key, value in means.items()},
        },
        "aggregates": {
            "overall": overall,
            "by_context": _grouped(cells, budgets_t, "context", contexts_t),
            "by_layer": _grouped(cells, budgets_t, "layer", layers_t),
            "by_step": _grouped(cells, budgets_t, "step", steps_t),
            "by_head": _grouped(cells, budgets_t, "head", heads_t),
        },
        "tail_warnings": warnings,
        "measurements": [asdict(cell) for cell in cells],
    }


def write_report(path: Path, report: Mapping[str, object]) -> None:
    """Atomically publish a report without replacing evidence from other captures."""

    path = Path(path)
    if path.is_symlink():
        raise ReportError("report path may not be a symlink")
    if not path.parent.is_dir() or path.parent.is_symlink():
        raise ReportError("report parent must be an existing regular directory")
    document = dict(report)
    if document.get("schema") != 1 or not isinstance(document.get("capture_identities"), Mapping):
        raise ReportError("invalid report document")
    if path.exists():
        try:
            previous = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ReportError(f"existing report is unreadable: {error}") from error
        if previous.get("capture_identities") != document["capture_identities"]:
            raise ReportError("refusing to replace different capture identities")
    temporary_name = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as stream:
            temporary_name = stream.name
            json.dump(document, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_name, path)
        temporary_name = None
    finally:
        if temporary_name is not None:
            Path(temporary_name).unlink(missing_ok=True)
