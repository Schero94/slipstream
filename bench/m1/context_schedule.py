"""Evaluate and run Peregrine S3b fixed-draft context sweeps."""

from __future__ import annotations

import argparse
from dataclasses import asdict
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from typing import Iterable, Mapping

from bench.m0a.qualify_model import (
    QualificationProfile,
    TOKEN_SEED_TEXT,
    _read_manifest,
    _run_point,
    profile_environment,
    qualification_server_command,
)
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    _json_request,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)
from bench.m1.headroom import _command_output, parse_memory_pressure, parse_vm_stat


DRAFT_LENGTHS = (4, 6, 8, 10, 12)
CONTEXTS = (4_000, 32_000, 64_000)
SWEEP_CONTEXT_SIZES = {4_000: 8_192, 32_000: 32_768, 64_000: 65_536}
SWEEP_CELLS = ((4, 64_000),) + tuple(
    (draft, context)
    for draft in DRAFT_LENGTHS
    for context in CONTEXTS
    if (draft, context) != (4, 64_000)
)
REFERENCE_SPEEDS = {
    4_000: 43.747761,
    32_000: 33.651878,
    64_000: 25.568990,
}
REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
MIN_WIRED_LIMIT_MB = 28 * 1024
MAX_WIRED_LIMIT_MB = 28 * 1024
MAX_DECODE_RECLAIM_BYTES = 512 * 1024
MAX_WARMUP_SPEED_DRIFT_PERCENT = 1.0
MAX_MEMORY_PRESSURE_DROP_PERCENT = 1


class ContextScheduleError(RuntimeError):
    pass


def build_profile(draft_tokens: int) -> QualificationProfile:
    if draft_tokens not in DRAFT_LENGTHS:
        raise ContextScheduleError(f"unsupported S3b draft length: {draft_tokens}")
    return QualificationProfile(
        name=f"s3b-f16-fa-mtp{draft_tokens}",
        flash_attention="on",
        cache_type_k="f16",
        cache_type_v="f16",
        speculation="draft-mtp",
        draft_tokens=draft_tokens,
    )


def build_sweep_command(
    model: Path | str,
    port: int,
    profile: QualificationProfile,
    context_tokens: int,
    *,
    server: Path = DEFAULT_SERVER,
) -> list[str]:
    if context_tokens not in SWEEP_CONTEXT_SIZES:
        raise ContextScheduleError(f"unsupported S3b context: {context_tokens}")
    command = qualification_server_command(Path(model), port, server=server, profile=profile)
    command.remove("--no-warmup")
    size_index = command.index("--ctx-size") + 1
    command[size_index] = str(SWEEP_CONTEXT_SIZES[context_tokens])
    return command


def evaluate_host_gate(wired_limit_mb: int) -> dict[str, object]:
    ready = MIN_WIRED_LIMIT_MB <= wired_limit_mb <= MAX_WIRED_LIMIT_MB
    return {
        "ready": ready,
        "wired_limit_mb": wired_limit_mb,
        "minimum_mb": MIN_WIRED_LIMIT_MB,
        "maximum_mb": MAX_WIRED_LIMIT_MB,
        "reason": "ready" if ready else "human_wired_limit_required",
    }


def _integer(cell: Mapping[str, object], key: str) -> int:
    value = cell.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        raise ContextScheduleError(f"cell field {key} is missing or not an integer")
    return value


def validate_matrix(
    cells: Iterable[Mapping[str, object]],
    *,
    require_complete: bool = True,
) -> dict[tuple[int, int], dict[str, object]]:
    expected = {(context, draft) for context in CONTEXTS for draft in DRAFT_LENGTHS}
    normalized: dict[tuple[int, int], dict[str, object]] = {}
    for source in cells:
        context = _integer(source, "context_tokens")
        draft = _integer(source, "draft_tokens")
        key = (context, draft)
        if key not in expected:
            raise ContextScheduleError(f"unexpected matrix cell {key}")
        if key in normalized:
            raise ContextScheduleError(f"duplicate matrix cell {key}")
        decoded = _integer(source, "decoded_tokens")
        rss = _integer(source, "peak_rss_kb")
        prefill_tokens = _integer(source, "prefill_tokens")
        prefill_sampled = _integer(source, "prefill_sampled_tokens")
        decode_prompt_tokens = _integer(source, "decode_window_prompt_tokens")
        warmup_decoded = _integer(source, "warmup_decode_tokens")
        warmup_prompt_tokens = _integer(source, "warmup_prompt_tokens_evaluated")
        pressure_before = _integer(source, "memory_free_percent_before")
        pressure_after = _integer(source, "memory_free_percent_after")
        pageouts = _integer(source, "pageouts_delta")
        page_size = _integer(source, "page_size_bytes")
        pageins = _integer(source, "pageins_delta")
        pageout_bytes = _integer(source, "pageout_bytes_delta")
        swapins = _integer(source, "swapins_delta")
        swapin_bytes = _integer(source, "swapin_bytes_delta")
        swapouts = _integer(source, "swapouts_delta")
        speed = source.get("decode_tokens_per_second")
        warmup_speed = source.get("warmup_decode_tokens_per_second")
        if isinstance(speed, bool) or not isinstance(speed, (int, float)):
            raise ContextScheduleError(f"cell {key} has invalid throughput")
        if isinstance(warmup_speed, bool) or not isinstance(warmup_speed, (int, float)):
            raise ContextScheduleError(f"cell {key} has invalid warmup throughput")
        if decoded != 128 or source.get("stop_type") != "limit":
            raise ContextScheduleError(f"cell {key} has invalid decode evidence")
        if (
            prefill_tokens != context
            or prefill_sampled != 1
            or warmup_decoded != 128
            or not 0 <= warmup_prompt_tokens <= 4
            or decode_prompt_tokens != warmup_prompt_tokens
            or source.get("cache_prompt") is not True
        ):
            raise ContextScheduleError(f"cell {key} has invalid phase isolation evidence")
        warmup_hash = source.get("warmup_output_sha256")
        output_hash = source.get("output_sha256")
        if (
            not isinstance(warmup_hash, str)
            or re.fullmatch(r"[0-9a-f]{64}", warmup_hash) is None
            or output_hash != warmup_hash
        ):
            raise ContextScheduleError(f"cell {key} has invalid output parity evidence")
        if float(speed) <= 0 or float(warmup_speed) <= 0 or rss <= 0:
            raise ContextScheduleError(f"cell {key} has invalid speed or RSS")
        speed_drift = abs(float(warmup_speed) / float(speed) - 1.0) * 100.0
        if speed_drift > MAX_WARMUP_SPEED_DRIFT_PERCENT:
            raise ContextScheduleError(f"cell {key} has unstable warmup throughput")
        if (
            page_size <= 0
            or pageins < 0
            or pageouts < 0
            or pageout_bytes != pageouts * page_size
            or swapins < 0
            or swapin_bytes != swapins * page_size
            or swapouts < 0
        ):
            raise ContextScheduleError(f"cell {key} has invalid VM evidence")
        if (
            pageout_bytes + swapin_bytes > MAX_DECODE_RECLAIM_BYTES
            or swapouts != 0
            or pressure_after < pressure_before - MAX_MEMORY_PRESSURE_DROP_PERCENT
        ):
            raise ContextScheduleError(f"cell {key} paged during measurement")
        normalized[key] = dict(source)
    missing = expected - normalized.keys()
    if require_complete and missing:
        raise ContextScheduleError(f"matrix is incomplete: {sorted(missing)}")
    return normalized


def evaluate_matrix(cells: Iterable[Mapping[str, object]]) -> dict[str, object]:
    matrix = validate_matrix(cells)
    winners: dict[str, dict[str, object]] = {}
    non_regressing = True
    improved = False
    schedule_change = False
    for context in CONTEXTS:
        candidates = [matrix[(context, draft)] for draft in DRAFT_LENGTHS]
        winner = min(
            candidates,
            key=lambda cell: (-float(cell["decode_tokens_per_second"]), int(cell["draft_tokens"])),
        )
        speed = float(winner["decode_tokens_per_second"])
        reference = REFERENCE_SPEEDS[context]
        non_regressing = non_regressing and speed >= reference
        improved = improved or speed > reference
        schedule_change = schedule_change or int(winner["draft_tokens"]) != 4
        winners[str(context)] = {
            "draft_tokens": int(winner["draft_tokens"]),
            "decode_tokens_per_second": speed,
            "reference_tokens_per_second": reference,
            "delta_percent": (speed / reference - 1.0) * 100.0,
        }
    passed = non_regressing and improved and schedule_change
    return {
        "schema": 2,
        "decision_policy": "s3b-v2-require-schedule-change",
        "matrix_complete": True,
        "qualification_passed": passed,
        "all_contexts_non_regressing": non_regressing,
        "any_context_improved": improved,
        "any_schedule_change": schedule_change,
        "winners": winners,
        "decision": "QUALIFICATION_PASS" if passed else "SPECULATION_TUNING_CLOSED",
        "corpus_gate": "PENDING" if passed else "NOT_APPLICABLE",
        "m0a_admitted_tokens": 0,
    }


def materialize_schedule(decision: Mapping[str, object]) -> dict[str, object]:
    if not decision.get("qualification_passed") or not decision.get("matrix_complete"):
        raise ContextScheduleError("cannot materialize schedule from rejected evidence")
    winners = decision.get("winners")
    if not isinstance(winners, Mapping):
        raise ContextScheduleError("decision has no winners")
    entries = []
    for context in CONTEXTS:
        winner = winners.get(str(context))
        if not isinstance(winner, Mapping):
            raise ContextScheduleError(f"decision has no winner for {context}")
        entries.append(
            {
                "max_occupied_context_tokens": context,
                "draft_tokens": _integer(winner, "draft_tokens"),
            }
        )
    return {
        "schema": 1,
        "selector": "occupied_context_lookup",
        "entries": entries,
        "corpus_gate": "PENDING",
        "production_enabled": False,
    }


def select_draft_tokens(schedule: Mapping[str, object], occupied_context_tokens: int) -> int:
    if (
        isinstance(occupied_context_tokens, bool)
        or not isinstance(occupied_context_tokens, int)
        or occupied_context_tokens < 0
    ):
        raise ContextScheduleError("occupied context must be a non-negative integer")
    entries = schedule.get("entries")
    if not isinstance(entries, list) or not entries:
        raise ContextScheduleError("schedule has no entries")
    for entry in entries:
        if occupied_context_tokens <= _integer(entry, "max_occupied_context_tokens"):
            return _integer(entry, "draft_tokens")
    return _integer(entries[-1], "draft_tokens")


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _token_sha256(tokens: list[int]) -> str:
    return hashlib.sha256(
        json.dumps(tokens, separators=(",", ":")).encode("ascii")
    ).hexdigest()


def prefill_context(port: int, prompt_tokens: list[int]) -> dict[str, object]:
    response = _json_request(
        f"http://127.0.0.1:{port}/completion",
        {
            "prompt": prompt_tokens,
            "n_predict": 0,
            "ignore_eos": True,
            "temperature": 0,
            "seed": 42,
            "stream": False,
            "cache_prompt": False,
        },
        timeout=3600,
    )
    timings = response.get("timings")
    if not isinstance(timings, Mapping):
        raise ContextScheduleError("prefill response has no timings")
    prompt_n = timings.get("prompt_n")
    predicted_n = timings.get("predicted_n")
    prompt_ms = timings.get("prompt_ms")
    if (
        prompt_n != len(prompt_tokens)
        or predicted_n != 1
        or response.get("tokens_cached") != len(prompt_tokens)
        or response.get("tokens_predicted") != 1
        or response.get("stop_type") != "limit"
    ):
        raise ContextScheduleError(
            "prefill response has incomplete token evidence: "
            f"prompt_n={prompt_n!r}, predicted_n={predicted_n!r}, "
            f"tokens_cached={response.get('tokens_cached')!r}, "
            f"tokens_predicted={response.get('tokens_predicted')!r}, "
            f"stop_type={response.get('stop_type')!r}"
        )
    if isinstance(prompt_ms, bool) or not isinstance(prompt_ms, (int, float)) or prompt_ms <= 0:
        raise ContextScheduleError("prefill response has invalid timing evidence")
    return {
        "prefill_tokens": prompt_n,
        "prefill_sampled_tokens": predicted_n,
        "prefill_ms": float(prompt_ms),
    }


def _run_cell(
    model: Path,
    output_dir: Path,
    *,
    server: Path,
    draft_tokens: int,
    context: int,
) -> dict[str, object]:
    profile = build_profile(draft_tokens)
    port = _unused_port()
    command = build_sweep_command(model, port, profile, context, server=server)
    stem = f"mtp{draft_tokens}-ctx{context}"
    stdout_path = output_dir / f"{stem}.stdout.log"
    stderr_path = output_dir / f"{stem}.stderr.log"
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            env=profile_environment(profile),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            _wait_for_health(process, port)
            tokenized = _json_request(
                f"http://127.0.0.1:{port}/tokenize",
                {"content": TOKEN_SEED_TEXT, "add_special": False, "parse_special": True},
            )
            seed = tokenized.get("tokens")
            if not isinstance(seed, list) or not all(isinstance(token, int) for token in seed):
                raise ContextScheduleError("token seed response is invalid")
            if len(seed) < context:
                raise ContextScheduleError("token seed is shorter than the S3b context")
            prefill = prefill_context(port, seed[:context])
            warmup = _run_point(
                process,
                port,
                seed[:context],
                cache_prompt=True,
                return_tokens=True,
            )
            warmup_tokens = warmup.pop("output_tokens")
            assert isinstance(warmup_tokens, list)
            vm_before = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_before = parse_memory_pressure(_command_output(["memory_pressure"]))
            point = _run_point(
                process,
                port,
                seed[:context],
                cache_prompt=True,
                return_tokens=True,
            )
            output_tokens = point.pop("output_tokens")
            assert isinstance(output_tokens, list)
            vm_after = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_after = parse_memory_pressure(_command_output(["memory_pressure"]))
            if vm_before["page_size_bytes"] != vm_after["page_size_bytes"]:
                raise ContextScheduleError("vm_stat page size changed during a cell")
            deltas = {
                f"{key}_delta": vm_after[key] - vm_before[key]
                for key in ("pageins", "pageouts", "swapins", "swapouts")
            }
            return {
                **point,
                **prefill,
                "draft_tokens": draft_tokens,
                "decode_window_prompt_tokens": point["prompt_tokens_evaluated"],
                "warmup_decode_tokens": warmup["decoded_tokens"],
                "warmup_decode_tokens_per_second": warmup[
                    "decode_tokens_per_second"
                ],
                "warmup_prompt_tokens_evaluated": warmup["prompt_tokens_evaluated"],
                "warmup_output_sha256": _token_sha256(warmup_tokens),
                "output_sha256": _token_sha256(output_tokens),
                "cache_prompt": True,
                "slot_context_tokens": SWEEP_CONTEXT_SIZES[context],
                "runtime_warmup": True,
                "page_size_bytes": vm_before["page_size_bytes"],
                **deltas,
                "pageout_bytes_delta": max(0, deltas["pageouts_delta"])
                * vm_before["page_size_bytes"],
                "swapin_bytes_delta": max(0, deltas["swapins_delta"])
                * vm_before["page_size_bytes"],
                "memory_free_percent_before": pressure_before,
                "memory_free_percent_after": pressure_after,
            }
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def load_resume_checkpoint(
    path: Path,
    run_identity: Mapping[str, object],
    host_gate: Mapping[str, object],
) -> list[dict[str, object]]:
    try:
        checkpoint = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContextScheduleError(f"cannot read resume checkpoint: {error}") from error
    if not isinstance(checkpoint, dict) or checkpoint.get("schema") not in (3, 4):
        raise ContextScheduleError("resume checkpoint schema is not identity-bound")
    for key, value in run_identity.items():
        if checkpoint.get(key) != value:
            raise ContextScheduleError(f"resume checkpoint identity mismatch: {key}")
    if checkpoint.get("host_gate") != dict(host_gate):
        raise ContextScheduleError("resume checkpoint host gate mismatch")
    source_cells = checkpoint.get("cells")
    if not isinstance(source_cells, list) or not all(isinstance(cell, dict) for cell in source_cells):
        raise ContextScheduleError("resume checkpoint cells are invalid")
    cells = [dict(cell) for cell in source_cells]
    for cell in cells:
        if "swapin_bytes_delta" not in cell:
            cell["swapin_bytes_delta"] = (
                _integer(cell, "swapins_delta") * _integer(cell, "page_size_bytes")
            )
    validate_matrix(cells, require_complete=False)
    return cells


def run_sweep(
    model: Path,
    output_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
    resume: bool = False,
) -> dict[str, object]:
    manifest = _read_manifest(model)
    llama_cpp_commit = subprocess.check_output(
        ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
        text=True,
    ).strip()
    run_identity = {
        "model_sha256": manifest["sha256"],
        "llama_cpp_commit": llama_cpp_commit,
        "server_sha256": _sha256(server),
    }
    wired_limit_mb = int(_command_output(["/usr/sbin/sysctl", "-n", "iogpu.wired_limit_mb"]).strip())
    host_gate = evaluate_host_gate(wired_limit_mb)
    if not host_gate["ready"]:
        raise ContextScheduleError(
            f"host gate failed: iogpu.wired_limit_mb={wired_limit_mb}; "
            f"human must select {MIN_WIRED_LIMIT_MB}..{MAX_WIRED_LIMIT_MB} MB"
        )
    if resume:
        if not output_dir.is_dir():
            raise ContextScheduleError("resume output directory does not exist")
        cells = load_resume_checkpoint(
            output_dir / "partial-cells.json", run_identity, host_gate
        )
    else:
        output_dir.mkdir(parents=True, exist_ok=False)
        cells = []
    completed = {
        (_integer(cell, "draft_tokens"), _integer(cell, "context_tokens"))
        for cell in cells
    }

    for draft_tokens, context in SWEEP_CELLS:
        if (draft_tokens, context) in completed:
            continue
        cell = _run_cell(
            model,
            output_dir,
            server=server,
            draft_tokens=draft_tokens,
            context=context,
        )
        cells.append(cell)
        _write_json_atomic(
            output_dir / "partial-cells.json",
            {"schema": 4, **run_identity, "host_gate": host_gate, "cells": cells},
        )
        speed_drift = abs(
            float(cell["warmup_decode_tokens_per_second"])
            / float(cell["decode_tokens_per_second"])
            - 1.0
        ) * 100.0
        if (
            int(cell["pageout_bytes_delta"]) + int(cell["swapin_bytes_delta"])
            > MAX_DECODE_RECLAIM_BYTES
            or int(cell["swapouts_delta"]) != 0
            or int(cell["memory_free_percent_after"])
            < int(cell["memory_free_percent_before"])
            - MAX_MEMORY_PRESSURE_DROP_PERCENT
            or speed_drift > MAX_WARMUP_SPEED_DRIFT_PERCENT
        ):
            raise ContextScheduleError(
                f"matrix cell {(context, draft_tokens)} failed decode stability; "
                "partial evidence preserved"
            )
        if (
            cell["warmup_output_sha256"] != cell["output_sha256"]
            or cell["warmup_prompt_tokens_evaluated"]
            != cell["decode_window_prompt_tokens"]
            or not 0 <= int(cell["decode_window_prompt_tokens"]) <= 4
        ):
            raise ContextScheduleError(
                f"matrix cell {(context, draft_tokens)} failed decode parity; "
                "partial evidence preserved"
            )

    decision = evaluate_matrix(cells)
    schedule = materialize_schedule(decision) if decision["qualification_passed"] else None
    report: dict[str, object] = {
        "schema": 1,
        "host_gate": host_gate,
        "runtime_profiles": [asdict(build_profile(draft)) for draft in DRAFT_LENGTHS],
        **run_identity,
        "cells": cells,
        "decision": decision,
        "schedule": schedule,
        "m0a_admitted_tokens": 0,
    }
    _write_json_atomic(output_dir / "context-schedule.json", report)
    return report


def append_results(path: Path, report: Mapping[str, object], evidence_hash: str) -> None:
    marker = f"S3b evidence SHA-256: `{evidence_hash}`"
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    if marker in existing:
        raise ContextScheduleError("S3b evidence is already present in RESULTS")
    decision = report.get("decision")
    if not isinstance(decision, Mapping):
        raise ContextScheduleError("S3b report has no decision")
    lines = [
        f"\n## Track S3b context schedule — {datetime.now(timezone.utc).isoformat()}\n",
        f"- {marker}",
        "- Matrix: draft lengths `{4,6,8,10,12}` x contexts `{4K,32K,64K}`; 128 measured decode tokens per cell; identity-bound multi-signal stability gate",
        f"- Decision policy: `{decision.get('decision_policy', 'legacy')}`; schedule changes at any context: `{str(bool(decision.get('any_schedule_change'))).lower()}`",
    ]
    winners = decision.get("winners")
    if isinstance(winners, Mapping):
        for context in CONTEXTS:
            winner = winners[str(context)]
            lines.append(
                f"- {context:,}: MTP={winner['draft_tokens']}, {winner['decode_tokens_per_second']:.4f} tok/s, "
                f"delta {winner['delta_percent']:+.2f}% versus fixed-MTP4 reference"
            )
    lines.extend(
        [
            f"- Qualification decision: **{decision['decision']}**; corpus gate: **{decision['corpus_gate']}**",
            "- 0 M0a-admitted tokens",
            "",
        ]
    )
    with path.open("a", encoding="utf-8") as stream:
        stream.write("\n".join(lines))
        stream.flush()
        os.fsync(stream.fileno())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    parser.add_argument("--resume", action="store_true")
    args = parser.parse_args()
    try:
        report = run_sweep(
            args.model, args.output_dir, server=args.server, resume=args.resume
        )
        evidence_path = args.output_dir / "context-schedule.json"
        append_results(args.results, report, _sha256(evidence_path))
    except (ContextScheduleError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"context schedule failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["decision"]["qualification_passed"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
