"""Consent-bound Track E cloud replay preparation and execution.

The preparation path is deliberately provider-free: it validates the frozen
offline replay and expands it into the exact twelve hash-bound requests that a
human may later consent to.  Provider execution is added behind a separate gate.
"""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
from typing import Mapping, Sequence

from bench.m0a.agentic_episode import Episode, EpisodeError, load_episodes
from bench.m0a.run_agentic_episodes import _hidden_verifier
from bench.m0a.smoke_server import _write_json_atomic
from bench.m1.provider_cli import (
    ConsentGrant,
    ProviderCliError,
    Runner,
    build_invocation,
    execute_invocation,
    probe_cli,
)


EXPECTED_INCIDENTS = (
    "f3aa0df2-e154-40f2-860e-ddb65a0b640b",
    "4a6de515-a4a4-4d60-9744-0a00ed80cbd1",
    "bee01b9a-e425-45aa-a000-b19cf947409d",
)
PROVIDERS = ("claude", "codex")
VARIANTS = ("without_diff", "with_diff")
HEX64 = re.compile(r"^[0-9a-f]{64}$")


class CloudReplayError(RuntimeError):
    """Raised before any provider launch when replay evidence is unsafe."""


@dataclass(frozen=True)
class PreparedWorktree:
    repository: Path
    worktree: Path
    baseline_commit: str


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _regular_file(path: Path, label: str) -> Path:
    if not path.is_absolute() or path.is_symlink() or not path.is_file():
        raise CloudReplayError(f"{label} must be an absolute regular non-symlink file")
    return path


def _json_object(path: Path, label: str) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise CloudReplayError(f"{label} must be a regular non-symlink file")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CloudReplayError(f"cannot read {label}: {error}") from error
    if not isinstance(value, dict):
        raise CloudReplayError(f"{label} must be a JSON object")
    return value


def _report_episode_map(report: Mapping[str, object]) -> dict[str, Episode]:
    manifest = report.get("manifest")
    if not isinstance(manifest, str):
        raise CloudReplayError("source report manifest is missing")
    try:
        episodes = load_episodes(Path(manifest))
    except EpisodeError as error:
        raise CloudReplayError(f"source report manifest is invalid: {error}") from error
    return {episode.episode_id: episode for episode in episodes}


def _request_id(identity: Mapping[str, object]) -> str:
    encoded = json.dumps(
        identity, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _git(argv: tuple[str, ...], *, cwd: Path) -> str:
    try:
        result = subprocess.run(
            argv,
            cwd=cwd,
            input="",
            text=True,
            capture_output=True,
            check=False,
            timeout=60,
            shell=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise CloudReplayError(f"Git command could not run: {error}") from error
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise CloudReplayError(f"Git command failed: {detail}")
    # Preserve leading spaces: porcelain status uses its first two columns as
    # semantic state. Only terminal newlines are transport noise.
    return result.stdout.rstrip("\r\n")


def _reject_tree_symlinks(root: Path) -> None:
    if root.is_symlink() or not root.is_dir():
        raise CloudReplayError("episode fixture must be a real directory")
    for path in root.rglob("*"):
        if path.is_symlink():
            raise CloudReplayError(f"episode fixture contains a symlink: {path.name}")


def prepare_linked_worktree(
    episode: Episode, attempt_root: Path
) -> PreparedWorktree:
    """Copy an immutable fixture, commit it, and create one linked worktree."""

    _reject_tree_symlinks(episode.fixture_dir)
    attempt_root = attempt_root.resolve()
    try:
        attempt_root.mkdir(parents=True, exist_ok=False, mode=0o700)
    except FileExistsError as error:
        raise CloudReplayError("cloud replay attempt directory already exists") from error
    repository = attempt_root / "repository"
    worktree = attempt_root / "worktree"
    try:
        shutil.copytree(episode.fixture_dir, repository, symlinks=False)
        _git(("git", "init", "--quiet"), cwd=repository)
        _git(("git", "config", "user.name", "Peregrine Replay"), cwd=repository)
        _git(
            ("git", "config", "user.email", "peregrine-replay@localhost"),
            cwd=repository,
        )
        _git(("git", "add", "--all"), cwd=repository)
        _git(
            (
                "git",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "Peregrine immutable episode baseline",
            ),
            cwd=repository,
        )
        baseline = _git(("git", "rev-parse", "HEAD"), cwd=repository)
        if len(baseline) not in (40, 64) or any(
            character not in "0123456789abcdef" for character in baseline
        ):
            raise CloudReplayError("Git baseline commit identity is invalid")
        _git(
            ("git", "worktree", "add", "--quiet", "--detach", str(worktree), baseline),
            cwd=repository,
        )
        marker = worktree / ".git"
        if marker.is_symlink() or not marker.is_file():
            raise CloudReplayError("Git did not create a linked worktree marker")
        return PreparedWorktree(repository, worktree, baseline)
    except Exception:
        shutil.rmtree(attempt_root, ignore_errors=True)
        raise


def _structured_payload(value: object) -> dict[str, str]:
    if not isinstance(value, Mapping) or set(value) != {
        "patch",
        "local_failure_cause",
        "generalizable_lesson",
    }:
        raise CloudReplayError("provider structured response has invalid fields")
    result: dict[str, str] = {}
    for name in ("patch", "local_failure_cause", "generalizable_lesson"):
        item = value.get(name)
        if not isinstance(item, str) or not item.strip():
            raise CloudReplayError("provider structured response has empty fields")
        result[name] = item
    return result


def parse_provider_output(provider: str, stdout: str) -> dict[str, str]:
    """Extract the schema-bound response from an official CLI envelope."""

    if provider == "claude":
        try:
            envelope = json.loads(stdout)
        except json.JSONDecodeError as error:
            raise CloudReplayError("Claude output is not one JSON object") from error
        if not isinstance(envelope, Mapping) or envelope.get("type") != "result":
            raise CloudReplayError("Claude output is not a result envelope")
        return _structured_payload(envelope.get("structured_output"))
    if provider == "codex":
        messages: list[object] = []
        for line in stdout.splitlines():
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError as error:
                raise CloudReplayError("Codex output contains invalid JSONL") from error
            if not isinstance(event, Mapping):
                raise CloudReplayError("Codex JSONL event is not an object")
            item = event.get("item")
            if (
                event.get("type") == "item.completed"
                and isinstance(item, Mapping)
                and item.get("type") == "agent_message"
            ):
                messages.append(item.get("text"))
        if len(messages) != 1 or not isinstance(messages[0], str):
            raise CloudReplayError("Codex JSONL has no unique final agent message")
        try:
            payload = json.loads(messages[0])
        except json.JSONDecodeError as error:
            raise CloudReplayError("Codex final message is not structured JSON") from error
        return _structured_payload(payload)
    raise CloudReplayError(f"unsupported provider output: {provider}")


def _text_sha256(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def _write_json_exclusive(path: Path, value: object) -> None:
    data = json.dumps(value, indent=2, sort_keys=True).encode("utf-8") + b"\n"
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as error:
        raise CloudReplayError(f"refusing to replace attempt artifact: {path.name}") from error
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        path.unlink(missing_ok=True)
        raise


def _validate_attempt_identity(
    request: Mapping[str, object], episode: Episode
) -> tuple[str, str, str, Path]:
    provider = request.get("provider")
    incident = request.get("incident_id")
    request_id = request.get("request_id")
    briefing_sha = request.get("briefing_sha256")
    briefing_value = request.get("briefing_path")
    if (
        provider not in PROVIDERS
        or not isinstance(incident, str)
        or incident not in EXPECTED_INCIDENTS
        or not isinstance(request_id, str)
        or HEX64.fullmatch(request_id) is None
        or not isinstance(briefing_sha, str)
        or HEX64.fullmatch(briefing_sha) is None
        or not isinstance(briefing_value, str)
        or request.get("episode_id") != episode.episode_id
        or request.get("task_sha256") != episode.task_sha256
    ):
        raise CloudReplayError("cloud attempt request identity is invalid")
    identity = {key: request[key] for key in request if key != "request_id"}
    if _request_id(identity) != request_id:
        raise CloudReplayError("cloud attempt request ID does not match its identity")
    briefing_path = _regular_file(Path(briefing_value), "briefing")
    if _sha256(briefing_path) != briefing_sha:
        raise CloudReplayError("briefing SHA changed after consent preparation")
    return str(provider), incident, briefing_sha, briefing_path


def execute_cloud_attempt(
    request: Mapping[str, object],
    episode: Episode,
    consent: ConsentGrant,
    *,
    attempt_root: Path,
    receipt_root: Path,
    runner: Runner = subprocess.run,
    executable: str | None = None,
) -> dict[str, object]:
    """Consume one exact grant, execute once, and run the hidden verifier."""

    provider, incident, briefing_sha, briefing_path = _validate_attempt_identity(
        request, episode
    )
    briefing = briefing_path.read_text(encoding="utf-8")
    prepared = prepare_linked_worktree(episode, attempt_root)
    invocation = build_invocation(
        provider,
        incident_id=incident,
        worktree=prepared.worktree,
        briefing=briefing,
        executable=executable,
    )
    if invocation.briefing_sha256 != briefing_sha:
        raise CloudReplayError("provider invocation briefing hash drifted")
    try:
        completed = execute_invocation(
            invocation,
            consent,
            receipt_root=receipt_root,
            runner=runner,
            timeout_seconds=episode.wall_timeout_seconds,
        )
    except ProviderCliError:
        raise

    reasons: list[str] = []
    structured: dict[str, str] | None = None
    if completed.returncode != 0:
        reasons.append("provider-exit")
    else:
        try:
            structured = parse_provider_output(provider, completed.stdout)
        except CloudReplayError:
            reasons.append("structured-output")

    changed = _git(("git", "diff", "--name-only"), cwd=prepared.worktree).splitlines()
    writable = {str(path) for path in episode.writable_paths}
    allowed_artifacts = {invocation.instruction_filename, invocation.schema_filename}
    status_paths: list[str] = []
    status = _git(
        ("git", "status", "--porcelain", "--untracked-files=all"),
        cwd=prepared.worktree,
    )
    for line in status.splitlines():
        if len(line) < 4:
            reasons.append("malformed-git-status")
            continue
        path = line[3:]
        if " -> " in path:
            path = path.split(" -> ", 1)[1]
        status_paths.append(path)
    outside = sorted(
        path
        for path in status_paths
        if path not in writable and path not in allowed_artifacts
    )
    if outside:
        reasons.append("out-of-scope-change")
    diff = _git(
        ("git", "diff", "--", *(str(path) for path in episode.writable_paths)),
        cwd=prepared.worktree,
    )
    if not diff.strip():
        reasons.append("no-writable-diff")
    verifier = _hidden_verifier(
        episode, prepared.worktree, timeout=episode.wall_timeout_seconds
    )
    if verifier["exit_code"] != 0:
        reasons.append("hidden-verifier")
    response = structured or {
        "patch": "",
        "local_failure_cause": "",
        "generalizable_lesson": "",
    }
    result: dict[str, object] = {
        "schema": 1,
        "request_id": request["request_id"],
        "incident_id": incident,
        "episode_id": episode.episode_id,
        "provider": provider,
        "variant": request["variant"],
        "briefing_sha256": briefing_sha,
        "consent_grant_id": consent.grant_id,
        "baseline_commit": prepared.baseline_commit,
        "origin": f"cloud:{provider}",
        "provider_returncode": completed.returncode,
        "provider_stdout_sha256": _text_sha256(completed.stdout),
        "provider_stderr_sha256": _text_sha256(completed.stderr),
        "response_patch_sha256": _text_sha256(response["patch"]),
        "local_failure_cause": response["local_failure_cause"],
        "generalizable_lesson": response["generalizable_lesson"],
        "writable_diff_sha256": _text_sha256(diff),
        "changed_tracked_paths": changed,
        "changed_worktree_paths": status_paths,
        "hidden_verifier_command": list(episode.hidden_verifier),
        "hidden_verifier_exit_code": verifier["exit_code"],
        "hidden_verifier_stdout": verifier["stdout"],
        "hidden_verifier_stderr": verifier["stderr"],
        "reasons": list(dict.fromkeys(reasons)),
        "passed": not reasons,
        "m0a_admitted_tokens": 0,
    }
    _write_json_exclusive(attempt_root / "result.json", result)
    return result


def load_consent_grants(
    requests: Sequence[Mapping[str, object]], consent_dir: Path
) -> dict[str, ConsentGrant]:
    if consent_dir.is_symlink() or not consent_dir.is_dir():
        raise CloudReplayError("consent directory must be a real directory")
    paths = sorted(consent_dir.glob("*.json"))
    if len(paths) != 12 or any(path.is_symlink() or not path.is_file() for path in paths):
        raise CloudReplayError("complete twelve-file consent set is required")
    by_identity: dict[tuple[str, str, str], ConsentGrant] = {}
    for path in paths:
        value = _json_object(path, "consent grant")
        try:
            grant = ConsentGrant(**value)
            grant.validate()
        except (TypeError, ProviderCliError) as error:
            raise CloudReplayError(f"consent grant is invalid: {error}") from error
        if path.stem != grant.grant_id:
            raise CloudReplayError("consent grant filename differs from grant ID")
        key = (grant.incident_id, grant.provider, grant.briefing_sha256)
        if key in by_identity:
            raise CloudReplayError("duplicate consent grant identity")
        by_identity[key] = grant
    result: dict[str, ConsentGrant] = {}
    for request in requests:
        try:
            request_id = str(request["request_id"])
            key = (
                str(request["incident_id"]),
                str(request["provider"]),
                str(request["briefing_sha256"]),
            )
        except KeyError as error:
            raise CloudReplayError("consent request identity is incomplete") from error
        grant = by_identity.get(key)
        if grant is None:
            raise CloudReplayError("complete matching consent set is required")
        result[request_id] = grant
    if len(result) != 12 or len(by_identity) != 12:
        raise CloudReplayError("complete exact consent matrix is required")
    return result


def evaluate_cloud_attempts(
    attempts: Sequence[Mapping[str, object]],
) -> dict[str, object]:
    if len(attempts) != 12:
        raise CloudReplayError("E-A3 requires exactly twelve attempt records")
    expected_matrix = {
        (incident, variant, provider)
        for incident in EXPECTED_INCIDENTS
        for variant in VARIANTS
        for provider in PROVIDERS
    }
    actual_matrix: set[tuple[str, str, str]] = set()
    request_ids: set[str] = set()
    passed_by_incident = {incident: False for incident in EXPECTED_INCIDENTS}
    for attempt in attempts:
        incident = attempt.get("incident_id")
        provider = attempt.get("provider")
        variant = attempt.get("variant")
        request_id = attempt.get("request_id")
        grant_id = attempt.get("consent_grant_id")
        if (
            attempt.get("schema") != 1
            or not isinstance(incident, str)
            or not isinstance(provider, str)
            or not isinstance(variant, str)
            or not isinstance(request_id, str)
            or HEX64.fullmatch(request_id) is None
            or not isinstance(grant_id, str)
            or HEX64.fullmatch(grant_id) is None
            or type(attempt.get("passed")) is not bool
            or type(attempt.get("hidden_verifier_exit_code")) is not int
        ):
            raise CloudReplayError("attempt ledger record is malformed")
        key = (incident, variant, provider)
        actual_matrix.add(key)
        request_ids.add(request_id)
        if (
            attempt.get("origin") != f"cloud:{provider}"
            or attempt.get("m0a_admitted_tokens") != 0
        ):
            raise CloudReplayError("attempt provenance invariant failed")
        if attempt["passed"] and attempt["hidden_verifier_exit_code"] != 0:
            raise CloudReplayError("attempt pass contradicts hidden verifier")
        if attempt["passed"] and incident in passed_by_incident:
            passed_by_incident[incident] = True
    if actual_matrix != expected_matrix or len(request_ids) != 12:
        raise CloudReplayError("attempt matrix is incomplete or duplicated")
    passed_incidents = sum(passed_by_incident.values())
    return {
        "schema": 1,
        "attempt_count": 12,
        "incident_count": 3,
        "passed_incidents": passed_incidents,
        "incident_results": [
            {"incident_id": incident, "passed": passed_by_incident[incident]}
            for incident in EXPECTED_INCIDENTS
        ],
        "ledger_complete": True,
        "provider_invocations": 12,
        "m0a_admitted_tokens": 0,
        "decision": "E_A3_PASS" if passed_incidents >= 2 else "E_A3_FAIL",
    }


def _load_consent_request(path: Path) -> dict[str, object]:
    stored = _json_object(path, "consent request")
    offline_value = stored.get("offline_replay_path")
    if not isinstance(offline_value, str):
        raise CloudReplayError("consent request has no offline replay identity")
    rebuilt = build_consent_request(Path(offline_value))
    if stored != rebuilt:
        raise CloudReplayError("consent request differs from rebuilt source evidence")
    return stored


def run_cloud_replays(
    consent_request_path: Path,
    consent_dir: Path,
    output_dir: Path,
    *,
    executables: Mapping[str, str] | None = None,
) -> dict[str, object]:
    request_report = _load_consent_request(consent_request_path)
    requests = request_report.get("requests")
    if not isinstance(requests, list):
        raise CloudReplayError("consent request has no request matrix")
    grants = load_consent_grants(requests, consent_dir)
    binaries = dict(executables or {})
    probes = {
        provider: probe_cli(provider, executable=binaries.get(provider))
        for provider in PROVIDERS
    }
    try:
        output_dir.mkdir(parents=True, exist_ok=False, mode=0o700)
    except FileExistsError as error:
        raise CloudReplayError("cloud replay output directory already exists") from error
    attempts: list[dict[str, object]] = []
    for request in requests:
        source = _json_object(Path(str(request["source_report_path"])), "source report")
        episodes = _report_episode_map(source)
        episode = episodes.get(str(request["episode_id"]))
        if episode is None:
            raise CloudReplayError("cloud replay episode disappeared from manifest")
        request_id = str(request["request_id"])
        attempts.append(
            execute_cloud_attempt(
                request,
                episode,
                grants[request_id],
                attempt_root=output_dir / "attempts" / request_id,
                receipt_root=output_dir / "receipts",
                executable=binaries.get(str(request["provider"])),
            )
        )
    aggregate = evaluate_cloud_attempts(attempts)
    report = {
        **aggregate,
        "consent_request_sha256": _sha256(consent_request_path),
        "provider_probes": {
            provider: asdict(probe) for provider, probe in probes.items()
        },
        "attempts": attempts,
    }
    _write_json_atomic(output_dir / "cloud-replay.json", report)
    return report


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare")
    prepare.add_argument("--offline-replay", required=True, type=Path)
    prepare.add_argument("--output-dir", required=True, type=Path)
    run = commands.add_parser("run")
    run.add_argument("--consent-request", required=True, type=Path)
    run.add_argument("--consent-dir", required=True, type=Path)
    run.add_argument("--output-dir", required=True, type=Path)
    run.add_argument("--claude", default="claude")
    run.add_argument("--codex", default="codex")
    args = parser.parse_args(argv)
    try:
        if args.command == "prepare":
            report = prepare_consent_request(args.offline_replay, args.output_dir)
        else:
            report = run_cloud_replays(
                args.consent_request,
                args.consent_dir,
                args.output_dir,
                executables={"claude": args.claude, "codex": args.codex},
            )
    except (CloudReplayError, ProviderCliError, EpisodeError, OSError) as error:
        print(f"cloud replay failed: {error}")
        return 2
    print(
        json.dumps(
            {
                "decision": report["decision"],
                "provider_invocations": report["provider_invocations"],
                "m0a_admitted_tokens": report["m0a_admitted_tokens"],
            },
            sort_keys=True,
        )
    )
    return 0


def build_consent_request(offline_replay_path: Path) -> dict[str, object]:
    """Validate the six offline briefings and expand them over two providers."""

    replay = _json_object(offline_replay_path, "offline replay")
    if (
        replay.get("schema") != 1
        or replay.get("decision") != "TRACK_E_OFFLINE_FOUNDATION_PASS"
        or replay.get("source_report_count") != 3
        or replay.get("provider_invocations") != 0
        or replay.get("m0a_admitted_tokens") != 0
        or replay.get("consent_gate") != "required"
    ):
        raise CloudReplayError("offline replay invariants are not satisfied")
    source_paths = replay.get("source_reports")
    rows = replay.get("rows")
    if not isinstance(source_paths, list) or len(source_paths) != 3:
        raise CloudReplayError("offline replay must name three source reports")
    if (
        replay.get("briefing_count") != 6
        or replay.get("ledger_entry_count") != 6
        or not isinstance(rows, list)
        or len(rows) != 6
    ):
        raise CloudReplayError("offline replay must contain six briefing rows")

    reports: dict[str, tuple[Path, str, dict[str, Episode]]] = {}
    for raw_path in source_paths:
        if not isinstance(raw_path, str):
            raise CloudReplayError("source report path is invalid")
        path = _regular_file(Path(raw_path), "source report")
        report = _json_object(path, "source report")
        incident = report.get("session_id")
        if not isinstance(incident, str) or incident not in EXPECTED_INCIDENTS:
            raise CloudReplayError("source report incident is not in the E-A3 set")
        if incident in reports:
            raise CloudReplayError("duplicate source report incident")
        reports[incident] = (path, _sha256(path), _report_episode_map(report))
    if set(reports) != set(EXPECTED_INCIDENTS):
        raise CloudReplayError("source report incident set is incomplete")

    variants_seen: set[tuple[str, str]] = set()
    requests: list[dict[str, object]] = []
    for raw_row in rows:
        if not isinstance(raw_row, Mapping):
            raise CloudReplayError("offline briefing row is not an object")
        incident = raw_row.get("session_id")
        episode_id = raw_row.get("episode_id")
        variant = raw_row.get("variant")
        task_sha = raw_row.get("task_sha256")
        report_sha = raw_row.get("source_report_sha256")
        briefing_sha = raw_row.get("briefing_sha256")
        briefing_value = raw_row.get("briefing_path")
        if (
            not isinstance(incident, str)
            or incident not in reports
            or not isinstance(episode_id, str)
            or variant not in VARIANTS
            or not isinstance(task_sha, str)
            or HEX64.fullmatch(task_sha) is None
            or not isinstance(report_sha, str)
            or HEX64.fullmatch(report_sha) is None
            or not isinstance(briefing_sha, str)
            or HEX64.fullmatch(briefing_sha) is None
            or not isinstance(briefing_value, str)
        ):
            raise CloudReplayError("offline briefing identity is malformed")
        if (
            raw_row.get("action") != "AWAITING_CONSENT"
            or raw_row.get("consent_state") != "required"
            or raw_row.get("provider_state") != "not_invoked"
            or raw_row.get("m0a_admitted_tokens") != 0
        ):
            raise CloudReplayError("offline briefing consent state is invalid")
        key = (incident, str(variant))
        if key in variants_seen:
            raise CloudReplayError("briefing variant matrix contains a duplicate")
        variants_seen.add(key)

        source_path, actual_report_sha, episodes = reports[incident]
        if actual_report_sha != report_sha:
            raise CloudReplayError("source report SHA does not match briefing row")
        episode = episodes.get(episode_id)
        if episode is None or episode.task_sha256 != task_sha:
            raise CloudReplayError("briefing task hash differs from episode manifest")
        briefing_path = _regular_file(Path(briefing_value), "briefing")
        if _sha256(briefing_path) != briefing_sha:
            raise CloudReplayError("briefing SHA does not match payload bytes")
        for provider in PROVIDERS:
            identity: dict[str, object] = {
                "provider": provider,
                "incident_id": incident,
                "episode_id": episode_id,
                "variant": variant,
                "task_sha256": task_sha,
                "source_report_sha256": report_sha,
                "source_report_path": str(source_path),
                "briefing_sha256": briefing_sha,
                "briefing_path": str(briefing_path),
            }
            requests.append({"request_id": _request_id(identity), **identity})

    expected_matrix = {
        (incident, variant)
        for incident in EXPECTED_INCIDENTS
        for variant in VARIANTS
    }
    if variants_seen != expected_matrix:
        raise CloudReplayError("briefing variant matrix is incomplete")
    requests.sort(
        key=lambda row: (
            EXPECTED_INCIDENTS.index(str(row["incident_id"])),
            VARIANTS.index(str(row["variant"])),
            PROVIDERS.index(str(row["provider"])),
        )
    )
    if len(requests) != 12 or len({row["request_id"] for row in requests}) != 12:
        raise CloudReplayError("consent request matrix is not exactly twelve unique rows")
    return {
        "schema": 1,
        "offline_replay_path": str(offline_replay_path.resolve()),
        "offline_replay_sha256": _sha256(offline_replay_path),
        "request_count": 12,
        "requests": requests,
        "provider_invocations": 0,
        "m0a_admitted_tokens": 0,
        "consent_state": "required",
        "decision": "E_A3_AWAITING_EXACT_CONSENT",
    }


def prepare_consent_request(
    offline_replay_path: Path, output_dir: Path
) -> dict[str, object]:
    report = build_consent_request(offline_replay_path)
    try:
        output_dir.mkdir(parents=True, exist_ok=False, mode=0o700)
    except FileExistsError as error:
        raise CloudReplayError("consent output directory already exists") from error
    try:
        _write_json_atomic(output_dir / "consent-request.json", report)
    except Exception:
        try:
            output_dir.rmdir()
        except OSError:
            pass
        raise
    return report


if __name__ == "__main__":
    raise SystemExit(main())
