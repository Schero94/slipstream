"""Measure W3 generator-plus-verifier continuous decode batching."""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import asdict
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import threading
import time
from typing import Iterator, Mapping

from bench.m0a.qualify_model import (
    QUALIFICATION_PROFILES,
    TOKEN_SEED_TEXT,
    _read_manifest,
    profile_environment,
    qualification_server_command,
)
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    _json_request,
    _monitor_rss,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)
from bench.m1.headroom import _command_output, parse_memory_pressure, parse_vm_stat


PROFILE = QUALIFICATION_PROFILES["baseline-f16-fa-mtp4"]
REPETITIONS = 3
DECODE_TOKENS = 128
AGGREGATE_FACTOR_MIN = 1.6
GENERATOR_OVERHEAD_MAX_PERCENT = 15.0
MAX_RECLAIM_BYTES = 512 * 1024
MAX_PRESSURE_DROP_PERCENT = 1
MAX_PROMPT_TOKENS_EVALUATED = 4
HEX64 = re.compile(r"^[0-9a-f]{64}$")
REPO_ROOT = Path(__file__).resolve().parents[2]
GENERATOR_CONTEXT = 30_000
VERIFIER_CONTEXT = 4_000
SUFFIXES = {
    "generator": "\nImplement the smallest correct patch and run the focused tests.",
    "verifier": "\nInspect the proposed patch and identify any contract violation.",
}


class W3Error(RuntimeError):
    pass


def build_server_command(
    model: Path,
    port: int,
    *,
    parallel: int,
    server: Path = DEFAULT_SERVER,
) -> list[str]:
    if parallel not in (1, 2):
        raise W3Error("W3 supports only one or two server slots")
    command = qualification_server_command(
        model, port, server=server, profile=PROFILE
    )
    command[command.index("--parallel") + 1] = str(parallel)
    command[command.index("--ctx-size") + 1] = "32768" if parallel == 1 else "65536"
    if "--cont-batching" not in command:
        command.append("--cont-batching")
    return command


def build_request(prompt_tokens: list[int], *, slot: int) -> dict[str, object]:
    if slot not in (0, 1):
        raise W3Error("W3 slot ID must be zero or one")
    if not prompt_tokens or not all(
        isinstance(token, int) and not isinstance(token, bool) for token in prompt_tokens
    ):
        raise W3Error("W3 prompt tokens must be a non-empty integer list")
    return {
        "prompt": prompt_tokens,
        "id_slot": slot,
        "n_predict": DECODE_TOKENS,
        "ignore_eos": True,
        "temperature": 0,
        "seed": 42,
        "stream": False,
        "cache_prompt": True,
        "return_tokens": True,
    }


def _token_sha256(tokens: list[int]) -> str:
    return hashlib.sha256(
        json.dumps(tokens, separators=(",", ":")).encode("ascii")
    ).hexdigest()


def parse_completion(
    name: str,
    response: Mapping[str, object],
    *,
    wall_seconds: float,
) -> dict[str, object]:
    if name not in SUFFIXES:
        raise W3Error("unknown W3 stream")
    timings = response.get("timings")
    tokens = response.get("tokens")
    if not isinstance(timings, Mapping) or not isinstance(tokens, list):
        raise W3Error(f"{name} completion lacks timings or tokens")
    if (
        len(tokens) != DECODE_TOKENS
        or not all(isinstance(token, int) and not isinstance(token, bool) for token in tokens)
        or timings.get("predicted_n") != DECODE_TOKENS
        or response.get("stop_type") != "limit"
    ):
        raise W3Error(f"{name} completion has incomplete decode evidence")
    prompt_n = _integer(timings.get("prompt_n"), "prompt tokens evaluated")
    if prompt_n > MAX_PROMPT_TOKENS_EVALUATED:
        raise W3Error(f"{name} completion did not isolate decode")
    return {
        "name": name,
        "decoded_tokens": DECODE_TOKENS,
        "wall_seconds": _number(wall_seconds, "wall seconds"),
        "server_predicted_ms": _number(
            timings.get("predicted_ms"), "server predicted milliseconds"
        ),
        "prompt_tokens_evaluated": prompt_n,
        "stop_type": response.get("stop_type"),
        "output_sha256": _token_sha256(tokens),
        "output_tokens": list(tokens),
    }


def _number(value: object, label: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise W3Error(f"{label} is not numeric")
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise W3Error(f"{label} must be finite and positive")
    return number


def _integer(value: object, label: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise W3Error(f"{label} is not a valid integer")
    return value


def _stream(value: object, expected_name: str) -> dict[str, object]:
    if not isinstance(value, Mapping) or value.get("name") != expected_name:
        raise W3Error(f"{expected_name} stream is missing")
    decoded = _integer(value.get("decoded_tokens"), "decoded tokens", minimum=1)
    if decoded != DECODE_TOKENS or value.get("stop_type") != "limit":
        raise W3Error(f"{expected_name} stream has incomplete decode evidence")
    prompt_n = _integer(
        value.get("prompt_tokens_evaluated"), "prompt tokens evaluated"
    )
    if prompt_n > MAX_PROMPT_TOKENS_EVALUATED:
        raise W3Error(f"{expected_name} stream did not isolate decode")
    output_hash = value.get("output_sha256")
    if not isinstance(output_hash, str) or HEX64.fullmatch(output_hash) is None:
        raise W3Error(f"{expected_name} stream has invalid output hash")
    return {
        **dict(value),
        "wall_seconds": _number(value.get("wall_seconds"), "wall seconds"),
        "server_predicted_ms": _number(
            value.get("server_predicted_ms"), "server predicted milliseconds"
        ),
    }


def _memory(value: object) -> dict[str, int]:
    if not isinstance(value, Mapping):
        raise W3Error("memory evidence is missing")
    fields = {
        key: _integer(value.get(key), key)
        for key in (
            "page_size_bytes",
            "pageouts_delta",
            "pageout_bytes_delta",
            "swapins_delta",
            "swapin_bytes_delta",
            "swapouts_delta",
            "free_percent_before",
            "free_percent_after",
            "peak_rss_kb",
        )
    }
    if fields["page_size_bytes"] <= 0 or fields["peak_rss_kb"] <= 0:
        raise W3Error("memory evidence has invalid page size or RSS")
    if fields["pageout_bytes_delta"] != fields["pageouts_delta"] * fields["page_size_bytes"]:
        raise W3Error("pageout byte evidence is inconsistent")
    if fields["swapin_bytes_delta"] != fields["swapins_delta"] * fields["page_size_bytes"]:
        raise W3Error("swapin byte evidence is inconsistent")
    reclaim = fields["pageout_bytes_delta"] + fields["swapin_bytes_delta"]
    if (
        reclaim > MAX_RECLAIM_BYTES
        or fields["swapouts_delta"] != 0
        or fields["free_percent_after"]
        < fields["free_percent_before"] - MAX_PRESSURE_DROP_PERCENT
    ):
        raise W3Error("W3 measurement paged or lost memory headroom")
    return fields


def _invalid(reason: str) -> dict[str, object]:
    return {
        "valid": False,
        "decision": "W3_INVALID",
        "reason": reason,
        "aggregate_factor": None,
        "generator_latency_overhead_percent": None,
        "repetitions": [],
        "thresholds": {
            "aggregate_factor_min": AGGREGATE_FACTOR_MIN,
            "generator_overhead_max_percent": GENERATOR_OVERHEAD_MAX_PERCENT,
        },
    }


def evaluate_report(report: Mapping[str, object]) -> dict[str, object]:
    try:
        if report.get("schema") != 1:
            raise W3Error("unsupported W3 report schema")
        repetitions = report.get("repetitions")
        if not isinstance(repetitions, list) or len(repetitions) != REPETITIONS:
            raise W3Error("W3 requires exactly three repetitions")
        rows: list[dict[str, object]] = []
        parity_errors: list[str] = []
        isolated_generator_wall = 0.0
        concurrent_window_wall = 0.0
        concurrent_generator_wall = 0.0
        for expected_index, repetition in enumerate(repetitions):
            if not isinstance(repetition, Mapping) or repetition.get("index") != expected_index:
                raise W3Error("W3 repetition indices are incomplete")
            isolated = repetition.get("isolated")
            concurrent = repetition.get("concurrent")
            if not isinstance(isolated, Mapping) or not isinstance(concurrent, Mapping):
                raise W3Error("W3 repetition arms are missing")
            iso_gen = _stream(isolated.get("generator"), "generator")
            _stream(isolated.get("verifier"), "verifier")
            con_gen = _stream(concurrent.get("generator"), "generator")
            con_ver = _stream(concurrent.get("verifier"), "verifier")
            _memory(repetition.get("memory"))
            if iso_gen["output_sha256"] != con_gen["output_sha256"]:
                parity_errors.append(f"repetition {expected_index} generator output parity failed")
            iso_ver = _stream(isolated.get("verifier"), "verifier")
            if iso_ver["output_sha256"] != con_ver["output_sha256"]:
                parity_errors.append(f"repetition {expected_index} verifier output parity failed")
            window = _number(
                concurrent.get("window_wall_seconds"), "concurrent window seconds"
            )
            iso_gen_wall = float(iso_gen["wall_seconds"])
            con_gen_wall = float(con_gen["wall_seconds"])
            factor = (2 * DECODE_TOKENS / window) / (DECODE_TOKENS / iso_gen_wall)
            overhead = (con_gen_wall / iso_gen_wall - 1.0) * 100.0
            passed = (
                factor >= AGGREGATE_FACTOR_MIN
                and overhead <= GENERATOR_OVERHEAD_MAX_PERCENT
            )
            rows.append(
                {
                    "index": expected_index,
                    "aggregate_factor": factor,
                    "generator_latency_overhead_percent": overhead,
                    "passed": passed,
                }
            )
            isolated_generator_wall += iso_gen_wall
            concurrent_window_wall += window
            concurrent_generator_wall += con_gen_wall
        aggregate_factor = (
            (2 * DECODE_TOKENS * REPETITIONS / concurrent_window_wall)
            / (DECODE_TOKENS * REPETITIONS / isolated_generator_wall)
        )
        overhead = (
            concurrent_generator_wall / isolated_generator_wall - 1.0
        ) * 100.0
        passed = (
            aggregate_factor >= AGGREGATE_FACTOR_MIN
            and overhead <= GENERATOR_OVERHEAD_MAX_PERCENT
            and all(bool(row["passed"]) for row in rows)
        )
        if parity_errors:
            return {
                "valid": False,
                "decision": "W3_INVALID",
                "reason": "; ".join(parity_errors),
                "aggregate_factor": aggregate_factor,
                "generator_latency_overhead_percent": overhead,
                "repetitions": rows,
                "thresholds": {
                    "aggregate_factor_min": AGGREGATE_FACTOR_MIN,
                    "generator_overhead_max_percent": GENERATOR_OVERHEAD_MAX_PERCENT,
                },
            }
        return {
            "valid": True,
            "decision": "W3_PASS" if passed else "W3_REJECTED",
            "reason": "all_gates_passed" if passed else "performance_gate_failed",
            "aggregate_factor": aggregate_factor,
            "generator_latency_overhead_percent": overhead,
            "repetitions": rows,
            "thresholds": {
                "aggregate_factor_min": AGGREGATE_FACTOR_MIN,
                "generator_overhead_max_percent": GENERATOR_OVERHEAD_MAX_PERCENT,
            },
        }
    except (KeyError, TypeError, ValueError, W3Error) as error:
        return _invalid(str(error))


def runtime_profile() -> dict[str, object]:
    return asdict(PROFILE)


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _tokenize(port: int, text: str) -> list[int]:
    response = _json_request(
        f"http://127.0.0.1:{port}/tokenize",
        {"content": text, "add_special": False, "parse_special": True},
        timeout=3600,
    )
    tokens = response.get("tokens")
    if not isinstance(tokens, list) or not all(
        isinstance(token, int) and not isinstance(token, bool) for token in tokens
    ):
        raise W3Error("tokenizer returned invalid tokens")
    return tokens


def _prompts(port: int) -> dict[str, list[int]]:
    seed = _tokenize(port, TOKEN_SEED_TEXT)
    prompts: dict[str, list[int]] = {}
    for name, length in (
        ("generator", GENERATOR_CONTEXT),
        ("verifier", VERIFIER_CONTEXT),
    ):
        suffix = _tokenize(port, SUFFIXES[name])
        if not suffix or len(suffix) >= length or len(seed) < length:
            raise W3Error(f"cannot construct exact {name} context")
        prompts[name] = seed[: length - len(suffix)] + suffix
        if len(prompts[name]) != length:
            raise W3Error(f"{name} prompt has the wrong length")
    return prompts


def _prefill(port: int, name: str, prompt: list[int], slot: int) -> dict[str, object]:
    response = _json_request(
        f"http://127.0.0.1:{port}/completion",
        {
            "prompt": prompt,
            "id_slot": slot,
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
    if (
        not isinstance(timings, Mapping)
        or timings.get("prompt_n") != len(prompt)
        or timings.get("predicted_n") != 1
        or response.get("tokens_cached") != len(prompt)
        or response.get("stop_type") != "limit"
    ):
        raise W3Error(f"{name} prefill has incomplete token evidence")
    return {
        "name": name,
        "prompt_tokens": len(prompt),
        "prompt_ms": _number(timings.get("prompt_ms"), "prefill milliseconds"),
    }


def _request_completion(port: int, name: str, prompt: list[int], slot: int) -> dict[str, object]:
    started = time.monotonic()
    response = _json_request(
        f"http://127.0.0.1:{port}/completion",
        build_request(prompt, slot=slot),
        timeout=3600,
    )
    return parse_completion(name, response, wall_seconds=time.monotonic() - started)


def _prepare_slot(port: int, name: str, prompt: list[int], slot: int) -> dict[str, object]:
    prefill = _prefill(port, name, prompt, slot)
    warmup = _request_completion(port, name, prompt, slot)
    return {"prefill": prefill, "warmup": warmup}


@contextmanager
def _server(
    model: Path,
    output_dir: Path,
    name: str,
    *,
    parallel: int,
    server: Path,
) -> Iterator[tuple[subprocess.Popen[bytes], int, list[str]]]:
    port = _unused_port()
    command = build_server_command(model, port, parallel=parallel, server=server)
    stdout_path = output_dir / f"{name}.stdout.log"
    stderr_path = output_dir / f"{name}.stderr.log"
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            env=profile_environment(PROFILE),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            _wait_for_health(process, port)
            yield process, port, command
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def _isolated_arm(
    model: Path,
    output_dir: Path,
    *,
    server: Path,
) -> tuple[list[dict[str, object]], list[str], dict[str, str]]:
    repetitions: list[dict[str, object]] = []
    with _server(model, output_dir, "isolated-server", parallel=1, server=server) as (
        _process,
        port,
        command,
    ):
        prompts = _prompts(port)
        prompt_hashes = {name: _token_sha256(prompt) for name, prompt in prompts.items()}
        for index in range(REPETITIONS):
            streams: dict[str, object] = {}
            preparation: dict[str, object] = {}
            for name in ("generator", "verifier"):
                preparation[name] = _prepare_slot(port, name, prompts[name], 0)
                streams[name] = _request_completion(port, name, prompts[name], 0)
            repetitions.append(
                {"index": index, "streams": streams, "preparation": preparation}
            )
    return repetitions, command, prompt_hashes


def _vm_memory(
    before: Mapping[str, int],
    after: Mapping[str, int],
    *,
    pressure_before: int,
    pressure_after: int,
    peak_rss_kb: int,
) -> dict[str, int]:
    if before["page_size_bytes"] != after["page_size_bytes"]:
        raise W3Error("vm_stat page size changed during W3")
    page_size = before["page_size_bytes"]
    deltas = {
        f"{name}_delta": after[name] - before[name]
        for name in ("pageins", "pageouts", "swapins", "swapouts")
    }
    if any(value < 0 for value in deltas.values()):
        raise W3Error("vm_stat counter moved backwards")
    return {
        "page_size_bytes": page_size,
        **deltas,
        "pageout_bytes_delta": deltas["pageouts_delta"] * page_size,
        "swapin_bytes_delta": deltas["swapins_delta"] * page_size,
        "free_percent_before": pressure_before,
        "free_percent_after": pressure_after,
        "peak_rss_kb": peak_rss_kb,
    }


def _concurrent_pair(
    process: subprocess.Popen[bytes],
    port: int,
    prompts: Mapping[str, list[int]],
) -> tuple[dict[str, object], dict[str, int]]:
    before = parse_vm_stat(_command_output(["vm_stat"]))
    pressure_before = parse_memory_pressure(_command_output(["memory_pressure"]))
    maximum = [0]
    stop_monitor = threading.Event()
    monitor = threading.Thread(
        target=_monitor_rss, args=(process, stop_monitor, maximum), daemon=True
    )
    barrier_started: list[float] = []
    barrier = threading.Barrier(3, action=lambda: barrier_started.append(time.monotonic()))

    def request(name: str, slot: int) -> dict[str, object]:
        barrier.wait(timeout=30)
        return _request_completion(port, name, prompts[name], slot)

    monitor.start()
    try:
        with ThreadPoolExecutor(max_workers=2, thread_name_prefix="w3") as executor:
            futures = {
                "generator": executor.submit(request, "generator", 0),
                "verifier": executor.submit(request, "verifier", 1),
            }
            barrier.wait(timeout=30)
            streams = {name: future.result(timeout=3600) for name, future in futures.items()}
        ended = time.monotonic()
    finally:
        stop_monitor.set()
        monitor.join(timeout=2)
    if len(barrier_started) != 1:
        raise W3Error("concurrent request barrier did not start exactly once")
    after = parse_vm_stat(_command_output(["vm_stat"]))
    pressure_after = parse_memory_pressure(_command_output(["memory_pressure"]))
    memory = _vm_memory(
        before,
        after,
        pressure_before=pressure_before,
        pressure_after=pressure_after,
        peak_rss_kb=maximum[0],
    )
    return {
        **streams,
        "window_wall_seconds": ended - barrier_started[0],
    }, memory


def _concurrent_arm(
    model: Path,
    output_dir: Path,
    *,
    server: Path,
) -> tuple[list[dict[str, object]], list[str], dict[str, str]]:
    repetitions: list[dict[str, object]] = []
    with _server(model, output_dir, "concurrent-server", parallel=2, server=server) as (
        process,
        port,
        command,
    ):
        prompts = _prompts(port)
        prompt_hashes = {name: _token_sha256(prompt) for name, prompt in prompts.items()}
        for index in range(REPETITIONS):
            preparation = {
                "generator": _prepare_slot(port, "generator", prompts["generator"], 0),
                "verifier": _prepare_slot(port, "verifier", prompts["verifier"], 1),
            }
            streams, memory = _concurrent_pair(process, port, prompts)
            repetitions.append(
                {
                    "index": index,
                    "streams": streams,
                    "preparation": preparation,
                    "memory": memory,
                }
            )
    return repetitions, command, prompt_hashes


def run_w3(
    model: Path,
    output_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
) -> dict[str, object]:
    manifest = _read_manifest(model)
    output_dir.mkdir(parents=True, exist_ok=False)
    identity = {
        "model_sha256": manifest["sha256"],
        "engine_commit": subprocess.check_output(
            ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
            text=True,
        ).strip(),
        "server_sha256": _sha256(server),
    }
    report: dict[str, object] = {
        "schema": 1,
        **identity,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "runtime_profile": runtime_profile(),
        "contexts": {"generator": GENERATOR_CONTEXT, "verifier": VERIFIER_CONTEXT},
        "repetitions": [],
        "m0a_admitted_tokens": 0,
    }
    try:
        isolated, isolated_command, isolated_hashes = _isolated_arm(
            model, output_dir, server=server
        )
        concurrent, concurrent_command, concurrent_hashes = _concurrent_arm(
            model, output_dir, server=server
        )
        if isolated_hashes != concurrent_hashes:
            raise W3Error("prompt identities differ between W3 arms")
        report.update(
            {
                "prompt_sha256": isolated_hashes,
                "commands": {
                    "isolated": isolated_command,
                    "concurrent": concurrent_command,
                },
                "repetitions": [
                    {
                        "index": index,
                        "isolated": isolated[index]["streams"],
                        "concurrent": concurrent[index]["streams"],
                        "memory": concurrent[index]["memory"],
                        "preparation": {
                            "isolated": isolated[index]["preparation"],
                            "concurrent": concurrent[index]["preparation"],
                        },
                    }
                    for index in range(REPETITIONS)
                ],
            }
        )
        report["decision"] = evaluate_report(report)
    except Exception as error:
        report["run_error"] = f"{type(error).__name__}: {error}"
        report["decision"] = _invalid(report["run_error"])
    _write_json_atomic(output_dir / "decode-batching.json", report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    args = parser.parse_args()
    report = run_w3(args.model, args.output_dir, server=args.server)
    print(json.dumps(report["decision"], indent=2, sort_keys=True))
    return 0 if report["decision"]["decision"] != "W3_INVALID" else 2


if __name__ == "__main__":
    raise SystemExit(main())
