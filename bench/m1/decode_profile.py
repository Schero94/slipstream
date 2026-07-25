"""Run and evaluate opt-in Metal decode-time profiles for Peregrine S7."""

from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import asdict
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
from typing import Iterable, Mapping

from bench.m0a.qualify_model import (
    QUALIFICATION_PROFILES,
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
from bench.m1.context_schedule import (
    SWEEP_CONTEXT_SIZES,
    _token_sha256,
    build_sweep_command,
    evaluate_host_gate,
    prefill_context,
)


PROFILE_PREFIX = "PGR_METAL_PROFILE "
CPU_PROFILE_PREFIX = "PGR_METAL_CPU_PROFILE "
DECODE_QUERY_LIMIT = 12
MINIMUM_COVERAGE = 0.95
MAX_PROFILER_OVERHEAD_PERCENT = 5.0
MAX_DECODE_RECLAIM_BYTES = 512 * 1024
MAX_MEMORY_PRESSURE_DROP_PERCENT = 1
MAX_WARMUP_SPEED_DRIFT_PERCENT = 1.0
GPU_TICK_FIELDS = (
    "attention_ticks",
    "expert_gemm_ticks",
    "dense_gemm_ticks",
    "normalization_ticks",
    "rope_ticks",
    "data_movement_ticks",
    "elementwise_ticks",
    "other_ticks",
)
REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
PROFILE = QUALIFICATION_PROFILES["baseline-f16-fa-mtp4"]
CONTEXTS = (4_000, 64_000)


class DecodeProfileError(RuntimeError):
    pass


def build_profile_command(
    model: Path | str,
    port: int,
    context_tokens: int,
    *,
    server: Path = DEFAULT_SERVER,
) -> list[str]:
    return build_sweep_command(
        Path(model), port, PROFILE, context_tokens, server=server
    )


def parse_profile_records(lines: Iterable[str]) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for line in lines:
        if PROFILE_PREFIX not in line:
            continue
        payload = line.split(PROFILE_PREFIX, 1)[1].strip()
        if not payload.startswith("{"):
            continue
        try:
            record = json.loads(payload)
        except json.JSONDecodeError as error:
            raise DecodeProfileError(f"malformed native profile record: {error}") from error
        if not isinstance(record, dict):
            raise DecodeProfileError("native profile record is not an object")
        records.append(record)
    return records


def parse_cpu_profile_records(lines: Iterable[str]) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for line in lines:
        if CPU_PROFILE_PREFIX not in line:
            continue
        payload = line.split(CPU_PROFILE_PREFIX, 1)[1].strip()
        if not payload.startswith("{"):
            continue
        try:
            record = json.loads(payload)
        except json.JSONDecodeError as error:
            raise DecodeProfileError(f"malformed native CPU profile record: {error}") from error
        if not isinstance(record, dict):
            raise DecodeProfileError("native CPU profile record is not an object")
        records.append(record)
    return records


def _integer(record: Mapping[str, object], key: str) -> int:
    value = record.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        raise DecodeProfileError(f"native profile field {key} is missing or not an integer")
    return value


def aggregate_decode_records(
    records: Iterable[Mapping[str, object]],
    *,
    minimum_coverage: float = MINIMUM_COVERAGE,
    query_tokens: int | None = None,
) -> dict[str, object]:
    by_graph: dict[int, list[Mapping[str, object]]] = defaultdict(list)
    for record in records:
        if _integer(record, "schema") != 2:
            raise DecodeProfileError("native GPU profile schema is not 2")
        by_graph[_integer(record, "graph_id")].append(record)

    decode_records: list[Mapping[str, object]] = []
    for graph_records in by_graph.values():
        if len(graph_records) > 2:
            raise DecodeProfileError(
                "native profile graph ID collision: more than two command buffers"
            )
        max_q = max(_integer(record, "max_attention_q_tokens") for record in graph_records)
        if query_tokens is not None and max_q == query_tokens:
            decode_records.extend(graph_records)
        elif query_tokens is None and 0 < max_q <= DECODE_QUERY_LIMIT:
            decode_records.extend(graph_records)

    if not decode_records:
        raise DecodeProfileError("no decode graphs found in native Metal profile")

    sampled = sum(_integer(record, "sampled_dispatches") for record in decode_records)
    total = sum(_integer(record, "total_dispatches") for record in decode_records)
    errors = sum(_integer(record, "counter_errors") for record in decode_records)
    overflow = any(bool(record.get("overflow")) for record in decode_records)
    coverage = sampled / total if total else 0.0
    if errors:
        raise DecodeProfileError(f"Metal counter samples contain {errors} errors")
    if overflow:
        raise DecodeProfileError("Metal counter sample buffer overflowed")
    if coverage < minimum_coverage:
        raise DecodeProfileError(
            f"Metal dispatch coverage {coverage:.3f} is below {minimum_coverage:.3f}"
        )

    categories = {
        key: sum(_integer(record, key) for record in decode_records)
        for key in GPU_TICK_FIELDS
    }
    total_ticks = sum(categories.values())
    if total_ticks <= 0:
        raise DecodeProfileError("Metal profile contains no positive timestamp deltas")

    return {
        "decode_graphs": len(
            {
                _integer(record, "graph_id")
                for record in decode_records
            }
        ),
        "query_tokens": query_tokens,
        "command_buffers": len(decode_records),
        "sampled_dispatches": sampled,
        "total_dispatches": total,
        "coverage": coverage,
        **categories,
        "total_ticks": total_ticks,
        "shares": {
            key.removesuffix("_ticks"): value / total_ticks
            for key, value in categories.items()
        },
        "command_buffer_gpu_ms_sum": sum(
            float(record.get("command_buffer_gpu_ms", 0.0)) for record in decode_records
        ),
        "max_attention_kv_tokens": max(
            _integer(record, "max_attention_kv_tokens") for record in decode_records
        ),
    }


def aggregate_cpu_records(
    records: Iterable[Mapping[str, object]], *, query_tokens: int
) -> dict[str, object]:
    graph_records: dict[int, Mapping[str, object]] = {}
    wait_us: dict[int, int] = defaultdict(int)
    for record in records:
        if _integer(record, "schema") != 1:
            raise DecodeProfileError("native CPU profile schema is not 1")
        graph_id = _integer(record, "graph_id")
        kind = record.get("kind")
        if kind == "graph":
            if graph_id in graph_records:
                raise DecodeProfileError("duplicate native CPU graph record")
            graph_records[graph_id] = record
        elif kind == "wait":
            wait_us[graph_id] += _integer(record, "wait_us")
        else:
            raise DecodeProfileError("unknown native CPU profile record kind")
    selected = [
        (graph_id, record)
        for graph_id, record in graph_records.items()
        if _integer(record, "query_tokens") == query_tokens
    ]
    if not selected:
        raise DecodeProfileError(f"no CPU graph records found for S={query_tokens}")
    return {
        "graphs": len(selected),
        "query_tokens": query_tokens,
        "encode_submit_us": sum(_integer(record, "encode_submit_us") for _, record in selected),
        "commit_us": sum(_integer(record, "commit_us") for _, record in selected),
        "wait_us": sum(wait_us[graph_id] for graph_id, _ in selected),
    }


def decide_w2_eligibility(profile: Mapping[str, object]) -> dict[str, object]:
    categories = {
        key.removesuffix("_ticks"): int(profile[key]) for key in GPU_TICK_FIELDS
    }
    total = sum(categories.values())
    if total <= 0:
        raise DecodeProfileError("cannot decide W2 eligibility without positive timings")
    dominant = max(categories, key=categories.get)
    attention_share = categories["attention"] / total
    eligible = dominant == "attention" and attention_share >= 0.5
    if eligible:
        reason = "attention_majority"
    elif dominant != "attention":
        reason = "attention_not_largest"
    else:
        reason = "attention_below_majority"
    return {
        "w2_eligible": eligible,
        "dominant_category": dominant,
        "attention_share": attention_share,
        "threshold": 0.5,
        "reason": reason,
    }


def decide_s8_eligibility(
    cpu_profile: Mapping[str, object], decode_wall_seconds: float
) -> dict[str, object]:
    if decode_wall_seconds <= 0:
        raise DecodeProfileError("decode wall time must be positive")
    encode_submit_us = int(cpu_profile["encode_submit_us"])
    share = encode_submit_us / (decode_wall_seconds * 1_000_000.0)
    return {
        "s8_eligible": share >= 0.10,
        "encode_submit_share": share,
        "threshold": 0.10,
        "reason": "encode_submit_material" if share >= 0.10 else "encode_submit_below_threshold",
    }


def evaluate_ab_pair(
    profiler_off: Mapping[str, object], profiler_on: Mapping[str, object]
) -> dict[str, object]:
    if profiler_off.get("output_sha256") != profiler_on.get("output_sha256"):
        raise DecodeProfileError("profiler off/on output token hashes differ")
    off_speed = float(profiler_off["decode_tokens_per_second"])
    on_speed = float(profiler_on["decode_tokens_per_second"])
    if off_speed <= 0 or on_speed <= 0:
        raise DecodeProfileError("profiler off/on throughput must be positive")
    overhead = max(0.0, (1.0 - on_speed / off_speed) * 100.0)
    return {
        "token_parity": True,
        "overhead_percent": overhead,
        "maximum_overhead_percent": MAX_PROFILER_OVERHEAD_PERCENT,
        "valid": overhead <= MAX_PROFILER_OVERHEAD_PERCENT,
    }


def evaluate_decode_stability(cell: Mapping[str, object]) -> dict[str, object]:
    speed = float(cell["decode_tokens_per_second"])
    warmup_speed = float(cell["warmup_decode_tokens_per_second"])
    speed_drift = abs(warmup_speed / speed - 1.0) * 100.0 if speed > 0 else float("inf")
    reclaim = int(cell["pageout_bytes_delta"]) + int(cell["swapin_bytes_delta"])
    parity = (
        cell.get("warmup_output_sha256") == cell.get("output_sha256")
        and cell.get("warmup_prompt_tokens_evaluated") == cell.get("decode_window_prompt_tokens")
        and 0 <= int(cell["decode_window_prompt_tokens"]) <= 4
    )
    valid = (
        reclaim <= MAX_DECODE_RECLAIM_BYTES
        and int(cell["swapouts_delta"]) == 0
        and int(cell["memory_free_percent_after"])
        >= int(cell["memory_free_percent_before"]) - MAX_MEMORY_PRESSURE_DROP_PERCENT
        and speed_drift <= MAX_WARMUP_SPEED_DRIFT_PERCENT
        and parity
    )
    return {
        "valid": valid,
        "decode_reclaim_bytes": reclaim,
        "speed_drift_percent": speed_drift,
        "token_parity": parity,
    }


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _read_log_segment(path: Path, start: int) -> list[str]:
    stable_size = -1
    for _ in range(20):
        size = path.stat().st_size
        if size == stable_size:
            break
        stable_size = size
        time.sleep(0.1)
    with path.open("rb") as stream:
        stream.seek(start)
        return stream.read().decode("utf-8", errors="replace").splitlines()


def _run_profile_cell(
    model: Path,
    output_dir: Path,
    *,
    server: Path,
    context: int,
    profiler_enabled: bool,
) -> dict[str, object]:
    port = _unused_port()
    command = build_profile_command(model, port, context, server=server)
    environment = profile_environment(PROFILE)
    environment["PGR_METAL_CPU_PROFILE"] = "1"
    if profiler_enabled:
        environment["PGR_METAL_PROFILE"] = "1"
        environment["PGR_METAL_PROFILE_COARSE"] = "1"
    else:
        environment.pop("PGR_METAL_PROFILE", None)
        environment.pop("PGR_METAL_PROFILE_COARSE", None)
    arm = "on" if profiler_enabled else "off"
    stem = f"ctx{context}-gpu-{arm}"
    stdout_path = output_dir / f"{stem}.stdout.log"
    stderr_path = output_dir / f"{stem}.stderr.log"

    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            env=environment,
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
                raise DecodeProfileError("token seed response is invalid")
            if len(seed) < context:
                raise DecodeProfileError("token seed is shorter than the S7 context")

            prefill = prefill_context(port, seed[:context])
            warmup = _run_point(
                process, port, seed[:context], cache_prompt=True, return_tokens=True
            )
            warmup_tokens = warmup.pop("output_tokens")
            assert isinstance(warmup_tokens, list)

            vm_before = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_before = parse_memory_pressure(_command_output(["memory_pressure"]))
            log_start = stderr_path.stat().st_size
            point = _run_point(
                process, port, seed[:context], cache_prompt=True, return_tokens=True
            )
            output_tokens = point.pop("output_tokens")
            assert isinstance(output_tokens, list)
            lines = _read_log_segment(stderr_path, log_start)
            vm_after = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_after = parse_memory_pressure(_command_output(["memory_pressure"]))
            if vm_before["page_size_bytes"] != vm_after["page_size_bytes"]:
                raise DecodeProfileError("vm_stat page size changed during an S7 cell")

            deltas = {
                f"{key}_delta": vm_after[key] - vm_before[key]
                for key in ("pageins", "pageouts", "swapins", "swapouts")
            }
            page_size = vm_before["page_size_bytes"]
            cell: dict[str, object] = {
                **point,
                **prefill,
                "profiler_enabled": profiler_enabled,
                "command": command,
                "slot_context_tokens": SWEEP_CONTEXT_SIZES[context],
                "cache_prompt": True,
                "warmup_decode_tokens": warmup["decoded_tokens"],
                "warmup_decode_tokens_per_second": warmup["decode_tokens_per_second"],
                "warmup_prompt_tokens_evaluated": warmup["prompt_tokens_evaluated"],
                "decode_window_prompt_tokens": point["prompt_tokens_evaluated"],
                "warmup_output_sha256": _token_sha256(warmup_tokens),
                "output_sha256": _token_sha256(output_tokens),
                "page_size_bytes": page_size,
                **deltas,
                "pageout_bytes_delta": max(0, deltas["pageouts_delta"]) * page_size,
                "swapin_bytes_delta": max(0, deltas["swapins_delta"]) * page_size,
                "memory_free_percent_before": pressure_before,
                "memory_free_percent_after": pressure_after,
            }
            cpu_records = parse_cpu_profile_records(lines)
            cell["cpu_profiles"] = {
                "s1": aggregate_cpu_records(cpu_records, query_tokens=1),
                "s5": aggregate_cpu_records(cpu_records, query_tokens=5),
            }
            cpu_path = output_dir / f"{stem}.cpu-records.json"
            _write_json_atomic(cpu_path, cpu_records)
            cell["cpu_records_sha256"] = _sha256(cpu_path)
            if profiler_enabled:
                gpu_records = parse_profile_records(lines)
                cell["gpu_profiles"] = {
                    "s1": aggregate_decode_records(gpu_records, query_tokens=1),
                    "s5": aggregate_decode_records(gpu_records, query_tokens=5),
                }
                gpu_path = output_dir / f"{stem}.gpu-records.json"
                _write_json_atomic(gpu_path, gpu_records)
                cell["gpu_records_sha256"] = _sha256(gpu_path)
            cell["stability"] = evaluate_decode_stability(cell)
            return cell
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def run_decode_profile(
    model: Path,
    output_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
) -> dict[str, object]:
    manifest = _read_manifest(model)
    output_dir.mkdir(parents=True, exist_ok=False)
    wired_limit_mb = int(
        _command_output(["/usr/sbin/sysctl", "-n", "iogpu.wired_limit_mb"]).strip()
    )
    host_gate = evaluate_host_gate(wired_limit_mb)
    if not host_gate["ready"]:
        raise DecodeProfileError(
            f"host gate failed: iogpu.wired_limit_mb={wired_limit_mb}; expected 28672"
        )
    llama_cpp_commit = subprocess.check_output(
        ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
        text=True,
    ).strip()
    identity = {
        "model_sha256": manifest["sha256"],
        "llama_cpp_commit": llama_cpp_commit,
        "server_sha256": _sha256(server),
        "host_gate": host_gate,
    }
    cells: list[dict[str, object]] = []
    for context in CONTEXTS:
        for profiler_enabled in (False, True):
            cell = _run_profile_cell(
                model,
                output_dir,
                server=server,
                context=context,
                profiler_enabled=profiler_enabled,
            )
            cells.append(cell)
            _write_json_atomic(
                output_dir / "partial-cells.json",
                {"schema": 1, **identity, "runtime_profile": asdict(PROFILE), "cells": cells},
            )

    ab_pairs: dict[str, dict[str, object]] = {}
    for context in CONTEXTS:
        off = next(cell for cell in cells if cell["context_tokens"] == context and not cell["profiler_enabled"])
        on = next(cell for cell in cells if cell["context_tokens"] == context and cell["profiler_enabled"])
        ab_pairs[str(context)] = evaluate_ab_pair(off, on)

    long_off = next(cell for cell in cells if cell["context_tokens"] == 64_000 and not cell["profiler_enabled"])
    long_on = next(cell for cell in cells if cell["context_tokens"] == 64_000 and cell["profiler_enabled"])
    w2_decision = decide_w2_eligibility(long_on["gpu_profiles"]["s5"])
    long_cpu = long_off["cpu_profiles"]
    s8_decision = decide_s8_eligibility(
        {
            "encode_submit_us": int(long_cpu["s1"]["encode_submit_us"])
            + int(long_cpu["s5"]["encode_submit_us"])
        },
        float(long_off["wall_seconds"]),
    )
    base_valid = all(bool(cell["stability"]["valid"]) for cell in cells)
    ab_valid = all(bool(pair["valid"]) for pair in ab_pairs.values())
    measurement_valid = base_valid and ab_valid
    report: dict[str, object] = {
        "schema": 2,
        "instrumentation": {
            "enabled_by": "PGR_METAL_PROFILE=1",
            "default_enabled": False,
            "counter": "MTLCommonCounterSetTimestamp",
            "sampling_point": "MTLCounterSamplingPointAtStageBoundary",
            "boundary": "decision pass uses contiguous attention-versus-rest runs; all dispatches covered",
            "classification": "coarse attention versus residual GPU work for W2 decision",
            "minimum_coverage": MINIMUM_COVERAGE,
            "decode_query_token_limit": DECODE_QUERY_LIMIT,
            "attention_dominance_threshold": 0.5,
            "cpu_timing": "PGR_METAL_CPU_PROFILE=1; graph encode/submit and synchronize wait",
            "other_semantics": "residual GPU operations only; never CPU dispatch",
        },
        "runtime_profile": asdict(PROFILE),
        **identity,
        "cells": cells,
        "ab_pairs": ab_pairs,
        "w2_decision": w2_decision,
        "w2_decision_basis": "64K S=5 production MTP4 verify path",
        "s8_decision": s8_decision,
        "s8_decision_basis": "64K profiler-off S=1+S=5 encode/submit CPU wall",
        "measurement_valid": measurement_valid,
        "m0a_admitted_tokens": 0,
    }
    report["w2_gate"] = "ELIGIBLE" if measurement_valid and w2_decision["w2_eligible"] else "REJECTED" if measurement_valid else "PENDING_VALID_RERUN"
    report["s8_gate"] = "ELIGIBLE" if measurement_valid and s8_decision["s8_eligible"] else "REJECTED" if measurement_valid else "PENDING_VALID_RERUN"
    _write_json_atomic(output_dir / "decode-profile.json", report)
    return report


def append_results(path: Path, report: Mapping[str, object], evidence_hash: str) -> None:
    marker = f"S7 evidence SHA-256: `{evidence_hash}`"
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    if marker in existing:
        raise DecodeProfileError("S7 evidence is already present in RESULTS")
    cells = report.get("cells")
    w2 = report.get("w2_decision")
    s8 = report.get("s8_decision")
    ab_pairs = report.get("ab_pairs")
    if (
        not isinstance(cells, list)
        or not isinstance(w2, Mapping)
        or not isinstance(s8, Mapping)
        or not isinstance(ab_pairs, Mapping)
    ):
        raise DecodeProfileError("S7 report shape is invalid")
    lines = [
        "\n## Track S7 production decode-time profile\n",
        f"- {marker}",
        "- Fixed f16/FA/MTP4 at 4K and 64K; isolated prefill/warmup/measured decode; profiler off/on A/B; 0 M0a-admitted tokens",
        "- `other` is residual GPU work only and is not CPU dispatch overhead",
    ]
    for cell in cells:
        arm = "on" if cell["profiler_enabled"] else "off"
        lines.append(
            f"- {cell['context_tokens']:,} profiler {arm}: {cell['decode_tokens_per_second']:.4f} tok/s, "
            f"wall {cell['wall_seconds']:.3f} s, stability {'PASS' if cell['stability']['valid'] else 'FAIL'}"
        )
        if cell["profiler_enabled"]:
            for name, profile in cell["gpu_profiles"].items():
                shares = profile["shares"]
                category_text = ", ".join(
                    f"{category} {share:.2%}" for category, share in shares.items()
                )
                lines.append(
                    f"  - {name.upper()} GPU: {category_text}; coverage {profile['coverage']:.2%} "
                    f"({profile['sampled_dispatches']}/{profile['total_dispatches']})"
                )
        else:
            cpu = cell["cpu_profiles"]
            lines.append(
                f"  - CPU S1+S5 encode/submit {cpu['s1']['encode_submit_us'] + cpu['s5']['encode_submit_us']} us; "
                f"commit {cpu['s1']['commit_us'] + cpu['s5']['commit_us']} us; "
                f"wait {cpu['s1']['wait_us'] + cpu['s5']['wait_us']} us"
            )
    for context, pair in ab_pairs.items():
        lines.append(
            f"- {int(context):,} profiler overhead: {pair['overhead_percent']:.2f}% "
            f"(limit {pair['maximum_overhead_percent']:.2f}%) — {'PASS' if pair['valid'] else 'FAIL'}"
        )
    lines.extend(
        [
            f"- W2 64K/S=5: dominant `{w2['dominant_category']}`, attention {w2['attention_share']:.2%}; gate **{report['w2_gate']}**",
            f"- S8 64K profiler-off encode/submit share: {s8['encode_submit_share']:.2%}; gate **{report['s8_gate']}**",
            f"- Measurement valid: **{'YES' if report['measurement_valid'] else 'NO'}**",
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
    args = parser.parse_args()
    try:
        report = run_decode_profile(args.model, args.output_dir, server=args.server)
        evidence_path = args.output_dir / "decode-profile.json"
        append_results(args.results, report, _sha256(evidence_path))
    except (DecodeProfileError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"decode profile failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["measurement_valid"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
