"""Aggregate S3 fixed-MTP8 versus adaptive-draft evidence."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
from typing import Mapping, Sequence

from bench.m0a.smoke_server import _write_json_atomic
from bench.m1.adaptive_draft import AdaptiveDraftController


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
BASELINE_PROFILE = "baseline-f16-fa-mtp8"
CANDIDATE_PROFILE = "adaptive-f16-fa-mtp12"


class AdaptiveDraftReportError(RuntimeError):
    pass


def _profile(report: Mapping[str, object]) -> str | None:
    value = report.get("runtime_profile")
    return value.get("name") if isinstance(value, Mapping) else None


def _telemetry(report: Mapping[str, object]) -> list[Mapping[str, object]]:
    episodes = report.get("episodes")
    if not isinstance(episodes, list):
        raise AdaptiveDraftReportError("episode telemetry is missing")
    points: list[Mapping[str, object]] = []
    for episode in episodes:
        if not isinstance(episode, Mapping) or not isinstance(episode.get("telemetry"), list):
            raise AdaptiveDraftReportError("response telemetry is missing")
        for point in episode["telemetry"]:
            if not isinstance(point, Mapping):
                raise AdaptiveDraftReportError("response telemetry is invalid")
            points.append(point)
    if not points:
        raise AdaptiveDraftReportError("response telemetry is empty")
    return points


def _speeds(report: Mapping[str, object]) -> list[float]:
    values = [float(point["decode_tokens_per_second"]) for point in _telemetry(report)]
    if any(not math.isfinite(value) or value <= 0 for value in values):
        raise AdaptiveDraftReportError("response speed is invalid")
    return values


def _percentile(values: Sequence[float], percentile: float) -> float:
    ordered = sorted(values)
    position = percentile * (len(ordered) - 1)
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def validate_adaptive_trajectory(report: Mapping[str, object]) -> list[dict[str, object]]:
    controller = AdaptiveDraftController()
    trajectory: list[dict[str, object]] = []
    for index, point in enumerate(_telemetry(report)):
        generated = point.get("draft_generated")
        accepted = point.get("draft_accepted")
        if generated is None:
            continue
        if not isinstance(generated, int) or not isinstance(accepted, int):
            raise AdaptiveDraftReportError("adaptive counters are invalid")
        expected = controller.observe(generated=generated, accepted=accepted)
        actual = (
            point.get("adaptive_draft_n"),
            point.get("adaptive_draft_n_next"),
            point.get("adaptive_acceptance"),
            point.get("adaptive_acceptance_ewma"),
        )
        if any(value is None for value in actual):
            raise AdaptiveDraftReportError("adaptive trajectory is incomplete")
        if (
            int(actual[0]) != expected.used
            or int(actual[1]) != expected.next
            or not math.isclose(float(actual[2]), expected.acceptance, abs_tol=1e-5)
            or not math.isclose(float(actual[3]), expected.ewma, abs_tol=1e-5)
        ):
            raise AdaptiveDraftReportError(f"controller mismatch at response {index}")
        trajectory.append(
            {
                "response": index,
                "used": expected.used,
                "next": expected.next,
                "acceptance": float(actual[2]),
                "ewma": float(actual[3]),
            }
        )
    if not trajectory:
        raise AdaptiveDraftReportError("adaptive trajectory is empty")
    return trajectory


def evaluate_adaptive_draft(
    pairs: Sequence[tuple[str, Mapping[str, object], Mapping[str, object]]],
    qualification: Mapping[str, object],
) -> dict[str, object]:
    if not pairs:
        raise AdaptiveDraftReportError("at least one corpus pair is required")
    pair_reports: list[dict[str, object]] = []
    baseline_speeds: list[float] = []
    candidate_speeds: list[float] = []
    b_generated = b_accepted = c_generated = c_accepted = 0
    quality_passed = True
    for label, baseline, candidate in pairs:
        if _profile(baseline) != BASELINE_PROFILE or _profile(candidate) != CANDIDATE_PROFILE:
            raise AdaptiveDraftReportError(f"runtime profile mismatch for {label}")
        if baseline.get("model_sha256") != candidate.get("model_sha256"):
            raise AdaptiveDraftReportError(f"model identity mismatch for {label}")
        if baseline.get("manifest") != candidate.get("manifest"):
            raise AdaptiveDraftReportError(f"corpus identity mismatch for {label}")
        if baseline.get("m0a_admission_eligible") is not False or candidate.get("m0a_admission_eligible") is not False:
            raise AdaptiveDraftReportError(f"M0a isolation missing for {label}")
        b_ids = {episode.get("episode_id") for episode in baseline["episodes"]}
        c_ids = {episode.get("episode_id") for episode in candidate["episodes"]}
        if b_ids != c_ids or None in b_ids:
            raise AdaptiveDraftReportError(f"episode identity mismatch for {label}")
        trajectory = validate_adaptive_trajectory(candidate)
        b_speed = _speeds(baseline)
        c_speed = _speeds(candidate)
        b_perf = baseline.get("performance_decision")
        c_perf = candidate.get("performance_decision")
        b_quality = baseline.get("quality_decision")
        c_quality = candidate.get("quality_decision")
        if not all(isinstance(value, Mapping) for value in (b_perf, c_perf, b_quality, c_quality)):
            raise AdaptiveDraftReportError(f"decisions missing for {label}")
        passed = b_quality.get("passed") is True and c_quality.get("passed") is True
        quality_passed = quality_passed and passed
        b_mean = float(b_perf["mean_decode_tokens_per_second"])
        c_mean = float(c_perf["mean_decode_tokens_per_second"])
        bg, ba = int(baseline["draft_generated"]), int(baseline["draft_accepted"])
        cg, ca = int(candidate["draft_generated"]), int(candidate["draft_accepted"])
        if not (bg > 0 and 0 <= ba <= bg and cg > 0 and 0 <= ca <= cg):
            raise AdaptiveDraftReportError(f"acceptance counters invalid for {label}")
        pair_reports.append(
            {
                "label": label,
                "manifest": baseline["manifest"],
                "quality_passed": passed,
                "baseline_mean_tokens_per_second": b_mean,
                "candidate_mean_tokens_per_second": c_mean,
                "mean_speed_delta_percent": 100.0 * (c_mean - b_mean) / b_mean,
                "baseline_p10_tokens_per_second": _percentile(b_speed, 0.10),
                "candidate_p10_tokens_per_second": _percentile(c_speed, 0.10),
                "baseline_acceptance": ba / bg,
                "candidate_acceptance": ca / cg,
                "acceptance_gain_percent": 100.0 * ((ca / cg) / (ba / bg) - 1.0),
                "baseline_peak_rss_kb": baseline["peak_rss_kb"],
                "candidate_peak_rss_kb": candidate["peak_rss_kb"],
                "trajectory": trajectory,
            }
        )
        baseline_speeds.extend(b_speed)
        candidate_speeds.extend(c_speed)
        b_generated += bg
        b_accepted += ba
        c_generated += cg
        c_accepted += ca
    mean_delta = math.fsum(float(pair["mean_speed_delta_percent"]) for pair in pair_reports) / len(pair_reports)
    b_p10 = _percentile(baseline_speeds, 0.10)
    c_p10 = _percentile(candidate_speeds, 0.10)
    qualification_profile = qualification.get("runtime_profile")
    if (
        not isinstance(qualification_profile, Mapping)
        or qualification_profile.get("name") != CANDIDATE_PROFILE
        or qualification.get("model_sha256") != pairs[0][1]["model_sha256"]
    ):
        raise AdaptiveDraftReportError("adaptive qualification identity mismatch")
    qualification_points = qualification.get("points")
    if not isinstance(qualification_points, list):
        raise AdaptiveDraftReportError("adaptive qualification points are missing")
    point_64k = next(
        (
            point
            for point in qualification_points
            if isinstance(point, Mapping) and point.get("context_tokens") in {64000, 65536}
        ),
        None,
    )
    if point_64k is None:
        raise AdaptiveDraftReportError("adaptive 64K qualification point is missing")
    qualification_64k = float(point_64k["decode_tokens_per_second"])
    corpus_gates = [
        pair["acceptance_gain_percent"] >= 5.0
        and pair["candidate_p10_tokens_per_second"] >= pair["baseline_p10_tokens_per_second"]
        for pair in pair_reports
    ]
    recommended = quality_passed and all(corpus_gates) and qualification_64k >= 26.5
    return {
        "schema": 1,
        "model_sha256": pairs[0][1]["model_sha256"],
        "pairs": pair_reports,
        "aggregate": {
            "baseline_acceptance": b_accepted / b_generated,
            "candidate_acceptance": c_accepted / c_generated,
            "mean_speed_delta_percent": mean_delta,
            "baseline_p10_tokens_per_second": b_p10,
            "candidate_p10_tokens_per_second": c_p10,
        },
        "quality_gate_passed": quality_passed,
        "qualification_64k_tokens_per_second": qualification_64k,
        "corpus_gates_passed": corpus_gates,
        "recommendation_gate": {
            "minimum_acceptance_gain_percent_per_corpus": 5.0,
            "require_non_regressing_p10_per_corpus": True,
            "minimum_64k_tokens_per_second": 26.5,
        },
        "decision": "ADAPTIVE_DRAFT_RECOMMENDED" if recommended else "ADAPTIVE_DRAFT_REJECTED",
        "m0a_admitted_tokens": 0,
    }


def _load(path: Path) -> dict[str, object]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise AdaptiveDraftReportError(f"evidence is not an object: {path}")
    return value


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def append_results(path: Path, report: Mapping[str, object], evidence_hash: str) -> None:
    marker = f"S3 evidence SHA-256: `{evidence_hash}`"
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    if marker in existing:
        raise AdaptiveDraftReportError("S3 evidence is already present in RESULTS")
    lines = [
        f"\n## Track S3 adaptive draft A/B — {datetime.now(timezone.utc).isoformat()}\n",
        f"- {marker}",
        f"- Model SHA-256: `{report['model_sha256']}`; llama.cpp fork: `{report['llama_cpp_commit']}`",
        "- Controller: response EWMA alpha 0.25; start 8; bounds 4–12; decrease below 70%, increase above 85%; opt-in only",
    ]
    for pair in report["pairs"]:
        trajectory = pair["trajectory"]
        values = [point["used"] for point in trajectory]
        lines.append(
            f"- {pair['label']}: quality **{'PASS' if pair['quality_passed'] else 'FAIL'}**, "
            f"acceptance {100 * pair['baseline_acceptance']:.2f}% -> {100 * pair['candidate_acceptance']:.2f}%, "
            f"relative acceptance gain {pair['acceptance_gain_percent']:+.2f}%, "
            f"mean {pair['baseline_mean_tokens_per_second']:.4f} -> {pair['candidate_mean_tokens_per_second']:.4f} tok/s "
            f"({pair['mean_speed_delta_percent']:+.2f}%), P10 {pair['baseline_p10_tokens_per_second']:.4f} -> "
            f"{pair['candidate_p10_tokens_per_second']:.4f} tok/s, RSS {pair['baseline_peak_rss_kb']:,} -> "
            f"{pair['candidate_peak_rss_kb']:,} KiB; trajectory n={len(values)}, range {min(values)}–{max(values)}, final {values[-1]}"
        )
    aggregate = report["aggregate"]
    lines.extend(
        [
            f"- Aggregate: acceptance {100 * aggregate['baseline_acceptance']:.2f}% -> {100 * aggregate['candidate_acceptance']:.2f}%, "
            f"mean corpus speed delta {aggregate['mean_speed_delta_percent']:+.2f}%, combined P10 "
            f"{aggregate['baseline_p10_tokens_per_second']:.4f} -> {aggregate['candidate_p10_tokens_per_second']:.4f} tok/s",
            f"- Adaptive qualification at 64K: {report['qualification_64k_tokens_per_second']:.4f} tok/s "
            "(required >=26.5)",
            f"- Decision: **{report['decision']}**; 0 M0a-admitted tokens",
            "",
        ]
    )
    with path.open("a", encoding="utf-8") as stream:
        stream.write("\n".join(lines))
        stream.flush()
        os.fsync(stream.fileno())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("core-baseline", "core-candidate", "batch4-baseline", "batch4-candidate", "qualification"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    args = parser.parse_args()
    paths = (args.core_baseline, args.core_candidate, args.batch4_baseline, args.batch4_candidate, args.qualification)
    try:
        loaded = [_load(path) for path in paths]
        report = evaluate_adaptive_draft(
            [("core", loaded[0], loaded[1]), ("batch4", loaded[2], loaded[3])],
            loaded[4],
        )
        report["source_evidence_sha256"] = {str(path.resolve()): _sha256(path) for path in paths}
        report["llama_cpp_commit"] = subprocess.check_output(
            ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"], text=True
        ).strip()
        _write_json_atomic(args.output, report)
        append_results(args.results, report, _sha256(args.output))
    except (OSError, ValueError, json.JSONDecodeError, subprocess.SubprocessError, AdaptiveDraftReportError) as error:
        print(f"adaptive draft report failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
