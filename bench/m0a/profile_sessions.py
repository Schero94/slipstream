"""Build deterministic, offline runtime profiles from admitted M0a sessions."""

from __future__ import annotations

import argparse
from collections import defaultdict
import json
import math
import os
from pathlib import Path
from typing import Sequence

from bench.m0a.admission_policy import evaluate_session_policy, percentile as policy_percentile
from bench.m0a.coding_telemetry import (
    MIN_DECODE_TOKENS_PER_SECOND,
    TARGET_DECODE_TOKENS_PER_SECOND,
    TelemetryError,
    parse_log_decode_samples,
    parse_log_draft_totals,
)
from bench.m0a.progress import (
    ProgressError,
    _validate_model_sha256,
    collect_progress,
)
from bench.m0a.start_session import DEFAULT_ARTIFACTS


def _percentile(sorted_values: Sequence[float], percentile: float) -> float:
    return policy_percentile(sorted_values, percentile)


def _rate_summary(rates: Sequence[float]) -> dict[str, object]:
    if not rates:
        return {
            "response_count": None,
            "mean_decode_tokens_per_second": None,
            "minimum_decode_tokens_per_second": None,
            "p10_decode_tokens_per_second": None,
            "p50_decode_tokens_per_second": None,
            "p90_decode_tokens_per_second": None,
            "p99_decode_tokens_per_second": None,
            "maximum_decode_tokens_per_second": None,
            "responses_below_minimum": None,
        }
    ordered = sorted(rates)
    return {
        "response_count": len(rates),
        "mean_decode_tokens_per_second": math.fsum(rates) / len(rates),
        "minimum_decode_tokens_per_second": ordered[0],
        "p10_decode_tokens_per_second": _percentile(ordered, 0.10),
        "p50_decode_tokens_per_second": _percentile(ordered, 0.50),
        "p90_decode_tokens_per_second": _percentile(ordered, 0.90),
        "p99_decode_tokens_per_second": _percentile(ordered, 0.99),
        "maximum_decode_tokens_per_second": ordered[-1],
        "responses_below_minimum": sum(
            rate < MIN_DECODE_TOKENS_PER_SECOND for rate in rates
        ),
    }


def _read_sidecars(artifacts: Path) -> dict[str, tuple[Path, dict[str, object]]]:
    sidecars: dict[str, tuple[Path, dict[str, object]]] = {}
    if not artifacts.exists():
        return sidecars
    for path in sorted(artifacts.rglob("routing-*.json")):
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise ProgressError(f"invalid sidecar {path}: {error}") from error
        if not isinstance(value, dict):
            raise ProgressError(f"sidecar is not a JSON object: {path}")
        session_id = value.get("session_id")
        if isinstance(session_id, str):
            if session_id in sidecars:
                raise ProgressError(f"duplicate session sidecar: {session_id}")
            sidecars[session_id] = (path, value)
    return sidecars


def _resolve_sidecar_path(sidecar_path: Path, value: object, label: str) -> Path:
    if not isinstance(value, str):
        raise ProgressError(f"schema-2 sidecar has no {label}: {sidecar_path}")
    path = Path(value)
    return path if path.is_absolute() else sidecar_path.parent / path


def _profile_session(
    progress: dict[str, object],
    sidecar_path: Path,
    sidecar: dict[str, object],
) -> tuple[dict[str, object], tuple[dict[str, object], ...]]:
    schema = sidecar.get("schema")
    profile_evidence = schema == 2
    client_profile = "unknown"
    speculation_type = "unknown"
    draft_tokens: int | None = None
    peak_rss_kb: int | None = None
    samples: tuple[dict[str, object], ...] = ()
    draft = {
        "draft_generated": None,
        "draft_accepted": None,
        "draft_acceptance": None,
    }
    if profile_evidence:
        client_profile_value = sidecar.get("client_profile")
        speculation = sidecar.get("speculation")
        if not isinstance(client_profile_value, str) or not client_profile_value.strip():
            raise ProgressError(f"invalid client profile in {sidecar_path}")
        if not isinstance(speculation, dict):
            raise ProgressError(f"invalid speculation profile in {sidecar_path}")
        speculation_type_value = speculation.get("type")
        draft_tokens_value = speculation.get("draft_tokens")
        if speculation_type_value not in ("none", "draft-mtp"):
            raise ProgressError(f"invalid speculation type in {sidecar_path}")
        if speculation_type_value == "none" and draft_tokens_value is not None:
            raise ProgressError(f"non-MTP profile has draft tokens in {sidecar_path}")
        if speculation_type_value == "draft-mtp" and (
            type(draft_tokens_value) is not int or draft_tokens_value <= 0
        ):
            raise ProgressError(f"invalid MTP draft length in {sidecar_path}")
        peak_value = sidecar.get("peak_rss_kb")
        if peak_value is not None and (type(peak_value) is not int or peak_value <= 0):
            raise ProgressError(f"invalid peak RSS in {sidecar_path}")
        log_path = _resolve_sidecar_path(
            sidecar_path, sidecar.get("server_log_path"), "server log"
        )
        try:
            log_text = log_path.read_text(encoding="utf-8")
            parsed_samples = parse_log_decode_samples(log_text)
            samples = tuple(
                {**sample, "peak_rss_kb": peak_value}
                for sample in parsed_samples
                if peak_value is not None
            )
            draft = parse_log_draft_totals(log_text)
        except (OSError, TelemetryError) as error:
            raise ProgressError(f"invalid server log {log_path}: {error}") from error
        if speculation_type_value == "draft-mtp" and draft["draft_generated"] is None:
            raise ProgressError(f"MTP server log has no draft evidence: {log_path}")
        client_profile = client_profile_value
        speculation_type = speculation_type_value
        draft_tokens = draft_tokens_value
        peak_rss_kb = peak_value

    decoded = int(progress["decode_tokens"])
    routed = int(progress["routed_decode_tokens"])
    rates = tuple(float(sample["decode_tokens_per_second"]) for sample in samples)
    policy_decision = evaluate_session_policy(samples) if samples else None
    result = {
        "session_id": progress["session_id"],
        "status": progress["status"],
        "client_profile": client_profile,
        "speculation": {
            "type": speculation_type,
            "draft_tokens": draft_tokens,
        },
        "profile_evidence": profile_evidence,
        "decode_tokens": decoded,
        "routed_decode_tokens": routed,
        "routing_efficiency": decoded / routed if routed else None,
        "corrupt_tail_bytes": progress["corrupt_tail_bytes"],
        "peak_rss_kb": peak_rss_kb,
        **_rate_summary(rates),
        **draft,
        "policy_decision": policy_decision,
        "target_decode_tokens_per_second": TARGET_DECODE_TOKENS_PER_SECOND,
        "operational_minimum_decode_tokens_per_second": MIN_DECODE_TOKENS_PER_SECOND,
    }
    return result, samples


def _group_profiles(
    sessions_and_samples: Sequence[
        tuple[dict[str, object], tuple[dict[str, object], ...]]
    ],
) -> list[dict[str, object]]:
    grouped: dict[
        tuple[str, str, int | None],
        list[tuple[dict[str, object], tuple[dict[str, object], ...]]],
    ] = defaultdict(list)
    for session, samples in sessions_and_samples:
        speculation = session["speculation"]
        assert isinstance(speculation, dict)
        key = (
            str(session["client_profile"]),
            str(speculation["type"]),
            speculation["draft_tokens"],
        )
        grouped[key].append((session, samples))

    rows: list[dict[str, object]] = []
    for key in sorted(grouped, key=lambda item: (item[0], item[1], item[2] or -1)):
        members = grouped[key]
        evidence = all(bool(session["profile_evidence"]) for session, _ in members)
        samples = tuple(sample for _, values in members for sample in values) if evidence else ()
        rates = tuple(float(sample["decode_tokens_per_second"]) for sample in samples)
        decoded = sum(int(session["decode_tokens"]) for session, _ in members)
        routed = sum(int(session["routed_decode_tokens"]) for session, _ in members)
        generated_values = [session["draft_generated"] for session, _ in members]
        accepted_values = [session["draft_accepted"] for session, _ in members]
        drafts_known = evidence and all(value is not None for value in generated_values)
        generated = sum(int(value) for value in generated_values) if drafts_known else None
        accepted = sum(int(value) for value in accepted_values) if drafts_known else None
        rss_values = [
            int(session["peak_rss_kb"])
            for session, _ in members
            if session["peak_rss_kb"] is not None
        ]
        rows.append(
            {
                "client_profile": key[0],
                "speculation": {"type": key[1], "draft_tokens": key[2]},
                "profile_evidence": evidence,
                "session_count": len(members),
                "decode_tokens": decoded,
                "routed_decode_tokens": routed,
                "routing_efficiency": decoded / routed if routed else None,
                **_rate_summary(rates),
                "policy_decision": evaluate_session_policy(samples) if samples else None,
                "draft_generated": generated,
                "draft_accepted": accepted,
                "draft_acceptance": (
                    accepted / generated if generated is not None and generated else None
                ),
                "maximum_peak_rss_kb": max(rss_values) if rss_values else None,
                "corrupt_tail_bytes": sum(
                    int(session["corrupt_tail_bytes"]) for session, _ in members
                ),
            }
        )
    return rows


def profile_sessions(
    artifacts: Path = DEFAULT_ARTIFACTS,
    model_sha256: str | None = None,
) -> dict[str, object]:
    if model_sha256 is not None:
        _validate_model_sha256(model_sha256)
    progress = collect_progress(artifacts, model_sha256=model_sha256)
    sidecars = _read_sidecars(artifacts)
    sessions_and_rates = []
    for session_progress in progress["sessions"]:
        session_id = str(session_progress["session_id"])
        if session_id not in sidecars:
            raise ProgressError(f"missing sidecar for admitted session: {session_id}")
        sidecar_path, sidecar = sidecars[session_id]
        sessions_and_rates.append(
            _profile_session(session_progress, sidecar_path, sidecar)
        )
    sessions = [session for session, _ in sessions_and_rates]
    return {
        "schema": 1,
        "source": "colibri-pr-232",
        "model_sha256": progress["model_sha256"],
        "session_count": progress["session_count"],
        "decode_tokens": progress["decode_tokens"],
        "routed_decode_tokens": progress["routed_decode_tokens"],
        "corrupt_tail_bytes": sum(
            int(session["corrupt_tail_bytes"]) for session in sessions
        ),
        "sessions": sessions,
        "groups": _group_profiles(sessions_and_rates),
    }


def _write_json_atomic(path: Path, value: object) -> None:
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, default=DEFAULT_ARTIFACTS)
    parser.add_argument("--model-sha256")
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = profile_sessions(args.artifacts, model_sha256=args.model_sha256)
        _write_json_atomic(args.output, report)
    except (OSError, ProgressError) as error:
        print(f"profile invalid: {error}")
        return 2
    print(
        f"profiled {report['session_count']} sessions and "
        f"{report['decode_tokens']} decode tokens"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
