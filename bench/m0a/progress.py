"""Validate M0a session artifacts and report progress toward 200K decode tokens."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from uuid import UUID

from bench.m0a.coding_telemetry import (
    MIN_DECODE_TOKENS_PER_SECOND,
    TARGET_DECODE_TOKENS_PER_SECOND,
    TelemetryError,
    parse_log_decode_rates,
    parse_log_decode_tokens,
)
from bench.m0a.routing_format import (
    PHASE_DECODE,
    PHASE_PROMPT,
    RECORD_PREFIX,
    RECORD_STRUCT,
    RoutingFormatError,
    UNUSED_EXPERT,
    _crc32,
    read_header,
)
from bench.m0a.start_session import DEFAULT_ARTIFACTS
from scripts.verify_model import sha256_file


TARGET_DECODE_TOKENS = 200_000


class ProgressError(RuntimeError):
    """Raised when session evidence is corrupt or cannot be combined."""


def _validated_record_fields(
    data: bytes,
    *,
    layer_count: int,
    expert_count: int,
    top_k: int,
) -> tuple[int, int, int, int, int, int]:
    """Validate one wire record and return only fields needed by progress."""

    values = RECORD_STRUCT.unpack(data)
    if values[-1] != _crc32(data[: RECORD_PREFIX.size]):
        raise RoutingFormatError("routing record CRC does not match")
    wire_experts = values[6:16]
    active = wire_experts[:top_k]
    if any(expert != UNUSED_EXPERT for expert in wire_experts[top_k:]):
        raise RoutingFormatError("unused routing expert slots are not 0xFFFF")
    phase = values[4]
    layer = values[5]
    if phase not in (PHASE_PROMPT, PHASE_DECODE):
        raise RoutingFormatError("record phase is invalid")
    if not 0 <= layer < layer_count:
        raise RoutingFormatError("record layer is outside header geometry")
    if len(set(active)) != len(active):
        raise RoutingFormatError("record contains duplicate selected experts")
    if any(expert >= expert_count for expert in active):
        raise RoutingFormatError("record contains an out-of-range expert")
    return values[0], values[1], values[2], values[3], phase, layer


def _routing_summary(path: Path) -> tuple[object, int, int, int | None, int | None]:
    """Validate every record while retaining only per-token group metadata."""

    groups: dict[tuple[UUID, int, int], list[int | bool]] = {}
    partial_bytes = 0
    try:
        with path.open("rb") as stream:
            header = read_header(stream)
            while data := stream.read(RECORD_STRUCT.size):
                if len(data) != RECORD_STRUCT.size:
                    partial_bytes = len(data)
                    break
                batch_id, token_pos, token_id, sequence_id, phase, layer = (
                    _validated_record_fields(
                        data,
                        layer_count=header.layer_count,
                        expert_count=header.expert_count,
                        top_k=header.top_k,
                    )
                )
                if phase != PHASE_DECODE:
                    continue
                key = (header.session_id, batch_id, token_pos)
                state = groups.get(key)
                layer_bit = 1 << layer
                if state is None:
                    groups[key] = [
                        layer_bit,
                        1,
                        token_id,
                        sequence_id,
                        True,
                    ]
                    continue
                if int(state[0]) & layer_bit:
                    raise ProgressError(f"duplicate layer in decode token: {key}")
                state[0] = int(state[0]) | layer_bit
                state[1] = int(state[1]) + 1
                state[4] = bool(state[4]) and (
                    token_id == state[2]
                    and sequence_id == state[3]
                )
    except (OSError, RoutingFormatError) as error:
        raise ProgressError(f"invalid routing file {path}: {error}") from error

    expected_layer_mask = (1 << 40) - 1
    routed_decode_tokens = 0
    incomplete_bytes = 0
    keys = list(groups)
    for index, key in enumerate(keys):
        layer_mask, record_count, _, _, metadata_consistent = groups[key]
        valid = record_count == 40 and layer_mask == expected_layer_mask
        if valid:
            if not metadata_consistent:
                raise ProgressError(
                    f"invalid decode groups in {path}: inconsistent token or "
                    f"sequence metadata for decode token {key}"
                )
            routed_decode_tokens += 1
            continue
        if index != len(keys) - 1:
            raise ProgressError(f"incomplete decode token before file tail: {key}")
        incomplete_bytes += int(record_count) * RECORD_STRUCT.size
    positions = [
        key[2]
        for key in keys
        if groups[key][1] == 40 and groups[key][0] == expected_layer_mask
    ]
    return (
        header,
        routed_decode_tokens,
        partial_bytes + incomplete_bytes,
        min(positions) if positions else None,
        max(positions) if positions else None,
    )


def _session_progress(sidecar_path: Path, sidecar: dict[str, object]) -> dict[str, object]:
    try:
        session_id = UUID(str(sidecar["session_id"]))
        model_hash = str(sidecar["model_sha256"])
        routing_path = Path(str(sidecar["routing_path"]))
    except (KeyError, ValueError) as error:
        raise ProgressError(f"invalid sidecar {sidecar_path}: {error}") from error
    if not routing_path.is_absolute():
        routing_path = sidecar_path.parent / routing_path
    if not routing_path.is_file():
        raise ProgressError(f"routing file is missing: {routing_path}")
    recorded_hash = sidecar.get("routing_sha256")
    if recorded_hash is not None and sha256_file(routing_path) != recorded_hash:
        raise ProgressError(f"routing hash mismatch: {routing_path}")

    header, routed_decode_tokens, corrupt_tail_bytes, first_position, last_position = (
        _routing_summary(routing_path)
    )
    if header.session_id != session_id:
        raise ProgressError(f"session UUID mismatch: {routing_path}")
    if header.model_sha256.hex() != model_hash:
        raise ProgressError(f"model hash differs between sidecar and routing header: {routing_path}")
    decode_tokens = routed_decode_tokens
    response_count: int | None = None
    observed_minimum_decode_rate: float | None = None
    observed_maximum_decode_rate: float | None = None
    responses_below_minimum: int | None = None
    if sidecar.get("schema") == 2:
        server_log_value = sidecar.get("server_log_path")
        if not isinstance(server_log_value, str):
            raise ProgressError(f"schema-2 sidecar has no server log: {sidecar_path}")
        server_log_path = Path(server_log_value)
        if not server_log_path.is_absolute():
            server_log_path = sidecar_path.parent / server_log_path
        try:
            server_log = server_log_path.read_text(encoding="utf-8")
            decode_tokens = parse_log_decode_tokens(server_log)
            decode_rates = parse_log_decode_rates(server_log)
        except (OSError, TelemetryError) as error:
            raise ProgressError(f"invalid server log {server_log_path}: {error}") from error
        response_count = len(decode_rates)
        observed_minimum_decode_rate = min(decode_rates)
        observed_maximum_decode_rate = max(decode_rates)
        responses_below_minimum = sum(
            rate < MIN_DECODE_TOKENS_PER_SECOND for rate in decode_rates
        )
        if decode_tokens > routed_decode_tokens:
            raise ProgressError(
                f"server output count exceeds routed evaluations: {server_log_path}"
            )
    return {
        "session_id": str(session_id),
        "status": sidecar["status"],
        "model_sha256": model_hash,
        "routing_path": str(routing_path),
        "decode_tokens": decode_tokens,
        "routed_decode_tokens": routed_decode_tokens,
        "bytes": routing_path.stat().st_size,
        "first_position": first_position,
        "last_position": last_position,
        "corrupt_tail_bytes": corrupt_tail_bytes,
        "response_count": response_count,
        "observed_minimum_decode_tokens_per_second": observed_minimum_decode_rate,
        "observed_maximum_decode_tokens_per_second": observed_maximum_decode_rate,
        "target_decode_tokens_per_second": TARGET_DECODE_TOKENS_PER_SECOND,
        "operational_minimum_decode_tokens_per_second": MIN_DECODE_TOKENS_PER_SECOND,
        "responses_below_minimum": responses_below_minimum,
    }


def _validate_model_sha256(value: str) -> None:
    if not isinstance(value, str) or len(value) != 64 or any(
        character not in "0123456789abcdef" for character in value
    ):
        raise ProgressError(
            "model_sha256 must be exactly 64 lowercase hexadecimal characters"
        )


def collect_progress(
    artifacts: Path = DEFAULT_ARTIFACTS,
    model_sha256: str | None = None,
) -> dict[str, object]:
    if model_sha256 is not None:
        _validate_model_sha256(model_sha256)
    sessions: list[dict[str, object]] = []
    model_hashes: set[str] = set()
    if artifacts.exists():
        for sidecar_path in sorted(artifacts.rglob("routing-*.json")):
            try:
                sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                raise ProgressError(f"invalid sidecar {sidecar_path}: {error}") from error
            if not isinstance(sidecar, dict):
                raise ProgressError(f"sidecar is not a JSON object: {sidecar_path}")
            if (
                model_sha256 is not None
                and sidecar.get("model_sha256") != model_sha256
            ):
                continue
            status = sidecar.get("status")
            if status in ("running", "rejected"):
                continue
            if status not in ("complete", "interrupted"):
                raise ProgressError(f"invalid session status in {sidecar_path}: {status}")
            session = _session_progress(sidecar_path, sidecar)
            sessions.append(session)
            model_hashes.add(str(session["model_sha256"]))
    if len(model_hashes) > 1:
        raise ProgressError("sessions contain mixed model hashes")
    decode_tokens = sum(int(session["decode_tokens"]) for session in sessions)
    routed_decode_tokens = sum(
        int(session["routed_decode_tokens"]) for session in sessions
    )
    total_bytes = sum(int(session["bytes"]) for session in sessions)
    return {
        "schema": 1,
        "sessions": sessions,
        "session_count": len(sessions),
        "model_sha256": next(iter(model_hashes), None),
        "decode_tokens": decode_tokens,
        "routed_decode_tokens": routed_decode_tokens,
        "bytes": total_bytes,
        "percent_of_200k": decode_tokens / TARGET_DECODE_TOKENS * 100.0,
        "ready_for_analysis": decode_tokens >= TARGET_DECODE_TOKENS,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, default=DEFAULT_ARTIFACTS)
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--model-sha256",
        help="only include sessions matching this lowercase SHA-256",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        progress = collect_progress(args.artifacts, model_sha256=args.model_sha256)
    except ProgressError as error:
        print(f"progress invalid: {error}")
        return 2
    if args.json:
        print(json.dumps(progress, indent=2, sort_keys=True))
    else:
        for session in progress["sessions"]:
            print(
                f"{session['session_id']} {session['status']}: "
                f"{session['decode_tokens']} decode tokens, {session['bytes']} bytes, "
                f"{session['routed_decode_tokens']} routed evaluations, "
                f"positions {session['first_position']}..{session['last_position']}, "
                f"corrupt tail {session['corrupt_tail_bytes']} bytes"
            )
        print(
            f"TOTAL: {progress['decode_tokens']} / {TARGET_DECODE_TOKENS} "
            f"({progress['percent_of_200k']:.3f}%), {progress['bytes']} bytes"
        )
    if progress["ready_for_analysis"]:
        print("READY_FOR_ANALYSIS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
