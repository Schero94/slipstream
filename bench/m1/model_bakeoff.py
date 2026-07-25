"""Run the fail-closed 4K admission smoke for an 18 GB Track M candidate."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time
from typing import Mapping

from bench.m0a.qualify_model import TOKEN_SEED_TEXT, _run_point, production_environment
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    SmokeError,
    _json_request,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)
from bench.m1.headroom import _command_output, parse_memory_pressure, parse_vm_stat


TRACK_M_MAX_RSS_KB = 18_000_000
TRACK_M_CONTEXT_TOKENS = 4_000
TRACK_M_CONTEXT_SPEED_FLOORS = {4_000: 24.0, 32_000: 24.0, 64_000: 20.0}
H1_MAX_RSS_KB = 31_000_000
H1_MIN_TOKENS_PER_SECOND = 8.0


class ModelBakeoffError(RuntimeError):
    pass


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def verify_candidate(
    path: Path, *, expected_size: int, expected_sha256: str
) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise ValueError("candidate must be a regular non-symlink file")
    if expected_size <= 0:
        raise ValueError("expected_size must be positive")
    if len(expected_sha256) != 64 or any(
        character not in "0123456789abcdef" for character in expected_sha256
    ):
        raise ValueError("expected_sha256 must be lowercase hexadecimal SHA-256")
    size = path.stat().st_size
    if size != expected_size:
        raise ValueError(f"candidate size mismatch: {size} != {expected_size}")
    actual_sha256 = _sha256(path)
    if actual_sha256 != expected_sha256:
        raise ValueError("candidate SHA-256 mismatch")
    return {
        "path": str(path.resolve()),
        "bytes": size,
        "sha256": actual_sha256,
    }


def track_m_server_command(
    model: Path,
    port: int,
    *,
    server: Path = DEFAULT_SERVER,
    gpu_layers: int = 99,
    context_tokens: int = TRACK_M_CONTEXT_TOKENS,
) -> list[str]:
    if gpu_layers < 0:
        raise ValueError("gpu_layers must be non-negative")
    if context_tokens not in TRACK_M_CONTEXT_SPEED_FLOORS:
        raise ValueError("unsupported Track M context point")
    context_size = {4_000: 8_192, 32_000: 32_768, 64_000: 65_536}[
        context_tokens
    ]
    return [
        str(server),
        "--model",
        str(model),
        "--host",
        "127.0.0.1",
        "--port",
        str(port),
        "--parallel",
        "1",
        "--ctx-size",
        str(context_size),
        "--fit",
        "off",
        "--gpu-layers",
        str(gpu_layers),
        "--no-warmup",
        "--spec-type",
        "none",
        "--flash-attn",
        "on",
        "--cache-type-k",
        "f16",
        "--cache-type-v",
        "f16",
        "--temp",
        "0",
        "--alias",
        "peregrine-track-m",
    ]


def evaluate_track_m_smoke(
    point: Mapping[str, object], memory: Mapping[str, object]
) -> dict[str, object]:
    reasons: list[str] = []
    context = point.get("context_tokens")
    if context not in TRACK_M_CONTEXT_SPEED_FLOORS:
        reasons.append("context")
    if point.get("decoded_tokens") != 128 or point.get("stop_type") != "limit":
        reasons.append("decode-count")
    try:
        speed = float(point["decode_tokens_per_second"])
        rss = int(point["peak_rss_kb"])
        pageouts = int(memory["pageouts_delta"])
        swapouts = int(memory["swapouts_delta"])
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError("Track M smoke evidence is malformed") from error
    minimum_speed = TRACK_M_CONTEXT_SPEED_FLOORS.get(
        context, TRACK_M_CONTEXT_SPEED_FLOORS[TRACK_M_CONTEXT_TOKENS]
    )
    if speed < minimum_speed:
        reasons.append("throughput")
    if rss <= 0 or rss > TRACK_M_MAX_RSS_KB:
        reasons.append("rss")
    if pageouts != 0:
        reasons.append("pageouts")
    if swapouts != 0:
        reasons.append("swapouts")
    pressure_after = memory.get("free_percent_after")
    if pressure_after is not None and int(pressure_after) < 10:
        reasons.append("memory-pressure")
    return {
        "decision": "PASS" if not reasons else "FAIL",
        "reasons": reasons,
        "maximum_peak_rss_kb": TRACK_M_MAX_RSS_KB,
        "minimum_decode_tokens_per_second": minimum_speed,
        "m0a_admitted_tokens": 0,
    }


def evaluate_h1_smoke(
    point: Mapping[str, object], memory: Mapping[str, object]
) -> dict[str, object]:
    """Evaluate the rare, on-demand dense hard-case tier independently."""

    reasons: list[str] = []
    if point.get("context_tokens") != TRACK_M_CONTEXT_TOKENS:
        reasons.append("context")
    if point.get("decoded_tokens") != 128 or point.get("stop_type") != "limit":
        reasons.append("decode-count")
    try:
        speed = float(point["decode_tokens_per_second"])
        rss = int(point["peak_rss_kb"])
        pageouts = int(memory["pageouts_delta"])
        swapouts = int(memory["swapouts_delta"])
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError("H1 smoke evidence is malformed") from error
    if speed < H1_MIN_TOKENS_PER_SECOND:
        reasons.append("throughput")
    if rss <= 0 or rss > H1_MAX_RSS_KB:
        reasons.append("rss")
    if pageouts != 0:
        reasons.append("pageouts")
    if swapouts != 0:
        reasons.append("swapouts")
    pressure_after = memory.get("free_percent_after")
    if pressure_after is not None and int(pressure_after) < 10:
        reasons.append("memory-pressure")
    return {
        "decision": "PASS" if not reasons else "FAIL",
        "reasons": reasons,
        "gate": "h1-hard-case-v1",
        "maximum_peak_rss_kb": H1_MAX_RSS_KB,
        "minimum_decode_tokens_per_second": H1_MIN_TOKENS_PER_SECOND,
        "m0a_admitted_tokens": 0,
    }


def _memory_window(
    before: Mapping[str, int],
    after: Mapping[str, int],
    *,
    pressure_before: int,
    pressure_after: int,
) -> dict[str, int]:
    if before["page_size_bytes"] != after["page_size_bytes"]:
        raise ModelBakeoffError("vm_stat page size changed")
    deltas = {
        f"{name}_delta": after[name] - before[name]
        for name in ("pageins", "pageouts", "swapins", "swapouts")
    }
    if any(value < 0 for value in deltas.values()):
        raise ModelBakeoffError("vm_stat counter moved backwards")
    return {
        "page_size_bytes": before["page_size_bytes"],
        **deltas,
        "free_percent_before": pressure_before,
        "free_percent_after": pressure_after,
    }


def run_track_m_smoke(
    model: Path,
    output_dir: Path,
    *,
    expected_size: int,
    expected_sha256: str,
    server: Path = DEFAULT_SERVER,
    gpu_layers: int = 99,
    context_tokens: int = TRACK_M_CONTEXT_TOKENS,
    gate: str = "track-m",
) -> dict[str, object]:
    if gate not in {"track-m", "h1"}:
        raise ValueError("unsupported model smoke gate")
    if gate == "h1" and context_tokens != TRACK_M_CONTEXT_TOKENS:
        raise ValueError("H1 smoke requires the 4K context point")
    identity = verify_candidate(
        model, expected_size=expected_size, expected_sha256=expected_sha256
    )
    output_dir.mkdir(parents=True, exist_ok=False)
    startup_before = parse_vm_stat(_command_output(["vm_stat"]))
    startup_pressure_before = parse_memory_pressure(
        _command_output(["memory_pressure"])
    )
    port = _unused_port()
    command = track_m_server_command(
        model,
        port,
        server=server,
        gpu_layers=gpu_layers,
        context_tokens=context_tokens,
    )
    stdout_path = output_dir / "server.stdout.log"
    stderr_path = output_dir / "server.stderr.log"
    point: dict[str, object] | None = None
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            env=production_environment(),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            _wait_for_health(process, port)
            tokenized = _json_request(
                f"http://127.0.0.1:{port}/tokenize",
                {
                    "content": TOKEN_SEED_TEXT,
                    "add_special": False,
                    "parse_special": True,
                },
            )
            seed = tokenized.get("tokens")
            if not isinstance(seed, list) or len(seed) < context_tokens:
                raise ModelBakeoffError("token seed is shorter than context point")
            # Server health does not materialize mmap-backed model pages when
            # `--no-warmup` is set. Run one tiny inference before opening the
            # resident-service measurement window so first-use faults remain
            # startup evidence rather than being mislabeled as steady-state.
            _json_request(
                f"http://127.0.0.1:{port}/completion",
                {
                    "prompt": seed[:16],
                    "n_predict": 1,
                    "ignore_eos": True,
                    "temperature": 0,
                    "seed": 42,
                    "stream": False,
                    "cache_prompt": False,
                },
                timeout=3600,
            )
            startup_after = parse_vm_stat(_command_output(["vm_stat"]))
            startup_pressure_after = parse_memory_pressure(
                _command_output(["memory_pressure"])
            )
            # Materialize the complete prompt outside the decode window.  H1's
            # contract is specifically zero paging *during decode*; wrapping a
            # cold 4K prefill and decode in one vm_stat interval mislabeled
            # prefill faults as decode pressure.  The measured request reuses
            # this exact prompt through llama.cpp's slot prompt cache.
            prefill_before = parse_vm_stat(_command_output(["vm_stat"]))
            prefill_pressure_before = parse_memory_pressure(
                _command_output(["memory_pressure"])
            )
            _json_request(
                f"http://127.0.0.1:{port}/completion",
                {
                    "prompt": seed[:context_tokens],
                    "n_predict": 1,
                    "ignore_eos": True,
                    "temperature": 0,
                    "seed": 42,
                    "stream": False,
                    "cache_prompt": True,
                },
                timeout=3600,
            )
            prefill_after = parse_vm_stat(_command_output(["vm_stat"]))
            prefill_pressure_after = parse_memory_pressure(
                _command_output(["memory_pressure"])
            )
            # The product path is a resident gateway. Let post-prefill
            # accounting settle, then measure cached decode in its own hard
            # paging window.
            time.sleep(2.0)
            decode_before = parse_vm_stat(_command_output(["vm_stat"]))
            decode_pressure_before = parse_memory_pressure(
                _command_output(["memory_pressure"])
            )
            point = _run_point(
                process, port, seed[:context_tokens], cache_prompt=True
            )
            decode_after = parse_vm_stat(_command_output(["vm_stat"]))
            decode_pressure_after = parse_memory_pressure(
                _command_output(["memory_pressure"])
            )
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
    if point is None:
        raise ModelBakeoffError("Track M smoke produced no point")
    startup_memory = _memory_window(
        startup_before,
        startup_after,
        pressure_before=startup_pressure_before,
        pressure_after=startup_pressure_after,
    )
    decode_memory = _memory_window(
        decode_before,
        decode_after,
        pressure_before=decode_pressure_before,
        pressure_after=decode_pressure_after,
    )
    prefill_memory = _memory_window(
        prefill_before,
        prefill_after,
        pressure_before=prefill_pressure_before,
        pressure_after=prefill_pressure_after,
    )
    decision = (
        evaluate_h1_smoke(point, decode_memory)
        if gate == "h1"
        else evaluate_track_m_smoke(point, decode_memory)
    )
    startup_warnings = [
        name
        for name in ("pageouts", "swapouts")
        if int(startup_memory[f"{name}_delta"]) > 0
    ]
    report: dict[str, object] = {
        "schema": 1,
        "candidate": identity,
        "command": command,
        "point": point,
        "measurement_mode": "resident-cached-decode-v2",
        "startup_memory": startup_memory,
        "startup_warnings": startup_warnings,
        "prefill_memory": prefill_memory,
        "memory": decode_memory,
        **decision,
    }
    report_name = "h1-smoke.json" if gate == "h1" else "track-m-smoke.json"
    _write_json_atomic(output_dir / report_name, report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--expected-size", required=True, type=int)
    parser.add_argument("--expected-sha256", required=True)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument("--gpu-layers", type=int, default=99)
    parser.add_argument("--gate", choices=("track-m", "h1"), default="track-m")
    parser.add_argument(
        "--context-tokens",
        type=int,
        choices=tuple(TRACK_M_CONTEXT_SPEED_FLOORS),
        default=TRACK_M_CONTEXT_TOKENS,
    )
    args = parser.parse_args()
    try:
        report = run_track_m_smoke(
            args.model,
            args.output_dir,
            expected_size=args.expected_size,
            expected_sha256=args.expected_sha256,
            server=args.server,
            gpu_layers=args.gpu_layers,
            context_tokens=args.context_tokens,
            gate=args.gate,
        )
    except (ModelBakeoffError, SmokeError, OSError, ValueError) as error:
        print(f"Track M smoke failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["decision"] == "PASS" else 2


if __name__ == "__main__":
    raise SystemExit(main())
