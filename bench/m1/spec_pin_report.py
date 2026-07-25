"""Aggregate the S2 SPEC_PIN audit and frozen-corpus A/B evidence."""

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


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
BASELINE_PROFILE = "baseline-f16-fa-mtp8"
CANDIDATE_PROFILE = "spec-pin-f16-fa-mtp8"


class SpecPinReportError(RuntimeError):
    """Raised when S2 evidence is incomplete or cannot be compared."""


def _profile_name(report: Mapping[str, object]) -> str | None:
    profile = report.get("runtime_profile")
    return profile.get("name") if isinstance(profile, Mapping) else None


def _speeds(report: Mapping[str, object]) -> list[float]:
    episodes = report.get("episodes")
    if not isinstance(episodes, list):
        raise SpecPinReportError("episode telemetry is missing")
    speeds: list[float] = []
    for episode in episodes:
        if not isinstance(episode, Mapping):
            raise SpecPinReportError("episode telemetry is invalid")
        telemetry = episode.get("telemetry")
        if not isinstance(telemetry, list):
            raise SpecPinReportError("response telemetry is missing")
        for point in telemetry:
            if not isinstance(point, Mapping):
                raise SpecPinReportError("response telemetry is invalid")
            speed = float(point["decode_tokens_per_second"])
            if not math.isfinite(speed) or speed <= 0:
                raise SpecPinReportError("response speed is invalid")
            speeds.append(speed)
    if not speeds:
        raise SpecPinReportError("response telemetry is empty")
    return speeds


def _percentile(values: Sequence[float], percentile: float) -> float:
    ordered = sorted(values)
    position = percentile * (len(ordered) - 1)
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1 - fraction) + ordered[upper] * fraction


def evaluate_spec_pin(
    pairs: Sequence[tuple[str, Mapping[str, object], Mapping[str, object]]],
) -> dict[str, object]:
    if len(pairs) < 1:
        raise SpecPinReportError("at least one corpus pair is required")
    pair_reports: list[dict[str, object]] = []
    all_baseline_speeds: list[float] = []
    all_candidate_speeds: list[float] = []
    total_baseline_generated = 0
    total_baseline_accepted = 0
    total_candidate_generated = 0
    total_candidate_accepted = 0
    all_quality_passed = True

    for label, baseline, candidate in pairs:
        if _profile_name(baseline) != BASELINE_PROFILE or _profile_name(candidate) != CANDIDATE_PROFILE:
            raise SpecPinReportError(f"runtime profile mismatch for {label}")
        if baseline.get("model_sha256") != candidate.get("model_sha256"):
            raise SpecPinReportError(f"model identity mismatch for {label}")
        if baseline.get("manifest") != candidate.get("manifest"):
            raise SpecPinReportError(f"corpus identity mismatch for {label}")
        if baseline.get("m0a_admission_eligible") is not False or candidate.get(
            "m0a_admission_eligible"
        ) is not False:
            raise SpecPinReportError(f"M0a isolation missing for {label}")
        for report in (baseline, candidate):
            episodes = report.get("episodes")
            if not isinstance(episodes, list):
                raise SpecPinReportError(f"episodes missing for {label}")
        baseline_ids = {episode.get("episode_id") for episode in baseline["episodes"]}
        candidate_ids = {episode.get("episode_id") for episode in candidate["episodes"]}
        if baseline_ids != candidate_ids or None in baseline_ids:
            raise SpecPinReportError(f"episode identity mismatch for {label}")

        b_generated = int(baseline["draft_generated"])
        b_accepted = int(baseline["draft_accepted"])
        c_generated = int(candidate["draft_generated"])
        c_accepted = int(candidate["draft_accepted"])
        if not (b_generated > 0 and 0 <= b_accepted <= b_generated):
            raise SpecPinReportError(f"baseline acceptance counters invalid for {label}")
        if not (c_generated > 0 and 0 <= c_accepted <= c_generated):
            raise SpecPinReportError(f"candidate acceptance counters invalid for {label}")
        b_acceptance = b_accepted / b_generated
        c_acceptance = c_accepted / c_generated
        b_performance = baseline.get("performance_decision")
        c_performance = candidate.get("performance_decision")
        b_quality = baseline.get("quality_decision")
        c_quality = candidate.get("quality_decision")
        if not all(
            isinstance(value, Mapping)
            for value in (b_performance, c_performance, b_quality, c_quality)
        ):
            raise SpecPinReportError(f"decisions missing for {label}")
        b_mean = float(b_performance["mean_decode_tokens_per_second"])
        c_mean = float(c_performance["mean_decode_tokens_per_second"])
        b_speeds = _speeds(baseline)
        c_speeds = _speeds(candidate)
        quality_passed = b_quality.get("passed") is True and c_quality.get("passed") is True
        all_quality_passed = all_quality_passed and quality_passed
        pair_reports.append(
            {
                "label": label,
                "manifest": baseline["manifest"],
                "episode_count": len(baseline_ids),
                "quality_passed": quality_passed,
                "baseline_decoded_tokens": baseline["decoded_tokens"],
                "candidate_decoded_tokens": candidate["decoded_tokens"],
                "baseline_acceptance": b_acceptance,
                "candidate_acceptance": c_acceptance,
                "acceptance_gain_points": 100.0 * (c_acceptance - b_acceptance),
                "baseline_mean_tokens_per_second": b_mean,
                "candidate_mean_tokens_per_second": c_mean,
                "mean_speed_delta_percent": 100.0 * (c_mean - b_mean) / b_mean,
                "baseline_p10_tokens_per_second": _percentile(b_speeds, 0.10),
                "candidate_p10_tokens_per_second": _percentile(c_speeds, 0.10),
                "baseline_peak_rss_kb": baseline["peak_rss_kb"],
                "candidate_peak_rss_kb": candidate["peak_rss_kb"],
            }
        )
        all_baseline_speeds.extend(b_speeds)
        all_candidate_speeds.extend(c_speeds)
        total_baseline_generated += b_generated
        total_baseline_accepted += b_accepted
        total_candidate_generated += c_generated
        total_candidate_accepted += c_accepted

    b_acceptance = total_baseline_accepted / total_baseline_generated
    c_acceptance = total_candidate_accepted / total_candidate_generated
    speed_deltas = [float(pair["mean_speed_delta_percent"]) for pair in pair_reports]
    baseline_p10 = _percentile(all_baseline_speeds, 0.10)
    candidate_p10 = _percentile(all_candidate_speeds, 0.10)
    aggregate = {
        "baseline_draft_generated": total_baseline_generated,
        "baseline_draft_accepted": total_baseline_accepted,
        "candidate_draft_generated": total_candidate_generated,
        "candidate_draft_accepted": total_candidate_accepted,
        "baseline_acceptance": b_acceptance,
        "candidate_acceptance": c_acceptance,
        "acceptance_gain_points": 100.0 * (c_acceptance - b_acceptance),
        "mean_speed_delta_percent": math.fsum(speed_deltas) / len(speed_deltas),
        "baseline_p10_tokens_per_second": baseline_p10,
        "candidate_p10_tokens_per_second": candidate_p10,
    }
    patch_recommended = (
        all_quality_passed
        and c_acceptance >= 0.75
        and aggregate["mean_speed_delta_percent"] >= 0
        and candidate_p10 >= baseline_p10
    )
    return {
        "schema": 1,
        "model_sha256": pairs[0][1]["model_sha256"],
        "pairs": pair_reports,
        "aggregate": aggregate,
        "quality_gate_passed": all_quality_passed,
        "recommendation_gate": {
            "minimum_candidate_acceptance": 0.75,
            "minimum_mean_speed_delta_percent": 0.0,
            "require_non_regressing_p10": True,
        },
        "patch_recommended": patch_recommended,
        "decision": "SPEC_PIN_RECOMMENDED" if patch_recommended else "SPEC_PIN_REJECTED",
        "m0a_admitted_tokens": 0,
    }


def _load(path: Path) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SpecPinReportError(f"cannot read {path}: {error}") from error
    if not isinstance(value, dict):
        raise SpecPinReportError(f"evidence is not an object: {path}")
    return value


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def append_results(results: Path, report: Mapping[str, object], evidence_sha256: str) -> None:
    marker = f"S2 evidence SHA-256: `{evidence_sha256}`"
    existing = results.read_text(encoding="utf-8") if results.exists() else ""
    if marker in existing:
        raise SpecPinReportError("S2 evidence is already present in RESULTS")
    audit = report["kernel_audit"]
    aggregate = report["aggregate"]
    assert isinstance(audit, Mapping) and isinstance(aggregate, Mapping)
    lines = [
        f"\n## Track S2 SPEC_PIN audit and A/B — {datetime.now(timezone.utc).isoformat()}\n",
        f"- {marker}",
        f"- Model SHA-256: `{report['model_sha256']}`; llama.cpp fork integration: "
        f"`{report['llama_cpp_commit']}`",
        "- Provenance: kernel-family pin adapted from colibrì `c/glm.c`, PR #163 / "
        "commit `37da111`, Apache-2.0; Peregrine keeps `SPEC_PIN` off by default",
        f"- Kernel audit: {audit['record_count']:,} matrix records, "
        f"{audit['divergence_count']} dense S-dependent divergences, MoE divergence "
        f"`{str(audit['moe_divergence_observed']).lower()}`; pinned rows observed "
        f"`{audit['pinned_rows']}`",
    ]
    for pair in report["pairs"]:
        lines.append(
            f"- {pair['label']}: quality **{'PASS' if pair['quality_passed'] else 'FAIL'}**, "
            f"acceptance {100 * pair['baseline_acceptance']:.2f}% -> "
            f"{100 * pair['candidate_acceptance']:.2f}% "
            f"({pair['acceptance_gain_points']:+.2f} pp), mean "
            f"{pair['baseline_mean_tokens_per_second']:.4f} -> "
            f"{pair['candidate_mean_tokens_per_second']:.4f} tok/s "
            f"({pair['mean_speed_delta_percent']:+.2f}%), P10 "
            f"{pair['baseline_p10_tokens_per_second']:.4f} -> "
            f"{pair['candidate_p10_tokens_per_second']:.4f} tok/s"
        )
    lines.extend(
        [
            f"- Aggregate acceptance: {100 * aggregate['baseline_acceptance']:.2f}% -> "
            f"{100 * aggregate['candidate_acceptance']:.2f}% "
            f"({aggregate['acceptance_gain_points']:+.2f} pp); mean corpus speed delta "
            f"{aggregate['mean_speed_delta_percent']:+.2f}%",
            "- S2 conclusion: the full S=1 family pin is real and quality-safe on both "
            "corpora, but misses the >=75% acceptance target and regresses mean speed and "
            "tail throughput. Keep the implementation opt-in for research; retain the "
            "unpinned MTP8 profile as the S3 baseline.",
            f"- Decision: **{report['decision']}**; 0 M0a-admitted tokens",
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
        "core-baseline",
        "core-candidate",
        "batch4-baseline",
        "batch4-candidate",
        "audit-baseline",
        "audit-pinned",
    ):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    paths = (
        args.core_baseline,
        args.core_candidate,
        args.batch4_baseline,
        args.batch4_candidate,
        args.audit_baseline,
        args.audit_pinned,
    )
    try:
        core_baseline, core_candidate, batch4_baseline, batch4_candidate, audit_baseline, audit_pinned = (
            _load(path) for path in paths
        )
        report = evaluate_spec_pin(
            [
                ("core", core_baseline, core_candidate),
                ("batch4", batch4_baseline, batch4_candidate),
            ]
        )
        audit = audit_baseline.get("audit")
        if not isinstance(audit, Mapping) or audit.get("complete") is not True:
            raise SpecPinReportError("baseline kernel audit is incomplete")
        pinned_rows = audit_pinned.get("spec_pin_event_rows")
        if audit_pinned.get("spec_pin_enabled") is not True or not isinstance(pinned_rows, list):
            raise SpecPinReportError("pinned kernel audit is incomplete")
        report["kernel_audit"] = {
            "record_count": audit["record_count"],
            "divergence_count": audit["divergence_count"],
            "dense_divergence_observed": audit["dense_divergence_observed"],
            "moe_divergence_observed": audit["moe_divergence_observed"],
            "pinned_rows": pinned_rows,
        }
        report["source_evidence_sha256"] = {
            str(path.resolve()): _sha256(path) for path in paths
        }
        report["llama_cpp_commit"] = subprocess.check_output(
            ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
            text=True,
        ).strip()
        _write_json_atomic(args.output, report)
        evidence_sha256 = _sha256(args.output)
        append_results(args.results, report, evidence_sha256)
    except (OSError, SpecPinReportError, subprocess.SubprocessError, ValueError) as error:
        print(f"SPEC_PIN report failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
