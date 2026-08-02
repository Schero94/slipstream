#!/usr/bin/env python3
"""Run a pinned, isolated baseline/candidate qualification and emit evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import statistics
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REQUIRED_METRICS = ("output", "tok_s", "ttft_ms", "peak_rss_mb", "swap_delta_mb")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")


class EvidenceError(RuntimeError):
    """Qualification cannot proceed without trustworthy evidence."""


def git_clean_head(worktree: Path) -> str:
    worktree = worktree.resolve()
    if not (worktree / ".git").exists():
        raise EvidenceError(f"not a git worktree: {worktree}")
    status = subprocess.run(
        ["git", "-C", str(worktree), "status", "--porcelain=v1", "--untracked-files=all"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    if status.strip():
        raise EvidenceError(f"worktree is not clean: {worktree}")
    head = subprocess.run(
        ["git", "-C", str(worktree), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if not SHA_RE.fullmatch(head):
        raise EvidenceError(f"invalid git HEAD for {worktree}: {head!r}")
    return head


def validate_candidate(candidate: dict[str, Any]) -> None:
    for key in ("id", "status", "baseline_sha", "candidate_sha", "command", "acceptance"):
        if key not in candidate:
            raise EvidenceError(f"candidate missing {key}")
    for key in ("baseline_sha", "candidate_sha"):
        if not isinstance(candidate[key], str) or not SHA_RE.fullmatch(candidate[key]):
            raise EvidenceError(f"candidate {key} must be a pinned 40-character lowercase SHA")
    command = candidate["command"]
    if not isinstance(command, list) or not command or not all(isinstance(v, str) and v for v in command):
        raise EvidenceError("candidate command must be a non-empty argv array")
    if not isinstance(candidate["acceptance"], dict):
        raise EvidenceError("candidate acceptance must be an object")


def _metric(raw: dict[str, Any]) -> dict[str, Any]:
    missing = [key for key in REQUIRED_METRICS if key not in raw]
    if missing:
        raise EvidenceError(f"benchmark evidence missing fields: {', '.join(missing)}")
    if not isinstance(raw["output"], str):
        raise EvidenceError("benchmark output must be a string")
    metric: dict[str, Any] = {
        "output_sha256": hashlib.sha256(raw["output"].encode("utf-8")).hexdigest(),
    }
    for key in REQUIRED_METRICS[1:]:
        value = raw[key]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise EvidenceError(f"benchmark {key} must be numeric")
        value = float(value)
        if not math.isfinite(value) or value < 0:
            raise EvidenceError(f"benchmark {key} must be finite and non-negative")
        metric[key] = value
    return metric


def run_side(
    command: list[str],
    worktree: Path,
    side: str,
    *,
    warmups: int,
    repeats: int,
    timeout_seconds: int = 3600,
) -> list[dict[str, Any]]:
    if warmups < 1 or repeats < 3:
        raise EvidenceError("qualification requires at least 1 warmup and 3 measured repeats")
    worktree = worktree.resolve()
    argv = [part.replace("{worktree}", str(worktree)) for part in command]
    measured: list[dict[str, Any]] = []
    for index in range(warmups + repeats):
        started = time.monotonic()
        proc = subprocess.run(
            argv,
            cwd=worktree,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
        )
        if proc.returncode != 0:
            stderr = proc.stderr[-1000:].replace("\n", " ")
            raise EvidenceError(f"{side} benchmark failed rc={proc.returncode}: {stderr}")
        lines = [line for line in proc.stdout.splitlines() if line.strip()]
        if not lines:
            raise EvidenceError(f"{side} benchmark emitted no JSON evidence")
        try:
            raw = json.loads(lines[-1])
        except json.JSONDecodeError as error:
            raise EvidenceError(f"{side} benchmark final line is not JSON") from error
        if not isinstance(raw, dict):
            raise EvidenceError(f"{side} benchmark JSON must be an object")
        metric = _metric(raw)
        metric["wall_seconds"] = time.monotonic() - started
        metric["repeat"] = index - warmups if index >= warmups else None
        if index >= warmups:
            measured.append(metric)
    return measured


def _median(runs: list[dict[str, Any]], key: str) -> float:
    return float(statistics.median(run[key] for run in runs))


def evaluate(
    baseline: list[dict[str, Any]],
    candidate: list[dict[str, Any]],
    acceptance: dict[str, Any],
) -> dict[str, Any]:
    if len(baseline) < 3 or len(candidate) < 3:
        raise EvidenceError("at least three measured runs per side are required")
    baseline = [_metric(run) if "output" in run else _validate_normalized(run) for run in baseline]
    candidate = [_metric(run) if "output" in run else _validate_normalized(run) for run in candidate]

    base_hashes = {run["output_sha256"] for run in baseline}
    candidate_hashes = {run["output_sha256"] for run in candidate}
    medians = {
        "baseline": {key: _median(baseline, key) for key in REQUIRED_METRICS[1:]},
        "candidate": {key: _median(candidate, key) for key in REQUIRED_METRICS[1:]},
    }
    ratios = {
        "tok_s": _safe_ratio(medians["candidate"]["tok_s"], medians["baseline"]["tok_s"]),
        "ttft": _safe_ratio(medians["candidate"]["ttft_ms"], medians["baseline"]["ttft_ms"]),
        "rss": _safe_ratio(
            medians["candidate"]["peak_rss_mb"], medians["baseline"]["peak_rss_mb"]
        ),
    }
    limits = {
        "min_tok_s_ratio": float(acceptance.get("min_tok_s_ratio", 0.98)),
        "max_ttft_ratio": float(acceptance.get("max_ttft_ratio", 1.10)),
        "max_rss_ratio": float(acceptance.get("max_rss_ratio", 1.10)),
        "max_swap_delta_mb": float(acceptance.get("max_swap_delta_mb", 0.0)),
    }
    reasons: list[str] = []
    if len(base_hashes) != 1:
        reasons.append("baseline_output_nondeterministic")
    if len(candidate_hashes) != 1:
        reasons.append("candidate_output_nondeterministic")
    if base_hashes != candidate_hashes:
        reasons.append("quality_output_mismatch")
    if ratios["tok_s"] < limits["min_tok_s_ratio"]:
        reasons.append("tok_s_regression")
    if ratios["ttft"] > limits["max_ttft_ratio"]:
        reasons.append("ttft_regression")
    if ratios["rss"] > limits["max_rss_ratio"]:
        reasons.append("rss_regression")
    if any(run["swap_delta_mb"] > limits["max_swap_delta_mb"] for run in candidate):
        reasons.append("swap_activity")
    return {
        "decision": "accepted" if not reasons else "rejected",
        "reasons": reasons,
        "medians": medians,
        "ratios": ratios,
        "limits": limits,
        "output_sha256": next(iter(base_hashes)) if len(base_hashes) == 1 else None,
    }


def _validate_normalized(run: dict[str, Any]) -> dict[str, Any]:
    required = ("output_sha256",) + REQUIRED_METRICS[1:]
    missing = [key for key in required if key not in run]
    if missing:
        raise EvidenceError(f"benchmark evidence missing fields: {', '.join(missing)}")
    if not re.fullmatch(r"[0-9a-f]{64}", str(run["output_sha256"])):
        raise EvidenceError("invalid output_sha256")
    normalized = {"output_sha256": run["output_sha256"]}
    for key in REQUIRED_METRICS[1:]:
        value = run[key]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise EvidenceError(f"benchmark {key} must be numeric")
        value = float(value)
        if not math.isfinite(value) or value < 0:
            raise EvidenceError(f"benchmark {key} must be finite and non-negative")
        normalized[key] = value
    return normalized


def _safe_ratio(numerator: float, denominator: float) -> float:
    if denominator == 0:
        return 1.0 if numerator == 0 else math.inf
    return numerator / denominator


def _load_candidate(path: Path, candidate_id: str) -> dict[str, Any]:
    data = json.loads(path.read_text())
    matches = [item for item in data.get("candidates", []) if item.get("id") == candidate_id]
    if len(matches) != 1:
        raise EvidenceError(f"expected exactly one candidate id={candidate_id!r}")
    return matches[0]


def _markdown(report: dict[str, Any]) -> str:
    evaluation = report["evaluation"]
    return "\n".join(
        [
            f"# Vendor A/B: {report['candidate']['id']}",
            "",
            f"- Decision: **{evaluation['decision'].upper()}**",
            f"- Baseline SHA: `{report['baseline']['sha']}`",
            f"- Candidate SHA: `{report['candidate']['sha']}`",
            f"- Token/s ratio: {evaluation['ratios']['tok_s']:.4f}",
            f"- TTFT ratio: {evaluation['ratios']['ttft']:.4f}",
            f"- RSS ratio: {evaluation['ratios']['rss']:.4f}",
            f"- Reasons: {', '.join(evaluation['reasons']) or 'none'}",
            "",
        ]
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=Path(__file__).with_name("candidates.json"))
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--baseline-worktree", type=Path, required=True)
    parser.add_argument("--candidate-worktree", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--markdown", type=Path)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout-seconds", type=int, default=3600)
    args = parser.parse_args()

    candidate = _load_candidate(args.manifest, args.candidate)
    validate_candidate(candidate)
    baseline_path = args.baseline_worktree.resolve()
    candidate_path = args.candidate_worktree.resolve()
    if baseline_path == candidate_path:
        raise EvidenceError("baseline and candidate worktrees must be distinct")
    baseline_sha = git_clean_head(baseline_path)
    candidate_sha = git_clean_head(candidate_path)
    if baseline_sha != candidate["baseline_sha"]:
        raise EvidenceError(f"baseline SHA mismatch: got {baseline_sha}")
    if candidate_sha != candidate["candidate_sha"]:
        raise EvidenceError(f"candidate SHA mismatch: got {candidate_sha}")

    baseline_runs = run_side(
        candidate["command"], baseline_path, "baseline", warmups=args.warmups,
        repeats=args.repeats, timeout_seconds=args.timeout_seconds,
    )
    candidate_runs = run_side(
        candidate["command"], candidate_path, "candidate", warmups=args.warmups,
        repeats=args.repeats, timeout_seconds=args.timeout_seconds,
    )
    report = {
        "schema_version": 1,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "candidate": {"id": candidate["id"], "sha": candidate_sha, "runs": candidate_runs},
        "baseline": {"sha": baseline_sha, "runs": baseline_runs},
        "evaluation": evaluate(baseline_runs, candidate_runs, candidate["acceptance"]),
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    if args.markdown:
        args.markdown.parent.mkdir(parents=True, exist_ok=True)
        args.markdown.write_text(_markdown(report))
    print(json.dumps(report["evaluation"], sort_keys=True))
    return 0 if report["evaluation"]["decision"] == "accepted" else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except EvidenceError as error:
        print(f"evidence_error: {error}", file=__import__("sys").stderr)
        raise SystemExit(3)
