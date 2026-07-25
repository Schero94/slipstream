"""Re-evaluate immutable historical M0a sidecars with the S4 policy."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
from typing import Mapping

from bench.m0a.admission_policy import PolicyError, evaluate_session_policy
from bench.m0a.coding_telemetry import TelemetryError, parse_log_decode_samples
from bench.m0a.start_session import DEFAULT_ARTIFACTS
from bench.m0a.smoke_server import _write_json_atomic


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_LEDGER = DEFAULT_ARTIFACTS / "admission-ledger.json"
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
TERMINAL_STATUSES = {"complete", "interrupted", "rejected"}


class ReevaluateError(RuntimeError):
    pass


def _load_object(path: Path) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ReevaluateError(f"cannot read {path}: {error}") from error
    if not isinstance(value, dict):
        raise ReevaluateError(f"JSON evidence is not an object: {path}")
    return value


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _resolve(sidecar_path: Path, value: object) -> Path:
    if not isinstance(value, str):
        raise ReevaluateError("server log path is missing")
    path = Path(value)
    return path if path.is_absolute() else sidecar_path.parent / path


def _old_decision(session_id: str, sidecar: Mapping[str, object], admitted: set[str]) -> str:
    if session_id in admitted:
        return "ADMITTED"
    if sidecar.get("status") == "rejected":
        return "REJECTED"
    return "UNREVIEWED"


def reevaluate_sessions(artifacts: Path, ledger_path: Path) -> dict[str, object]:
    ledger = _load_object(ledger_path)
    entries = ledger.get("entries")
    if not isinstance(entries, list):
        raise ReevaluateError("admission ledger entries are missing")
    admitted = {
        str(entry["session_id"])
        for entry in entries
        if isinstance(entry, Mapping) and isinstance(entry.get("session_id"), str)
    }
    sessions: list[dict[str, object]] = []
    source_hashes: dict[str, str] = {str(ledger_path.resolve()): _sha256(ledger_path)}
    for sidecar_path in sorted(artifacts.glob("routing-*.json")):
        sidecar = _load_object(sidecar_path)
        status = sidecar.get("status")
        if status == "running":
            continue
        if status not in TERMINAL_STATUSES:
            raise ReevaluateError(f"invalid status in {sidecar_path}: {status}")
        session_id = sidecar.get("session_id")
        if not isinstance(session_id, str):
            raise ReevaluateError(f"session id is missing in {sidecar_path}")
        source_hashes[str(sidecar_path.resolve())] = _sha256(sidecar_path)
        row: dict[str, object] = {
            "session_id": session_id,
            "status": status,
            "old_decision": _old_decision(session_id, sidecar, admitted),
            "sidecar_sha256": source_hashes[str(sidecar_path.resolve())],
        }
        missing: list[str] = []
        if sidecar.get("schema") != 2:
            missing.append("schema-2")
        peak = sidecar.get("peak_rss_kb")
        if type(peak) is not int or peak <= 0:
            missing.append("peak-rss")
        samples: tuple[dict[str, object], ...] = ()
        if not missing:
            try:
                log_path = _resolve(sidecar_path, sidecar.get("server_log_path"))
                log_text = log_path.read_text(encoding="utf-8")
                source_hashes[str(log_path.resolve())] = _sha256(log_path)
                samples = tuple(
                    {**sample, "peak_rss_kb": peak}
                    for sample in parse_log_decode_samples(log_text)
                )
            except (OSError, ReevaluateError, TelemetryError) as error:
                missing.append(f"server-log:{error}")
        if missing:
            row.update(
                {
                    "new_decision": "MISSING_EVIDENCE",
                    "missing_evidence": missing,
                    "policy_decision": None,
                }
            )
        else:
            try:
                policy = evaluate_session_policy(samples)
            except PolicyError as error:
                raise ReevaluateError(f"invalid policy evidence for {session_id}: {error}") from error
            row.update(
                {
                    "new_decision": "PERFORMANCE_PASS" if policy["passed"] else "PERFORMANCE_FAIL",
                    "missing_evidence": [],
                    "policy_decision": policy,
                }
            )
        sessions.append(row)
    return {
        "schema": 1,
        "policy": "peregrine-s4-v1",
        "session_count": len(sessions),
        "evaluated_count": sum(row["new_decision"] != "MISSING_EVIDENCE" for row in sessions),
        "missing_evidence_count": sum(row["new_decision"] == "MISSING_EVIDENCE" for row in sessions),
        "performance_pass_count": sum(row["new_decision"] == "PERFORMANCE_PASS" for row in sessions),
        "performance_fail_count": sum(row["new_decision"] == "PERFORMANCE_FAIL" for row in sessions),
        "sessions": sessions,
        "source_sha256": source_hashes,
        "m0a_admitted_tokens": 0,
    }


def append_results(path: Path, report: Mapping[str, object], evidence_hash: str) -> None:
    marker = f"S4 evidence SHA-256: `{evidence_hash}`"
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    if marker in existing:
        raise ReevaluateError("S4 evidence is already present in RESULTS")
    lines = [
        f"\n## Track S4 admission-policy re-evaluation — {datetime.now(timezone.utc).isoformat()}\n",
        f"- {marker}",
        "- Policy `peregrine-s4-v1`: scored responses >=8 tokens; session mean >=24 tok/s; P10 >=18 tok/s; response warning below 24 tok/s; RSS <=31,000,000 KiB; profile 64K floor >=20 tok/s",
        f"- Historical terminal sidecars: {report['session_count']}; evaluated: {report['evaluated_count']}; missing evidence: {report['missing_evidence_count']}; performance PASS/FAIL: {report['performance_pass_count']}/{report['performance_fail_count']}",
        "- Old admission/quality decisions are retained separately; the new result is performance-only and never admits or removes historical tokens.",
    ]
    for row in report["sessions"]:
        policy = row["policy_decision"]
        detail = "missing=" + ",".join(row["missing_evidence"])
        if isinstance(policy, Mapping):
            detail = (
                f"mean={policy['mean_decode_tokens_per_second']:.4f}, "
                f"P10={policy['p10_decode_tokens_per_second']:.4f}, "
                f"scored={policy['scored_response_count']}/{policy['response_count']}"
            )
        lines.append(
            f"- `{row['session_id']}`: old `{row['old_decision']}` -> new `{row['new_decision']}` ({detail})"
        )
    lines.extend(["- S4 re-evaluation admitted tokens: 0", ""])
    with path.open("a", encoding="utf-8") as stream:
        stream.write("\n".join(lines))
        stream.flush()
        os.fsync(stream.fileno())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, default=DEFAULT_ARTIFACTS)
    parser.add_argument("--ledger", type=Path, default=DEFAULT_LEDGER)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    args = parser.parse_args()
    try:
        report = reevaluate_sessions(args.artifacts, args.ledger)
        _write_json_atomic(args.output, report)
        append_results(args.results, report, _sha256(args.output))
    except (OSError, ReevaluateError) as error:
        print(f"session re-evaluation failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
