"""Deterministic perplexity evidence for numerics-isolated M0a profiles."""

from __future__ import annotations

import argparse
from dataclasses import asdict
import hashlib
import json
import math
from pathlib import Path
import re
import subprocess
from typing import Mapping

from bench.m0a.qualify_model import (
    QUALIFICATION_PROFILES,
    QualificationProfile,
    _read_manifest,
    production_environment,
)
from bench.m0a.smoke_server import _write_json_atomic


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_BINARY = REPO_ROOT / "vendor" / "llama.cpp" / "build" / "bin" / "llama-perplexity"
DEFAULT_CORPUS = REPO_ROOT / "bench" / "m0a" / "eval" / "coding-perplexity-v1.txt"
DEFAULT_CONTEXT_SIZE = 512
MAX_PERPLEXITY_DELTA = 0.05
_FINAL_ESTIMATE = re.compile(
    r"Final estimate:\s*PPL\s*=\s*([0-9]+(?:\.[0-9]+)?)\s*\+/-\s*([0-9]+(?:\.[0-9]+)?)"
)


class PerplexityError(RuntimeError):
    """Raised when a perplexity artifact cannot support a trustworthy gate."""


def corpus_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_perplexity_output(output: str) -> tuple[float, float]:
    matches = _FINAL_ESTIMATE.findall(output)
    if len(matches) != 1:
        raise PerplexityError("expected exactly one finite final perplexity estimate")
    perplexity, uncertainty = (float(value) for value in matches[0])
    if not all(math.isfinite(value) and value >= 0 for value in (perplexity, uncertainty)):
        raise PerplexityError("final perplexity estimate is not finite")
    return perplexity, uncertainty


def perplexity_command(
    model: Path,
    corpus: Path,
    *,
    profile: QualificationProfile,
    binary: Path = DEFAULT_BINARY,
    context_size: int = DEFAULT_CONTEXT_SIZE,
) -> list[str]:
    if context_size <= 0:
        raise ValueError("context size must be positive")
    return [
        str(binary),
        "--model",
        str(model),
        "--file",
        str(corpus),
        "--ctx-size",
        str(context_size),
        "--batch-size",
        str(context_size),
        "--ubatch-size",
        str(context_size),
        "--gpu-layers",
        "99",
        "--flash-attn",
        profile.flash_attention,
        "--cache-type-k",
        profile.cache_type_k,
        "--cache-type-v",
        profile.cache_type_v,
    ]


def evaluate_perplexity_pair(
    baseline: Mapping[str, object],
    candidate: Mapping[str, object],
    *,
    maximum_delta: float = MAX_PERPLEXITY_DELTA,
) -> dict[str, object]:
    reasons: list[str] = []
    for key in ("model_sha256", "corpus_sha256", "context_size"):
        if baseline.get(key) != candidate.get(key):
            reasons.append(f"mismatch-{key.replace('_', '-')}")
    try:
        baseline_ppl = float(baseline["perplexity"])
        candidate_ppl = float(candidate["perplexity"])
    except (KeyError, TypeError, ValueError):
        return {"passed": False, "reasons": reasons + ["invalid-perplexity"], "delta": None}
    delta = candidate_ppl - baseline_ppl
    if not all(math.isfinite(value) and value >= 0 for value in (baseline_ppl, candidate_ppl)):
        reasons.append("invalid-perplexity")
    elif delta > maximum_delta + 1e-12:
        reasons.append("perplexity-delta")
    return {
        "passed": not reasons,
        "reasons": reasons,
        "delta": delta,
        "maximum_delta": maximum_delta,
    }


def run_perplexity(
    model: Path,
    corpus: Path,
    output: Path,
    *,
    profile: QualificationProfile,
    binary: Path = DEFAULT_BINARY,
    context_size: int = DEFAULT_CONTEXT_SIZE,
) -> dict[str, object]:
    manifest = _read_manifest(model)
    command = perplexity_command(
        model,
        corpus,
        profile=profile,
        binary=binary,
        context_size=context_size,
    )
    completed = subprocess.run(
        command,
        env=production_environment(),
        capture_output=True,
        text=True,
        check=False,
    )
    combined = completed.stdout + "\n" + completed.stderr
    if completed.returncode != 0:
        raise PerplexityError(f"llama-perplexity exited with {completed.returncode}")
    perplexity, uncertainty = parse_perplexity_output(combined)
    report: dict[str, object] = {
        "schema": 1,
        "model_path": str(model.resolve()),
        "model_sha256": manifest["sha256"],
        "corpus_path": str(corpus.resolve()),
        "corpus_sha256": corpus_sha256(corpus),
        "context_size": context_size,
        "runtime_profile": asdict(profile),
        "command": command,
        "perplexity": perplexity,
        "uncertainty": uncertainty,
        "returncode": completed.returncode,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    _write_json_atomic(output, report)
    output.with_suffix(output.suffix + ".log").write_text(combined, encoding="utf-8")
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--corpus", type=Path, default=DEFAULT_CORPUS)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--profile", required=True, choices=tuple(QUALIFICATION_PROFILES))
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    parser.add_argument("--context-size", type=int, default=DEFAULT_CONTEXT_SIZE)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = run_perplexity(
            args.model,
            args.corpus,
            args.output,
            profile=QUALIFICATION_PROFILES[args.profile],
            binary=args.binary,
            context_size=args.context_size,
        )
    except (OSError, PerplexityError, ValueError) as error:
        print(f"perplexity failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
