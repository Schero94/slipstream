"""Fail-closed H1 target selection from the admitted Batch-5 retry evidence."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
from typing import Mapping

from bench.m0a.agentic_episode import load_episodes


class HardCaseSelectionError(RuntimeError):
    pass


def _json_object(path: Path, label: str) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise HardCaseSelectionError(f"{label} must be a regular non-symlink file")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise HardCaseSelectionError(f"cannot read {label}: {error}") from error
    if not isinstance(value, dict):
        raise HardCaseSelectionError(f"{label} must be a JSON object")
    return value


def _write_exclusive(path: Path, value: Mapping[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if path.parent.is_symlink() or not path.parent.is_dir():
        raise HardCaseSelectionError("output parent must be a real directory")
    data = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as error:
        raise HardCaseSelectionError("refusing to replace hard-case manifest") from error
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        path.unlink(missing_ok=True)
        raise


def select_hard_cases(
    report_path: Path,
    manifest_path: Path,
    *,
    output_path: Path | None = None,
) -> tuple[str, str]:
    report = _json_object(report_path, "Batch-5 report")
    manifest_data = _json_object(manifest_path, "Batch-5 manifest")
    episodes = load_episodes(manifest_path)
    if report.get("schema") != 1 or manifest_data.get("schema") != 1:
        raise HardCaseSelectionError("unsupported report or manifest schema")
    report_manifest = report.get("manifest")
    if not isinstance(report_manifest, str) or Path(report_manifest).resolve() != manifest_path.resolve():
        raise HardCaseSelectionError("report manifest identity mismatch")
    if report.get("verifier_retry_enabled") is not True:
        raise HardCaseSelectionError("Batch-5 evidence did not enable the verifier retry")

    report_rows = report.get("episodes")
    if not isinstance(report_rows, list) or len(report_rows) != len(episodes):
        raise HardCaseSelectionError("report does not contain the full hard corpus")
    by_id: dict[str, dict[str, object]] = {}
    for row in report_rows:
        if not isinstance(row, dict) or not isinstance(row.get("episode_id"), str):
            raise HardCaseSelectionError("report episode is malformed")
        episode_id = str(row["episode_id"])
        if episode_id in by_id:
            raise HardCaseSelectionError("report contains duplicate episodes")
        stored = _json_object(report_path.parent / episode_id / "result.json", "episode result")
        if stored != row:
            raise HardCaseSelectionError(f"episode result differs from report: {episode_id}")
        by_id[episode_id] = row

    expected_ids = tuple(episode.episode_id for episode in episodes)
    if set(by_id) != set(expected_ids):
        raise HardCaseSelectionError("report episode set differs from the manifest")
    failures: list[str] = []
    for episode in episodes:
        row = by_id[episode.episode_id]
        if row.get("task_sha256") != episode.task_sha256:
            raise HardCaseSelectionError(f"episode task hash differs from manifest: {episode.episode_id}")
        passed = row.get("passed")
        exit_code = row.get("hidden_verifier_exit_code")
        if passed is True:
            if exit_code != 0:
                raise HardCaseSelectionError("passing episode has a red verifier")
            continue
        if passed is not False or isinstance(exit_code, bool) or not isinstance(exit_code, int) or exit_code == 0:
            raise HardCaseSelectionError("failed episode lacks red verifier evidence")
        attempts = row.get("verifier_attempts")
        if row.get("feedback_retry_used") is not True or not isinstance(attempts, list) or not attempts:
            raise HardCaseSelectionError("failed episode lacks exhausted retry evidence")
        last = attempts[-1]
        if not isinstance(last, Mapping) or last.get("exit_code") in (None, 0):
            raise HardCaseSelectionError("failed episode lacks exhausted retry evidence")
        failures.append(episode.episode_id)

    if len(failures) != 2:
        raise HardCaseSelectionError(f"H1 requires exactly two open hard cases, found {len(failures)}")
    decision = report.get("decision")
    if (
        not isinstance(decision, Mapping)
        or decision.get("episode_count") != len(episodes)
        or decision.get("episode_passes") != len(episodes) - len(failures)
        or decision.get("passed") is not False
    ):
        raise HardCaseSelectionError("aggregate Batch-5 decision differs from episode evidence")

    if output_path is not None:
        raw_episodes = manifest_data.get("episodes")
        if not isinstance(raw_episodes, list):
            raise HardCaseSelectionError("manifest episodes are malformed")
        selected_rows = [
            row
            for row in raw_episodes
            if isinstance(row, Mapping) and row.get("id") in failures
        ]
        if len(selected_rows) != 2:
            raise HardCaseSelectionError("cannot materialize the two hard cases")
        _write_exclusive(output_path, {"schema": 1, "episodes": selected_rows})
        written = load_episodes(output_path)
        if tuple(item.episode_id for item in written) != tuple(failures):
            raise HardCaseSelectionError("written hard-case manifest identity mismatch")
    return failures[0], failures[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        selected = select_hard_cases(args.report, args.manifest, output_path=args.output)
    except (HardCaseSelectionError, OSError, ValueError) as error:
        print(f"hard-case selection failed: {error}")
        return 2
    print(json.dumps({"selected": list(selected)}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
