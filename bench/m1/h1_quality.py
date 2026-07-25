"""Identity-bound admission decision for the Plan-v3 H1 dense fallback."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Mapping
from urllib.parse import urlparse

from bench.m0a.agentic_episode import load_episodes
from bench.m1.model_bakeoff import H1_MAX_RSS_KB
from bench.m0a.smoke_server import _write_json_atomic


OPEN_IDS = ("parse-streaming-unified-diff", "renew-bounded-worker-leases")
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost"}
REPO_ROOT = Path(__file__).resolve().parents[2]
OPEN_MANIFEST = REPO_ROOT / "bench/m0a/episodes/h1-open.json"
FULL_MANIFEST = REPO_ROOT / "bench/m0a/episodes/batch5.json"


class H1QualityError(RuntimeError):
    pass


def _json_object(path: Path, label: str) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise H1QualityError(f"{label} must be a regular non-symlink file")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise H1QualityError(f"cannot read {label}: {error}") from error
    if not isinstance(value, dict):
        raise H1QualityError(f"{label} must be a JSON object")
    return value


def _validated_report(
    path: Path,
    expected_ids: tuple[str, ...],
    expected_manifest: Path,
) -> dict[str, object]:
    report = _json_object(path, "H1 quality report")
    if report.get("schema") != 1 or report.get("verifier_retry_enabled") is not True:
        raise H1QualityError("H1 quality report schema or retry policy is invalid")
    if report.get("model") != "peregrine-h1":
        raise H1QualityError("H1 quality report model alias is invalid")
    base_url = report.get("base_url")
    if not isinstance(base_url, str) or urlparse(base_url).hostname not in LOOPBACK_HOSTS:
        raise H1QualityError("H1 quality endpoint is not loopback")
    server_pid = report.get("server_pid")
    peak_rss = report.get("peak_rss_kb")
    if (
        isinstance(server_pid, bool)
        or not isinstance(server_pid, int)
        or server_pid <= 0
        or isinstance(peak_rss, bool)
        or not isinstance(peak_rss, int)
        or peak_rss <= 0
    ):
        raise H1QualityError("H1 server or RSS evidence is invalid")
    manifest_value = report.get("manifest")
    if (
        not isinstance(manifest_value, str)
        or Path(manifest_value).resolve() != expected_manifest.resolve()
    ):
        raise H1QualityError("H1 report manifest identity is invalid")
    manifest_path = Path(manifest_value)
    episodes = load_episodes(manifest_path)
    manifest_ids = tuple(episode.episode_id for episode in episodes)
    if manifest_ids != expected_ids:
        raise H1QualityError("H1 report manifest episode set is invalid")

    rows = report.get("episodes")
    if not isinstance(rows, list) or len(rows) != len(episodes):
        raise H1QualityError("H1 report is missing episode evidence")
    by_id: dict[str, Mapping[str, object]] = {}
    for row in rows:
        if not isinstance(row, Mapping) or not isinstance(row.get("episode_id"), str):
            raise H1QualityError("H1 report episode is malformed")
        episode_id = str(row["episode_id"])
        if episode_id in by_id:
            raise H1QualityError("H1 report contains duplicate episodes")
        stored = _json_object(path.parent / episode_id / "result.json", "H1 episode result")
        if stored != row:
            raise H1QualityError(f"H1 episode result differs from report: {episode_id}")
        by_id[episode_id] = row
    if tuple(by_id) != expected_ids:
        raise H1QualityError("H1 report episode order or identity is invalid")

    passes = 0
    for episode in episodes:
        row = by_id[episode.episode_id]
        if row.get("task_sha256") != episode.task_sha256:
            raise H1QualityError("H1 episode task hash differs from manifest")
        passed = row.get("passed")
        exit_code = row.get("hidden_verifier_exit_code")
        if passed is True and exit_code == 0:
            passes += 1
            continue
        attempts = row.get("verifier_attempts")
        if (
            passed is not False
            or isinstance(exit_code, bool)
            or not isinstance(exit_code, int)
            or exit_code == 0
            or row.get("feedback_retry_used") is not True
            or not isinstance(attempts, list)
            or not attempts
            or not isinstance(attempts[-1], Mapping)
            or attempts[-1].get("exit_code") in (None, 0)
        ):
            raise H1QualityError("H1 failed episode lacks exhausted verifier evidence")
    decision = report.get("decision")
    if (
        not isinstance(decision, Mapping)
        or decision.get("episode_count") != len(episodes)
        or decision.get("episode_passes") != passes
    ):
        raise H1QualityError("H1 aggregate report differs from episode evidence")
    report["validated_passes"] = passes
    return report


def evaluate_h1_quality(
    smoke_path: Path,
    open_report_path: Path,
    full_report_path: Path,
    quality_memory_path: Path,
    *,
    expected_model_sha256: str,
) -> dict[str, object]:
    smoke = _json_object(smoke_path, "H1 smoke report")
    candidate = smoke.get("candidate")
    if (
        len(expected_model_sha256) != 64
        or not isinstance(candidate, Mapping)
        or candidate.get("sha256") != expected_model_sha256
    ):
        raise H1QualityError("H1 model identity does not match the selected artifact")
    if smoke.get("schema") != 1 or smoke.get("gate") != "h1-hard-case-v1":
        raise H1QualityError("H1 smoke gate identity is invalid")

    full_ids = tuple(episode.episode_id for episode in load_episodes(FULL_MANIFEST))
    if len(full_ids) != 6 or not set(OPEN_IDS).issubset(full_ids):
        raise H1QualityError("H1 full corpus is not the six-task hard corpus")
    open_report = _validated_report(open_report_path, OPEN_IDS, OPEN_MANIFEST)
    full_report = _validated_report(full_report_path, full_ids, FULL_MANIFEST)
    if (
        open_report["server_pid"] != full_report["server_pid"]
        or open_report["base_url"] != full_report["base_url"]
        or open_report["model"] != full_report["model"]
    ):
        raise H1QualityError("H1 quality reports do not share one local server identity")

    memory = _json_object(quality_memory_path, "H1 quality memory window")
    try:
        pageouts = int(memory["pageouts_delta"])
        swapouts = int(memory["swapouts_delta"])
        free_after = int(memory["free_percent_after"])
    except (KeyError, TypeError, ValueError) as error:
        raise H1QualityError("H1 quality memory evidence is malformed") from error
    if min(pageouts, swapouts, free_after) < 0:
        raise H1QualityError("H1 quality memory counters are invalid")

    open_passes = int(open_report["validated_passes"])
    full_passes = int(full_report["validated_passes"])
    reasons: list[str] = []
    if smoke.get("decision") != "PASS":
        reasons.append("resource-smoke")
    if open_passes < 1:
        reasons.append("open-quality")
    if full_passes < 4:
        reasons.append("full-quality")
    if max(int(open_report["peak_rss_kb"]), int(full_report["peak_rss_kb"])) > H1_MAX_RSS_KB:
        reasons.append("quality-rss")
    if pageouts != 0:
        reasons.append("quality-pageouts")
    if swapouts != 0:
        reasons.append("quality-swapouts")
    if free_after < 10:
        reasons.append("quality-memory-pressure")
    return {
        "schema": 1,
        "decision": "H1_ADMITTED" if not reasons else "H1_REJECTED",
        "reasons": reasons,
        "model_sha256": expected_model_sha256,
        "server_pid": open_report["server_pid"],
        "open_passes": open_passes,
        "open_count": 2,
        "full_passes": full_passes,
        "full_count": 6,
        "quality_peak_rss_kb": max(
            int(open_report["peak_rss_kb"]), int(full_report["peak_rss_kb"])
        ),
        "quality_memory": memory,
        "m0a_admitted_tokens": 0,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--smoke", required=True, type=Path)
    parser.add_argument("--open-report", required=True, type=Path)
    parser.add_argument("--full-report", required=True, type=Path)
    parser.add_argument("--quality-memory", required=True, type=Path)
    parser.add_argument("--expected-model-sha256", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        decision = evaluate_h1_quality(
            args.smoke,
            args.open_report,
            args.full_report,
            args.quality_memory,
            expected_model_sha256=args.expected_model_sha256,
        )
        _write_json_atomic(args.output, decision)
    except (H1QualityError, OSError, ValueError) as error:
        print(f"H1 quality evaluation failed: {error}")
        return 2
    print(json.dumps(decision, indent=2, sort_keys=True))
    return 0 if decision["decision"] == "H1_ADMITTED" else 2


if __name__ == "__main__":
    raise SystemExit(main())
