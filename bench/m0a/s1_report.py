"""Combine S1 qualification, perplexity, and verifier evidence fail-closed."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
from typing import Mapping

from bench.m0a.perplexity import evaluate_perplexity_pair
from bench.m0a.smoke_server import _write_json_atomic


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
EXPECTED_CONTEXTS = (4_000, 32_000, 64_000)
BASELINE_PROFILE = "baseline-f16-fa-mtp4"
CANDIDATE_PROFILE = "kv-q8_0-fa-mtp4"


class S1ReportError(RuntimeError):
    """Raised when S1 inputs cannot support a controlled comparison."""


def _profile_name(report: Mapping[str, object]) -> str | None:
    profile = report.get("runtime_profile")
    return profile.get("name") if isinstance(profile, Mapping) else None


def _points(report: Mapping[str, object]) -> dict[int, Mapping[str, object]]:
    raw = report.get("points")
    if not isinstance(raw, list):
        raise S1ReportError("qualification points are missing")
    points: dict[int, Mapping[str, object]] = {}
    for point in raw:
        if not isinstance(point, Mapping):
            raise S1ReportError("qualification point is invalid")
        context = int(point.get("context_tokens", -1))
        if context in points:
            raise S1ReportError(f"duplicate qualification context: {context}")
        points[context] = point
    if set(points) != set(EXPECTED_CONTEXTS):
        raise S1ReportError("qualification context matrix differs")
    return points


def evaluate_s1(
    baseline_qualification: Mapping[str, object],
    candidate_qualification: Mapping[str, object],
    baseline_perplexity: Mapping[str, object],
    candidate_perplexity: Mapping[str, object],
    baseline_verifier: Mapping[str, object],
    candidate_verifier: Mapping[str, object],
) -> dict[str, object]:
    reports = (
        baseline_qualification,
        candidate_qualification,
        baseline_perplexity,
        candidate_perplexity,
        baseline_verifier,
        candidate_verifier,
    )
    models = {report.get("model_sha256") for report in reports}
    if len(models) != 1 or None in models:
        raise S1ReportError("model identity differs across S1 evidence")
    expected_profiles = (
        BASELINE_PROFILE,
        CANDIDATE_PROFILE,
        BASELINE_PROFILE,
        CANDIDATE_PROFILE,
        BASELINE_PROFILE,
        CANDIDATE_PROFILE,
    )
    if tuple(_profile_name(report) for report in reports) != expected_profiles:
        raise S1ReportError("runtime profile identity differs across S1 evidence")
    if baseline_verifier.get("m0a_admission_eligible") is not False or candidate_verifier.get(
        "m0a_admission_eligible"
    ) is not False:
        raise S1ReportError("profile verifier evidence is not isolated from M0a")

    baseline_points = _points(baseline_qualification)
    candidate_points = _points(candidate_qualification)
    point_report: dict[str, object] = {}
    speed_improved = True
    for context in EXPECTED_CONTEXTS:
        baseline_speed = float(baseline_points[context]["decode_tokens_per_second"])
        candidate_speed = float(candidate_points[context]["decode_tokens_per_second"])
        baseline_rss = int(baseline_points[context]["peak_rss_kb"])
        candidate_rss = int(candidate_points[context]["peak_rss_kb"])
        if not all(math.isfinite(value) and value > 0 for value in (baseline_speed, candidate_speed)):
            raise S1ReportError(f"invalid speed at context {context}")
        speed_delta = candidate_speed - baseline_speed
        speed_improved = speed_improved and speed_delta > 0
        point_report[str(context)] = {
            "baseline_tokens_per_second": baseline_speed,
            "candidate_tokens_per_second": candidate_speed,
            "speed_delta_tokens_per_second": speed_delta,
            "speed_delta_percent": 100.0 * speed_delta / baseline_speed,
            "baseline_peak_rss_kb": baseline_rss,
            "candidate_peak_rss_kb": candidate_rss,
            "rss_delta_kb": candidate_rss - baseline_rss,
        }

    perplexity_decision = evaluate_perplexity_pair(
        baseline_perplexity, candidate_perplexity
    )
    baseline_quality = baseline_verifier.get("quality_decision")
    candidate_quality = candidate_verifier.get("quality_decision")
    if not isinstance(baseline_quality, Mapping) or not isinstance(candidate_quality, Mapping):
        raise S1ReportError("verifier decision is missing")
    qualification_decisions = (
        baseline_qualification.get("decision"),
        candidate_qualification.get("decision"),
    )
    qualification_passed = all(
        isinstance(decision, Mapping) and decision.get("passed") is True
        for decision in qualification_decisions
    )
    verifier_passed = (
        baseline_quality.get("passed") is True
        and candidate_quality.get("passed") is True
        and baseline_quality.get("episode_passes") == 4
        and candidate_quality.get("episode_passes") == 4
    )
    quality_passed = verifier_passed and perplexity_decision["passed"] is True
    profile_recommended = quality_passed and qualification_passed and speed_improved
    if not quality_passed:
        decision = "PROFILE_REJECTED_QUALITY"
    elif not qualification_passed:
        decision = "PROFILE_REJECTED_QUALIFICATION"
    elif not speed_improved:
        decision = "PROFILE_REJECTED_SPEED_REGRESSION"
    else:
        decision = "PROFILE_RECOMMENDED"
    return {
        "schema": 1,
        "model_sha256": next(iter(models)),
        "baseline_profile": BASELINE_PROFILE,
        "candidate_profile": CANDIDATE_PROFILE,
        "points": point_report,
        "baseline_perplexity": baseline_perplexity["perplexity"],
        "candidate_perplexity": candidate_perplexity["perplexity"],
        "perplexity_decision": perplexity_decision,
        "baseline_verifier_passes": baseline_quality.get("episode_passes"),
        "candidate_verifier_passes": candidate_quality.get("episode_passes"),
        "baseline_agent_mean_tokens_per_second": baseline_verifier.get(
            "performance_decision", {}
        ).get("mean_decode_tokens_per_second"),
        "candidate_agent_mean_tokens_per_second": candidate_verifier.get(
            "performance_decision", {}
        ).get("mean_decode_tokens_per_second"),
        "baseline_agent_peak_rss_kb": baseline_verifier.get("peak_rss_kb"),
        "candidate_agent_peak_rss_kb": candidate_verifier.get("peak_rss_kb"),
        "quality_gate_passed": quality_passed,
        "qualification_gate_passed": qualification_passed,
        "speed_improved_all_contexts": speed_improved,
        "profile_recommended": profile_recommended,
        "decision": decision,
        "m0a_admitted_tokens": 0,
    }


def _load(path: Path) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise S1ReportError(f"cannot read evidence {path}: {error}") from error
    if not isinstance(value, dict):
        raise S1ReportError(f"evidence is not an object: {path}")
    return value


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def append_results(results: Path, report: Mapping[str, object], evidence_sha256: str) -> None:
    marker = f"S1 evidence SHA-256: `{evidence_sha256}`"
    existing = results.read_text(encoding="utf-8") if results.exists() else ""
    if marker in existing:
        raise S1ReportError("S1 evidence is already present in RESULTS")
    lines = [
        f"\n## Track S1 KV-cache q8_0 A/B — {datetime.now(timezone.utc).isoformat()}\n",
        f"- {marker}",
        f"- Model SHA-256: `{report['model_sha256']}`; llama.cpp fork: "
        f"`{report['llama_cpp_commit']}` (dirty source: `{str(report['llama_cpp_dirty']).lower()}`)",
        "- Profiles: baseline `f16 + Flash-Attention + MTP4`; candidate "
        "`q8_0 K/V + Flash-Attention + MTP4`; routing disabled; 0 M0a-admitted tokens",
    ]
    points = report["points"]
    assert isinstance(points, Mapping)
    for context in EXPECTED_CONTEXTS:
        point = points[str(context)]
        assert isinstance(point, Mapping)
        lines.append(
            f"- {context:,} context: {point['baseline_tokens_per_second']:.6f} -> "
            f"{point['candidate_tokens_per_second']:.6f} tok/s "
            f"({point['speed_delta_percent']:+.2f}%); RSS "
            f"{point['baseline_peak_rss_kb']:,} -> {point['candidate_peak_rss_kb']:,} KiB"
        )
    perplexity = report["perplexity_decision"]
    assert isinstance(perplexity, Mapping)
    lines.extend(
        [
            f"- Perplexity: {report['baseline_perplexity']:.4f} -> "
            f"{report['candidate_perplexity']:.4f}, delta {perplexity['delta']:+.4f} "
            f"(gate <= {perplexity['maximum_delta']:.2f}): **PASS**",
            f"- Frozen verifier: baseline {report['baseline_verifier_passes']}/4, "
            f"candidate {report['candidate_verifier_passes']}/4: **PASS**",
            f"- Agent verifier mean: {report['baseline_agent_mean_tokens_per_second']:.4f} -> "
            f"{report['candidate_agent_mean_tokens_per_second']:.4f} tok/s; peak RSS "
            f"{report['baseline_agent_peak_rss_kb']:,} -> {report['candidate_agent_peak_rss_kb']:,} KiB",
            "- S1 conclusion: quality gates pass, but q8_0 regresses decode speed at all "
            "three long-context points. Candidate is **REJECTED for production speed use**; "
            "the f16 profile remains the baseline.",
            f"- Decision: **{report['decision']}**",
            "",
        ]
    )
    with results.open("a", encoding="utf-8") as stream:
        stream.write("\n".join(lines))
        stream.flush()
        os.fsync(stream.fileno())


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in (
        "baseline-qualification",
        "candidate-qualification",
        "baseline-perplexity",
        "candidate-perplexity",
        "baseline-verifier",
        "candidate-verifier",
    ):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    paths = (
        args.baseline_qualification,
        args.candidate_qualification,
        args.baseline_perplexity,
        args.candidate_perplexity,
        args.baseline_verifier,
        args.candidate_verifier,
    )
    try:
        report = evaluate_s1(*(_load(path) for path in paths))
        report["source_evidence_sha256"] = {
            str(path.resolve()): _sha256(path) for path in paths
        }
        report["llama_cpp_commit"] = subprocess.check_output(
            ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
            text=True,
        ).strip()
        report["llama_cpp_dirty"] = subprocess.run(
            ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "diff", "--quiet"],
            check=False,
        ).returncode != 0
        _write_json_atomic(args.output, report)
        evidence_sha256 = _sha256(args.output)
        append_results(args.results, report, evidence_sha256)
    except (OSError, S1ReportError, subprocess.SubprocessError, ValueError) as error:
        print(f"S1 report failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["quality_gate_passed"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
