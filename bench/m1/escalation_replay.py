"""Replay rejected M0a reports through Track E without invoking a provider."""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
from pathlib import Path
import re
from typing import Mapping

from bench.m0a.agentic_episode import Episode, load_episodes
from bench.m0a.smoke_server import _write_json_atomic
from bench.m1.escalation import (
    EscalationError,
    EscalationLedger,
    LedgerEntry,
    TriggerContext,
    build_briefing,
    decide_escalation,
    detect_violations,
)


SAFE_ID = re.compile(r"^[A-Za-z0-9_.-]+$")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _json_object(path: Path, label: str) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise EscalationError(f"{label} must be a regular non-symlink file")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EscalationError(f"cannot read {label}: {error}") from error
    if not isinstance(value, dict):
        raise EscalationError(f"{label} must be a JSON object")
    return value


def _failed_episode(report: Mapping[str, object]) -> Mapping[str, object]:
    episodes = report.get("episodes")
    if not isinstance(episodes, list):
        raise EscalationError("agentic report has no episode evidence")
    candidates = [
        item
        for item in episodes
        if isinstance(item, Mapping)
        and item.get("passed") is False
        and item.get("error") == "hidden verifier failed"
        and item.get("hidden_verifier_exit_code") not in (None, 0)
    ]
    if not candidates:
        raise EscalationError("report has no replayable hidden-verifier failure")
    return candidates[0]


def _episode_identity(report: Mapping[str, object], result: Mapping[str, object]) -> Episode:
    manifest_value = report.get("manifest")
    episode_id = result.get("episode_id")
    task_hash = result.get("task_sha256")
    if not isinstance(manifest_value, str) or not isinstance(episode_id, str):
        raise EscalationError("report manifest or episode ID is missing")
    episodes = {episode.episode_id: episode for episode in load_episodes(Path(manifest_value))}
    episode = episodes.get(episode_id)
    if episode is None:
        raise EscalationError("failed episode is absent from its manifest")
    if task_hash is not None and task_hash != episode.task_sha256:
        raise EscalationError("failed episode task hash differs from manifest")
    return episode


def _step_evidence(
    episode_dir: Path,
    episode: Episode,
) -> tuple[str, tuple[str, ...]]:
    step_paths = sorted(episode_dir.glob("step-*.json"))
    if not step_paths:
        raise EscalationError("replay episode has no step evidence")
    accessed: list[str] = []
    patches: list[str] = []
    writes = 0
    for step_path in step_paths:
        step = _json_object(step_path, "episode step")
        tool = step.get("tool")
        arguments = step.get("arguments")
        if not isinstance(arguments, Mapping):
            raise EscalationError("episode step arguments are missing")
        path_value = arguments.get("path")
        if tool == "read_file" and isinstance(path_value, str):
            accessed.append(path_value)
        elif tool == "write_file":
            content = arguments.get("content")
            if not isinstance(path_value, str) or not isinstance(content, str):
                raise EscalationError("write_file evidence is incomplete")
            relative = Path(path_value)
            if relative.is_absolute() or ".." in relative.parts:
                raise EscalationError("write_file evidence path is unsafe")
            source = episode.fixture_dir / relative
            if source.is_symlink() or not source.is_file():
                raise EscalationError("write_file source fixture is unavailable")
            old = source.read_text(encoding="utf-8").splitlines(keepends=True)
            new = content.splitlines(keepends=True)
            patches.append(
                "".join(
                    difflib.unified_diff(
                        old,
                        new,
                        fromfile=f"a/{path_value}",
                        tofile=f"b/{path_value}",
                    )
                )
            )
            writes += 1
        elif tool == "apply_patch":
            patch = arguments.get("patch")
            if not isinstance(patch, str) or not patch:
                raise EscalationError("apply_patch evidence is incomplete")
            patches.append(patch)
            writes += 1
    if writes == 0 or not any(patches):
        raise EscalationError("replay episode has no materialized local diff")
    return "\n".join(patches), tuple(dict.fromkeys(accessed))


def _exclusive_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        with path.open("x", encoding="utf-8") as stream:
            stream.write(text)
            stream.flush()
    except FileExistsError as error:
        raise EscalationError(f"briefing already exists: {path.name}") from error
    path.chmod(0o600)


def replay_report(
    report_path: Path,
    briefing_root: Path,
    ledger: EscalationLedger,
) -> list[dict[str, object]]:
    report = _json_object(report_path, "agentic report")
    result = _failed_episode(report)
    episode = _episode_identity(report, result)
    session_id = report.get("session_id")
    if not isinstance(session_id, str) or SAFE_ID.fullmatch(session_id) is None:
        raise EscalationError("agentic report session ID is unsafe")
    episode_dir = report_path.parent / episode.episode_id
    stored_result = _json_object(episode_dir / "result.json", "episode result")
    for field in ("episode_id", "passed", "error", "hidden_verifier_exit_code"):
        if stored_result.get(field) != result.get(field):
            raise EscalationError(f"episode result differs from report: {field}")
    local_diff, accessed_paths = _step_evidence(episode_dir, episode)
    verifier_output = "\n".join(
        str(result.get(key, ""))
        for key in ("hidden_verifier_stdout", "hidden_verifier_stderr")
    ).strip()
    decoded_tokens = result.get("decoded_tokens")
    if isinstance(decoded_tokens, bool) or not isinstance(decoded_tokens, int) or decoded_tokens <= 0:
        raise EscalationError("failed episode decode-token count is invalid")
    context = TriggerContext(
        task_contract=episode.task,
        local_diff=local_diff,
        summary="Local attempt finished; hidden verifier failed.",
        verifier_passed=False,
        verifier_output=verifier_output,
        verifier_command=episode.hidden_verifier,
        accessed_paths=accessed_paths,
        file_pointers=tuple(str(path) for path in episode.writable_paths),
        feedback_retries=1,
        local_output_tokens=decoded_tokens,
        manual_boost=False,
    )
    findings = detect_violations(context)
    decision = decide_escalation(context, mode="ask", findings=findings)
    if decision.action != "AWAITING_CONSENT" or decision.provider_invoked:
        raise EscalationError("offline replay did not stop at consent")
    evidence_sha = _sha256(report_path)
    rows: list[dict[str, object]] = []
    for include_diff, variant in ((False, "without_diff"), (True, "with_diff")):
        briefing = build_briefing(context, findings, include_failed_diff=include_diff)
        briefing_path = briefing_root / f"{session_id}-{episode.episode_id}-{variant}.md"
        _exclusive_text(briefing_path, briefing.markdown)
        entry = LedgerEntry.create(
            task_contract=episode.task,
            local_evidence_sha256=evidence_sha,
            briefing=briefing,
            triggers=decision.triggers,
            consent_state="required",
        )
        ledger_path = ledger.append(entry)
        rows.append(
            {
                "session_id": session_id,
                "episode_id": episode.episode_id,
                "task_sha256": episode.task_sha256,
                "source_report_sha256": evidence_sha,
                "variant": variant,
                "briefing_sha256": briefing.sha256,
                "briefing_path": str(briefing_path.resolve()),
                "ledger_entry_id": entry.entry_id,
                "ledger_path": str(ledger_path.resolve()),
                "action": decision.action,
                "triggers": list(decision.triggers),
                "finding_ids": [finding.detector_id for finding in findings],
                "consent_state": entry.consent_state,
                "provider_state": entry.provider_state,
                "m0a_admitted_tokens": 0,
            }
        )
    return rows


def run_replays(report_paths: tuple[Path, ...], output_dir: Path) -> dict[str, object]:
    if len(report_paths) != 3 or len({path.resolve() for path in report_paths}) != 3:
        raise EscalationError("Track E acceptance requires three unique reports")
    try:
        output_dir.mkdir(parents=True, exist_ok=False)
    except FileExistsError as error:
        raise EscalationError("replay output directory already exists") from error
    ledger = EscalationLedger(output_dir / "ledger")
    rows: list[dict[str, object]] = []
    for report_path in report_paths:
        rows.extend(replay_report(report_path, output_dir / "briefings", ledger))
    entries = ledger.read_all()
    report: dict[str, object] = {
        "schema": 1,
        "source_report_count": len(report_paths),
        "source_reports": [str(path.resolve()) for path in report_paths],
        "briefing_count": len(rows),
        "ledger_entry_count": len(entries),
        "rows": rows,
        "provider_execution_available": False,
        "provider_invocations": 0,
        "consent_gate": "required",
        "m0a_admitted_tokens": 0,
        "decision": "TRACK_E_OFFLINE_FOUNDATION_PASS" if len(rows) == 6 else "INVALID",
    }
    _write_json_atomic(output_dir / "offline-replay.json", report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", action="append", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    args = parser.parse_args()
    result = run_replays(tuple(args.report), args.output_dir)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
