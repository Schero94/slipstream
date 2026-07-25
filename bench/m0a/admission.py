"""Admit only unique, passing agentic evidence into the M0a collection ledger."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
from uuid import UUID


class AdmissionError(RuntimeError):
    """Raised when evidence cannot safely contribute to the 200K collection."""


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise AdmissionError(f"cannot hash evidence {path}: {error}") from error
    return digest.hexdigest()


def _read_object(path: Path, label: str) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise AdmissionError(f"invalid {label} {path}: {error}") from error
    if not isinstance(value, dict):
        raise AdmissionError(f"{label} is not a JSON object: {path}")
    return value


def _hash(value: object, label: str) -> str:
    if not isinstance(value, str) or len(value) != 64 or any(
        character not in "0123456789abcdef" for character in value
    ):
        raise AdmissionError(f"{label} must be 64 lowercase hexadecimal characters")
    return value


def _session(value: object, label: str) -> str:
    try:
        return str(UUID(str(value)))
    except ValueError as error:
        raise AdmissionError(f"invalid {label}") from error


def _load_ledger(path: Path, model_sha256: str) -> dict[str, object]:
    if not path.exists():
        return {
            "schema": 1,
            "model_sha256": model_sha256,
            "entries": [],
            "session_count": 0,
            "task_count": 0,
            "admitted_output_tokens": 0,
        }
    ledger = _read_object(path, "admission ledger")
    if ledger.get("schema") != 1 or ledger.get("model_sha256") != model_sha256:
        raise AdmissionError("admission ledger schema or model hash differs")
    if not isinstance(ledger.get("entries"), list):
        raise AdmissionError("admission ledger entries are invalid")
    return ledger


def _write_json_atomic(path: Path, value: object) -> None:
    temporary = path.with_name(path.name + ".tmp")
    try:
        with temporary.open("w", encoding="utf-8") as stream:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    except OSError as error:
        raise AdmissionError(f"cannot write admission ledger {path}: {error}") from error


def _report_evidence(report: dict[str, object]) -> tuple[str, list[str], int]:
    kind = report.get("kind", "agentic")
    if kind == "agentic":
        episodes = report.get("episodes")
        if not isinstance(episodes, list) or not episodes:
            raise AdmissionError("agentic report has no episodes")
        tasks: list[str] = []
        output_tokens = 0
        for episode in episodes:
            if not isinstance(episode, dict) or episode.get("passed") is not True:
                raise AdmissionError("agentic report contains a failed episode")
            tasks.append(_hash(episode.get("task_sha256"), "task_sha256"))
            tokens = episode.get("decoded_tokens")
            if type(tokens) is not int or tokens <= 0:
                raise AdmissionError("episode decoded_tokens must be a positive integer")
            output_tokens += tokens
    elif kind == "real_repository":
        review = report.get("review")
        if not isinstance(review, dict) or (
            review.get("quality_gate") != "passed"
            or review.get("reviewer_verified") is not True
            or review.get("accepted_agent_diff") is not True
        ):
            raise AdmissionError("real repository report lacks a positive reviewer gate")
        tasks = [_hash(report.get("task_sha256"), "task_sha256")]
        tokens = report.get("decoded_tokens")
        if type(tokens) is not int or tokens <= 0:
            raise AdmissionError("report decoded_tokens must be a positive integer")
        output_tokens = tokens
    else:
        raise AdmissionError("unsupported evidence kind")

    if len(set(tasks)) != len(tasks):
        raise AdmissionError("report repeats a task hash")
    if report.get("decoded_tokens") != output_tokens:
        raise AdmissionError("report output-token total differs from its evidence")
    return str(kind), tasks, output_tokens


def admit_report(
    ledger_path: Path,
    report_path: Path,
    sidecar_path: Path,
    *,
    expected_model_sha256: str,
) -> dict[str, object]:
    """Validate and atomically append one unique passing session."""

    model_hash = _hash(expected_model_sha256, "expected_model_sha256")
    report = _read_object(report_path, "agentic report")
    sidecar = _read_object(sidecar_path, "routing sidecar")
    if report.get("schema") != 1 or sidecar.get("schema") != 2:
        raise AdmissionError("unsupported report or sidecar schema")
    report_session = _session(report.get("session_id"), "report session UUID")
    sidecar_session = _session(sidecar.get("session_id"), "sidecar session UUID")
    if report_session != sidecar_session:
        raise AdmissionError("report and sidecar session UUIDs differ")
    if sidecar.get("status") not in ("complete", "interrupted"):
        raise AdmissionError("routing session is not finalized")
    if sidecar.get("model_sha256") != model_hash:
        raise AdmissionError("routing session uses a different model hash")
    decision = report.get("decision")
    if not isinstance(decision, dict) or decision.get("passed") is not True:
        raise AdmissionError("agentic report did not pass its gate")
    evidence_kind, tasks, output_tokens = _report_evidence(report)

    ledger = _load_ledger(ledger_path, model_hash)
    entries = ledger["entries"]
    assert isinstance(entries, list)
    existing_sessions = {
        str(entry.get("session_id")) for entry in entries if isinstance(entry, dict)
    }
    existing_tasks = {
        str(task)
        for entry in entries
        if isinstance(entry, dict) and isinstance(entry.get("task_sha256"), list)
        for task in entry["task_sha256"]
    }
    if report_session in existing_sessions:
        raise AdmissionError("session is already admitted")
    duplicate_tasks = existing_tasks.intersection(tasks)
    if duplicate_tasks:
        raise AdmissionError(f"task is already admitted: {min(duplicate_tasks)}")

    updated_entries = list(entries)
    updated_entries.append(
        {
            "session_id": report_session,
            "evidence_kind": evidence_kind,
            "task_sha256": tasks,
            "output_tokens": output_tokens,
            "report_path": str(report_path),
            "report_sha256": _sha256(report_path),
            "sidecar_path": str(sidecar_path),
        }
    )
    updated = {
        "schema": 1,
        "model_sha256": model_hash,
        "entries": updated_entries,
        "session_count": len(updated_entries),
        "task_count": sum(len(entry["task_sha256"]) for entry in updated_entries),
        "admitted_output_tokens": sum(
            int(entry["output_tokens"]) for entry in updated_entries
        ),
    }
    _write_json_atomic(ledger_path, updated)
    return updated


def reject_report(
    sidecar_path: Path,
    report_path: Path,
    *,
    reason: str,
) -> dict[str, object]:
    """Atomically retain but exclude a finalized, failed quality-gate session."""

    if not isinstance(reason, str) or not reason.strip():
        raise AdmissionError("rejection reason must be a non-empty string")
    report = _read_object(report_path, "agentic report")
    sidecar = _read_object(sidecar_path, "routing sidecar")
    if report.get("schema") != 1 or sidecar.get("schema") != 2:
        raise AdmissionError("unsupported report or sidecar schema")
    report_session = _session(report.get("session_id"), "report session UUID")
    sidecar_session = _session(sidecar.get("session_id"), "sidecar session UUID")
    if report_session != sidecar_session:
        raise AdmissionError("report and sidecar session UUIDs differ")
    decision = report.get("decision")
    if not isinstance(decision, dict) or decision.get("passed") is not False:
        raise AdmissionError("only an explicitly failed report may be rejected")
    original_status = sidecar.get("status")
    if original_status not in ("complete", "interrupted"):
        raise AdmissionError("only a finalized routing session may be rejected")
    updated = dict(sidecar)
    updated["status"] = "rejected"
    updated["rejection"] = {
        "reason": reason.strip(),
        "original_status": original_status,
        "report_path": str(report_path),
        "report_sha256": _sha256(report_path),
    }
    _write_json_atomic(sidecar_path, updated)
    return updated


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ledger", type=Path)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--sidecar", required=True, type=Path)
    parser.add_argument("--model-sha256")
    parser.add_argument("--reject-reason")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.reject_reason is not None:
            result = reject_report(
                args.sidecar,
                args.report,
                reason=args.reject_reason,
            )
        else:
            if args.ledger is None or args.model_sha256 is None:
                raise AdmissionError(
                    "admission requires --ledger and --model-sha256"
                )
            result = admit_report(
                args.ledger,
                args.report,
                args.sidecar,
                expected_model_sha256=args.model_sha256,
            )
    except AdmissionError as error:
        print(f"evidence rejected: {error}")
        return 2
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
