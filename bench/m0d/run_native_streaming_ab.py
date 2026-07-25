"""Qualify native bounded PGRN streaming with reproducible fail-closed evidence.

The default run deliberately exercises only the admitted SSD-streaming profile.
A resident comparison is never started implicitly because loading it can consume
the foreground/macOS reserve that this qualification is intended to protect.
"""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import re
import socket
import subprocess
import threading
import time
from typing import Mapping, Sequence
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_SERVER = REPO_ROOT / "vendor" / "llama.cpp" / "build" / "bin" / "llama-server"
DEFAULT_MODEL = Path(
    "/Users/schero/.cache/peregrine/models/qwen3.6-35b-a3b-q4/"
    "Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"
)
DEFAULT_PGRN = DEFAULT_MODEL.with_suffix(".pgrn")
CAFFEINATE = Path("/usr/bin/caffeinate")
GIB = 1024**3
TELEMETRY_ROUNDING_BYTES = math.ceil(0.005 * 1024 * 1024)
MAXIMUM_COLD_RECLAIM_BYTES = 4 * 1024 * 1024
# The memory-health gate is anchored on the hard signals (0 swapouts, reclaim within
# limit). The free-% check is derived from the configured reserve — the run must keep at
# least the reserve fraction free — with this absolute floor so it is never zero. A fixed
# 25% predated the validated 3 GiB reserve (which intentionally allows ~8% free).
MINIMUM_MEMORY_FREE_FLOOR_PERCENT = 5
MINIMUM_DECODE_TOKENS_PER_SECOND = 5.0

PROMPTS = (
    "Implement a concise Python function stable_unique(values) that preserves first-seen "
    "order. Return code only.",
    "Find and fix the off-by-one bug in this function, then include one assertion:\n"
    "def chunks(xs, n):\n    return [xs[i:i+n] for i in range(0, len(xs)-1, n)]",
)


class QualificationError(RuntimeError):
    """Raised for incomplete or unsafe qualification evidence."""


@dataclass(frozen=True)
class NativeRunConfig:
    server: Path
    model: Path
    pgrn: Path
    cache_gib: float
    headroom_gib: float
    context: int
    batch: int
    ubatch: int
    draft_max: int
    compact_slots: bool = False
    io_threads: int = 1
    coupling_path: Path | None = None
    page_cache: bool = False   # PGRN_PAGECACHE: trust the OS page cache instead of F_NOCACHE

    def __post_init__(self) -> None:
        for name in ("cache_gib", "headroom_gib"):
            value = getattr(self, name)
            if isinstance(value, bool) or not math.isfinite(value) or value <= 0:
                raise ValueError(f"{name} must be a positive finite number")
        for name in ("context", "batch", "ubatch", "io_threads"):
            value = getattr(self, name)
            if type(value) is not int or value <= 0:
                raise ValueError(f"{name} must be a positive integer")
        if type(self.draft_max) is not int or self.draft_max < 0:
            raise ValueError("draft_max must be a non-negative integer")
        if type(self.compact_slots) is not bool:
            raise ValueError("compact_slots must be a boolean")


def _format_number(value: float) -> str:
    return f"{value:g}"


def build_server_command(config: NativeRunConfig, port: int) -> list[str]:
    """Build the exact bounded native command; PGRN mode never requests mmap."""

    if type(port) is not int or not 0 < port < 65536:
        raise ValueError("port must be in 1..65535")
    command = [
        str(config.server),
        "--model", str(config.model),
        "--pgrn", str(config.pgrn),
        "--pgrn-cache-gb", _format_number(config.cache_gib),
        "--pgrn-headroom-gb", _format_number(config.headroom_gib),
        "--host", "127.0.0.1",
        "--port", str(port),
        "--parallel", "1",
        "--ctx-size", str(config.context),
        "--batch-size", str(config.batch),
        "--ubatch-size", str(config.ubatch),
        "--gpu-layers", "99",
        "--no-warmup",
        "--alias", "peregrine-pgrn",
        "--temp", "0",
        "--spec-type", "draft-mtp" if config.draft_max else "none",
    ]
    if config.draft_max:
        command.extend(["--spec-draft-n-max", str(config.draft_max)])
    if config.io_threads > 1:
        command.extend(["--pgrn-io-threads", str(config.io_threads)])
    if config.compact_slots:
        command.append("--pgrn-compact-slots")
    if config.coupling_path is not None:
        command.extend(["--pgrn-coupling", str(config.coupling_path)])
    return command


def parse_vm_stat(text: str) -> dict[str, int]:
    """Parse monotonic VM counters required by the safety decision."""

    page_size = re.search(r"page size of\s+(\d+)\s+bytes", text)
    if page_size is None:
        raise ValueError("vm_stat output missing page size")
    result = {"page_size_bytes": int(page_size.group(1))}
    for key, label in (
        ("pageins", "Pageins"),
        ("pageouts", "Pageouts"),
        ("swapins", "Swapins"),
        ("swapouts", "Swapouts"),
    ):
        match = re.search(rf"^{label}:\s+(\d+)\.\s*$", text, re.MULTILINE)
        if match is None:
            raise ValueError(f"vm_stat output missing {label}")
        result[key] = int(match.group(1))
    return result


_CACHE_LINE = re.compile(
    r"PGRN cache = ([0-9.]+) MiB, high-water = ([0-9.]+) MiB, "
    r"hits = (\d+), misses = (\d+) \(([0-9.]+)%\)"
)
_STAGE_LINE = re.compile(
    r"PGRN stage = ([0-9.]+) ms \(fetch ([0-9.]+) ms, upload ([0-9.]+) ms\), "
    r"experts = (\d+), bytes = ([0-9.]+) MiB"
)
_DRAFT_LINE = re.compile(
    r"draft acceptance = ([0-9.]+)\s+\(\s*(\d+) accepted /\s*(\d+) generated\), "
    r"mean len =\s*([0-9.]+)"
)
_COMPACT_LINE = re.compile(r"PGRN compact slot compute = (enabled|disabled)")


def parse_native_telemetry(text: str) -> list[dict[str, object]]:
    """Join each server request's cache, staging, and MTP timing block."""

    cache = list(_CACHE_LINE.finditer(text))
    stage = list(_STAGE_LINE.finditer(text))
    draft = list(_DRAFT_LINE.finditer(text))
    compact = list(_COMPACT_LINE.finditer(text))
    if len(cache) != len(stage):
        return []
    if draft and len(draft) != len(cache):
        return []
    if compact and len(compact) != len(cache):
        return []
    samples: list[dict[str, object]] = []
    for index, (cache_match, stage_match) in enumerate(zip(cache, stage, strict=True)):
        sample: dict[str, object] = {
            "cache_bytes": round(float(cache_match.group(1)) * 1024 * 1024),
            "high_water_bytes": round(float(cache_match.group(2)) * 1024 * 1024),
            "hits": int(cache_match.group(3)),
            "misses": int(cache_match.group(4)),
            "hit_percent": float(cache_match.group(5)),
            "stage_ms": float(stage_match.group(1)),
            "fetch_ms": float(stage_match.group(2)),
            "upload_ms": float(stage_match.group(3)),
            "experts": int(stage_match.group(4)),
            "streamed_bytes": round(float(stage_match.group(5)) * 1024 * 1024),
        }
        if draft:
            draft_match = draft[index]
            sample.update(
                {
                    "draft_acceptance": float(draft_match.group(1)),
                    "draft_accepted": int(draft_match.group(2)),
                    "draft_generated": int(draft_match.group(3)),
                    "draft_mean_length": float(draft_match.group(4)),
                }
            )
        if compact:
            sample["compact_slots"] = compact[index].group(1) == "enabled"
        samples.append(sample)
    return samples


def resident_run_allowed(available_bytes: int, resident_bytes: int, headroom_bytes: int) -> bool:
    """Return true only when a full resident load preserves the complete reserve."""

    if any(type(value) is not int or value <= 0 for value in (
        available_bytes, resident_bytes, headroom_bytes
    )):
        return False
    return resident_bytes <= available_bytes and headroom_bytes <= available_bytes - resident_bytes


def _pgrn_memory_bound(path: Path, requested_cache_bytes: int, io_threads: int = 1) -> dict[str, int]:
    """Derive the runtime's exact fixed cache+per-layer staging bound from PGRN."""

    if type(io_threads) is not int or io_threads < 1:
        raise QualificationError("io_threads must be a positive integer")
    if type(requested_cache_bytes) is not int or requested_cache_bytes <= 0:
        raise QualificationError("requested PGRN cache bytes must be positive")
    with path.open("rb") as stream:
        fixed = stream.read(16)
        if len(fixed) != 16 or fixed[:8] != b"PGRN1\x00\x00\x00":
            raise QualificationError("PGRN artifact has invalid fixed header")
        version = int.from_bytes(fixed[8:12], "little")
        json_length = int.from_bytes(fixed[12:16], "little")
        if version != 1 or not 0 < json_length <= 16384 - 16:
            raise QualificationError("PGRN artifact has invalid version or JSON length")
        try:
            header = json.loads(stream.read(json_length).decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise QualificationError(f"PGRN artifact has malformed JSON: {error}") from error
    if not isinstance(header, Mapping):
        raise QualificationError("PGRN header is not an object")
    metadata = header.get("metadata")
    layouts = metadata.get("tensor_directory") if isinstance(metadata, Mapping) else None
    if not isinstance(layouts, list) or not layouts:
        raise QualificationError("PGRN tensor directory is absent")
    layers: set[int] = set()
    maximum_slot = 0
    for layout in layouts:
        if (
            not isinstance(layout, list)
            or len(layout) != 7
            or any(type(value) is not int or value < 0 for value in layout)
            or any(layout[index] <= 0 for index in (2, 4, 6))
            or layout[0] in layers
        ):
            raise QualificationError("PGRN tensor directory geometry is invalid")
        layers.add(layout[0])
        slot = layout[2] + layout[4] + layout[6]
        if slot > 0xFFFFFFFF:
            raise QualificationError("PGRN expert slot exceeds native format")
        maximum_slot = max(maximum_slot, slot)
    capacity = requested_cache_bytes // maximum_slot
    if capacity < len(layers):
        raise QualificationError("PGRN cache cannot provide one slot per routed layer")
    cache_bytes = capacity * maximum_slot
    # Each layer stream keeps io_threads cold-read staging records (io_threads=1 serial).
    staging_bytes = len(layers) * maximum_slot * io_threads
    return {
        "layer_count": len(layers),
        "slot_bytes": maximum_slot,
        "cache_capacity": capacity,
        "cache_bytes": cache_bytes,
        "staging_bytes": staging_bytes,
        "maximum_high_water_bytes": cache_bytes + staging_bytes,
    }


def _positive_finite(value: object) -> bool:
    return (
        not isinstance(value, bool)
        and isinstance(value, (int, float))
        and math.isfinite(float(value))
        and float(value) > 0
    )


def _non_negative_int(value: object) -> bool:
    return type(value) is int and value >= 0


def evaluate_report(
    report: Mapping[str, object], *, require_mtp: bool, require_thinking: bool = False
) -> dict[str, object]:
    """Make a fail-closed decision from the complete collected report."""

    reasons: list[str] = []
    if report.get("sleep_prevention_active") is not True:
        reasons.append("sleep-prevention")
    if report.get("server_exit_code") != 0 or report.get("clean_shutdown") is not True:
        reasons.append("shutdown")
    if not _positive_finite(report.get("peak_rss_kb")):
        reasons.append("rss")

    vm = report.get("vm")
    if not isinstance(vm, Mapping):
        reasons.extend(("pageouts", "swapouts"))
    else:
        page_size = vm.get("page_size_bytes")
        pageouts = vm.get("pageouts_delta")
        swapins = vm.get("swapins_delta")
        swapouts = vm.get("swapouts_delta")
        maximum_reclaim = report.get("maximum_reclaim_bytes")
        if (
            type(page_size) is not int
            or page_size <= 0
            or not _non_negative_int(pageouts)
            or not _non_negative_int(swapins)
            or type(maximum_reclaim) is not int
            or maximum_reclaim < 0
            or (pageouts + swapins) * page_size > maximum_reclaim
        ):
            reasons.append("pageouts")
        if not _non_negative_int(swapouts) or swapouts != 0:
            reasons.append("swapouts")

    pressure_before = report.get("memory_free_percent_before")
    pressure_after = report.get("memory_free_percent_after")
    minimum_pressure = report.get("minimum_memory_free_percent")
    if (
        type(pressure_before) is not int
        or type(pressure_after) is not int
        or not 0 <= pressure_before <= 100
        or not 0 <= pressure_after <= 100
        or type(minimum_pressure) is not int
        or not 0 <= minimum_pressure <= 100
        or pressure_after < minimum_pressure
    ):
        reasons.append("memory-pressure")

    requests = report.get("requests")
    if not isinstance(requests, Sequence) or isinstance(requests, (str, bytes)) or len(requests) < 2:
        reasons.append("turn-count")
        requests = []
    minimum_speed = report.get("minimum_decode_tokens_per_second")
    if not _positive_finite(minimum_speed):
        reasons.append("throughput")
        minimum_speed = float("inf")
    for request in requests:
        if not isinstance(request, Mapping) or not all(
            _positive_finite(request.get(key))
            for key in ("prompt_tokens_per_second", "decode_tokens_per_second")
        ):
            reasons.append("request-metrics")
            break
        if type(request.get("completion_tokens")) is not int or request["completion_tokens"] <= 0:
            reasons.append("request-metrics")
            break
        if float(request["decode_tokens_per_second"]) < float(minimum_speed):
            reasons.append("throughput")
    if require_thinking and (
        not requests
        or not all(
            isinstance(request, Mapping) and request.get("thinking_observed") is True
            for request in requests
        )
    ):
        reasons.append("thinking")

    telemetry = report.get("telemetry")
    if (
        not isinstance(telemetry, Sequence)
        or isinstance(telemetry, (str, bytes))
        or len(telemetry) == 0
        or len(telemetry) != len(requests)
    ):
        reasons.append("telemetry-count")
        telemetry = []
    maximum = report.get("maximum_high_water_bytes")
    if not _positive_finite(maximum):
        reasons.append("high-water")
        maximum = 0
    mtp_seen = False
    configuration = report.get("configuration")
    require_compact = isinstance(configuration, Mapping) and configuration.get("compact_slots") is True
    for sample in telemetry:
        if not isinstance(sample, Mapping):
            reasons.append("telemetry-count")
            continue
        if require_compact and sample.get("compact_slots") is not True:
            reasons.append("compact-slots")
        required = ("cache_bytes", "high_water_bytes", "hits", "misses", "experts")
        if not all(_non_negative_int(sample.get(key)) for key in required) or not all(
            _positive_finite(sample.get(key)) for key in ("fetch_ms", "upload_ms")
        ):
            reasons.append("telemetry-count")
            continue
        if int(sample["high_water_bytes"]) > float(maximum):
            reasons.append("high-water")
        accepted = sample.get("draft_accepted")
        generated = sample.get("draft_generated")
        if _non_negative_int(accepted) and _non_negative_int(generated):
            if accepted <= generated and generated > 0:
                mtp_seen = True
            else:
                reasons.append("mtp")
    if require_mtp and not mtp_seen:
        reasons.append("mtp")
    return {"status": "PASS" if not reasons else "FAIL", "reasons": list(dict.fromkeys(reasons))}


def _unused_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def _json_request(url: str, body: Mapping[str, object] | None = None, timeout: float = 900) -> dict[str, object]:
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = Request(url, data=data, headers={"Content-Type": "application/json"})
    try:
        with urlopen(request, timeout=timeout) as response:
            value = json.loads(response.read().decode("utf-8"))
    except HTTPError as error:
        detail = error.read().decode("utf-8", errors="replace")
        raise QualificationError(f"HTTP {error.code} from {url}: {detail}") from error
    except (URLError, TimeoutError, json.JSONDecodeError) as error:
        raise QualificationError(f"request failed for {url}: {error}") from error
    if not isinstance(value, dict):
        raise QualificationError(f"expected JSON object from {url}")
    return value


def _wait_for_health(process: subprocess.Popen[bytes], port: int, timeout: float = 900) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise QualificationError(f"server exited during load with code {process.returncode}")
        try:
            health = _json_request(f"http://127.0.0.1:{port}/health", timeout=2)
            if health.get("status") in ("ok", "no slot available"):
                return
        except QualificationError:
            pass
        time.sleep(0.5)
    raise QualificationError(f"server did not become healthy within {timeout:g} seconds")


def snapshot_vm_stat() -> dict[str, int]:
    result = subprocess.run(
        ["vm_stat"], capture_output=True, text=True, check=False, timeout=10
    )
    if result.returncode != 0:
        raise QualificationError(f"vm_stat failed with code {result.returncode}")
    return parse_vm_stat(result.stdout)


def reserve_free_floor_percent(headroom_gib: float) -> int:
    """Minimum system-free % the run must hold: the configured reserve as a fraction of
    physical RAM, never below the absolute floor. Ties the soft gate to the reserve
    contract instead of a fixed threshold (the hard gates remain swapouts + reclaim)."""
    try:
        out = subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True,
                             text=True, check=False, timeout=5).stdout.strip()
        total_gib = int(out) / (1024 ** 3)
    except Exception:
        total_gib = 0.0
    reserve_pct = round(headroom_gib / total_gib * 100) if total_gib > 0 else MINIMUM_MEMORY_FREE_FLOOR_PERCENT
    return max(MINIMUM_MEMORY_FREE_FLOOR_PERCENT, reserve_pct)


def snapshot_memory_pressure() -> int:
    result = subprocess.run(
        ["memory_pressure", "-Q"], capture_output=True, text=True,
        check=False, timeout=10,
    )
    if result.returncode != 0:
        raise QualificationError(f"memory_pressure failed with code {result.returncode}")
    match = re.search(r"System-wide memory free percentage:\s*(\d+)%", result.stdout)
    if match is None:
        raise QualificationError("memory_pressure output missing free percentage")
    value = int(match.group(1))
    if not 0 <= value <= 100:
        raise QualificationError("memory_pressure free percentage is invalid")
    return value


def _vm_report(before: Mapping[str, int], after: Mapping[str, int]) -> dict[str, int]:
    if before.get("page_size_bytes") != after.get("page_size_bytes"):
        raise QualificationError("vm_stat page size changed during request window")
    result = {"page_size_bytes": int(before["page_size_bytes"])}
    for key in ("pageins", "pageouts", "swapins", "swapouts"):
        delta = int(after[key]) - int(before[key])
        if delta < 0:
            raise QualificationError(f"vm_stat {key} counter moved backwards")
        result[f"{key}_before"] = int(before[key])
        result[f"{key}_after"] = int(after[key])
        result[f"{key}_delta"] = delta
    return result


def _monitor_rss(process: subprocess.Popen[bytes], stop: threading.Event, maximum: list[int]) -> None:
    while not stop.wait(0.1):
        result = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(process.pid)],
            capture_output=True, text=True, check=False, timeout=5,
        )
        try:
            maximum[0] = max(maximum[0], int(result.stdout.strip()))
        except ValueError:
            pass


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _build_fingerprint(server: Path) -> dict[str, object]:
    submodule = subprocess.run(
        ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
        capture_output=True, text=True, check=False, timeout=10,
    )
    if submodule.returncode != 0 or not submodule.stdout.strip():
        raise QualificationError("cannot identify llama.cpp submodule HEAD")
    version = subprocess.run(
        [str(server), "--version"], capture_output=True, text=True, check=False, timeout=30
    )
    version_text = (version.stdout + version.stderr).strip()
    if version.returncode != 0 or not version_text:
        raise QualificationError("cannot read llama-server build version")
    return {
        "server_sha256": _sha256(server),
        "server_size_bytes": server.stat().st_size,
        "llama_submodule_head": submodule.stdout.strip(),
        "version": version_text,
    }


def _atomic_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    try:
        with temporary.open("w", encoding="utf-8") as stream:
            json.dump(value, stream, indent=2, sort_keys=True, allow_nan=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _preflight(config: NativeRunConfig) -> None:
    if not config.server.is_file() or not os.access(config.server, os.X_OK):
        raise QualificationError(f"server is not executable: {config.server}")
    for label, path in (("model", config.model), ("PGRN", config.pgrn)):
        if not path.is_file() or not os.access(path, os.R_OK):
            raise QualificationError(f"{label} is not readable: {path}")
    if not CAFFEINATE.is_file() or not os.access(CAFFEINATE, os.X_OK):
        raise QualificationError(f"sleep prevention is unavailable: {CAFFEINATE}")


def _parse_request(
    response: Mapping[str, object], expected_tokens: int, wall_seconds: float
) -> dict[str, object]:
    choices = response.get("choices")
    if not isinstance(choices, list) or not choices or not isinstance(choices[0], Mapping):
        raise QualificationError("chat response has no choices")
    message = choices[0].get("message")
    if not isinstance(message, Mapping) or not isinstance(message.get("content"), str):
        raise QualificationError("chat response has no assistant content")
    content = message["content"]
    reasoning = message.get("reasoning_content")
    if not isinstance(reasoning, str):
        match = re.search(r"<think>(.*?)</think>", content, re.DOTALL)
        reasoning = match.group(1) if match else ""
    usage = response.get("usage")
    timings = response.get("timings")
    if not isinstance(usage, Mapping) or not isinstance(timings, Mapping):
        raise QualificationError("chat response has no usage/timings mapping")
    completion_tokens = usage.get("completion_tokens")
    prompt_speed = timings.get("prompt_per_second")
    decode_speed = timings.get("predicted_per_second")
    prompt_ms = timings.get("prompt_ms")
    decode_ms = timings.get("predicted_ms")
    if type(completion_tokens) is not int or completion_tokens <= 0 or completion_tokens > expected_tokens:
        raise QualificationError("chat response has invalid completion token count")
    if not _positive_finite(prompt_speed) or not _positive_finite(decode_speed):
        raise QualificationError("chat response has invalid prompt/decode rate")
    if not _positive_finite(wall_seconds):
        raise QualificationError("chat request has invalid wall time")
    return {
        "completion_tokens": completion_tokens,
        "prompt_tokens": timings.get("prompt_n"),
        "prompt_tokens_per_second": float(prompt_speed),
        "decode_tokens_per_second": float(decode_speed),
        "prompt_ms": float(prompt_ms) if _positive_finite(prompt_ms) else None,
        "decode_ms": float(decode_ms) if _positive_finite(decode_ms) else None,
        "wall_seconds": float(wall_seconds),
        "finish_reason": choices[0].get("finish_reason"),
        "content_sha256": hashlib.sha256(content.encode("utf-8")).hexdigest(),
        "thinking_observed": bool(reasoning.strip()),
        "thinking_characters": len(reasoning),
        "thinking_sha256": hashlib.sha256(reasoning.encode("utf-8")).hexdigest(),
    }


def run_qualification(
    config: NativeRunConfig,
    output_path: Path,
    *,
    completion_limits: Sequence[int] = (64, 128),
    startup_timeout: float = 900,
    maximum_reclaim_bytes: int = MAXIMUM_COLD_RECLAIM_BYTES,
    minimum_decode_tokens_per_second: float = MINIMUM_DECODE_TOKENS_PER_SECOND,
) -> dict[str, object]:
    """Run the native workload and always leave one atomic decision artifact."""

    started = datetime.now(timezone.utc)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    stem = output_path.with_suffix("")
    stdout_path = stem.with_name(stem.name + "-server.stdout.log")
    stderr_path = stem.with_name(stem.name + "-server.stderr.log")
    report: dict[str, object] = {
        "schema": 1,
        "started_at": started.isoformat(),
        "configuration": {key: str(value) if isinstance(value, Path) else value for key, value in asdict(config).items()},
        "requests": [],
        "telemetry": [],
        "peak_rss_kb": 0,
        "server_exit_code": None,
        "clean_shutdown": False,
        "maximum_high_water_bytes": 0,
        "maximum_reclaim_bytes": maximum_reclaim_bytes,
        "minimum_memory_free_percent": reserve_free_floor_percent(config.headroom_gib),
        "minimum_decode_tokens_per_second": minimum_decode_tokens_per_second,
        "stdout_log": str(stdout_path),
        "stderr_log": str(stderr_path),
        "sleep_prevention_command": [str(CAFFEINATE), "-im"],
        "sleep_prevention_active": False,
    }
    process: subprocess.Popen[bytes] | None = None
    caffeinator: subprocess.Popen[bytes] | None = None
    monitor: threading.Thread | None = None
    stop_monitor = threading.Event()
    peak_rss = [0]
    try:
        _preflight(config)
        if len(completion_limits) != len(PROMPTS) or any(
            type(value) is not int or value <= 0 for value in completion_limits
        ):
            raise QualificationError("exactly two positive completion limits are required")
        port = _unused_port()
        command = build_server_command(config, port)
        report["command"] = command
        memory_bound = _pgrn_memory_bound(config.pgrn, round(config.cache_gib * GIB), config.io_threads)
        report["pgrn_memory_bound"] = memory_bound
        # Native telemetry is printed to 0.01 MiB. Permit exactly half a display
        # unit while retaining the byte-exact derived bound next to it.
        report["maximum_high_water_bytes"] = (
            memory_bound["maximum_high_water_bytes"] + TELEMETRY_ROUNDING_BYTES
        )
        report["telemetry_rounding_bytes"] = TELEMETRY_ROUNDING_BYTES
        report["build"] = _build_fingerprint(config.server)
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            caffeinator = subprocess.Popen(
                report["sleep_prevention_command"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            if caffeinator.poll() is not None:
                raise QualificationError("caffeinate exited before server launch")
            server_env = {**os.environ, "PGRN_PAGECACHE": "1"} if config.page_cache else None
            process = subprocess.Popen(command, stdout=stdout, stderr=stderr, env=server_env)
            monitor = threading.Thread(
                target=_monitor_rss, args=(process, stop_monitor, peak_rss), daemon=True
            )
            monitor.start()
            _wait_for_health(process, port, timeout=startup_timeout)
            before = snapshot_vm_stat()
            pressure_before = snapshot_memory_pressure()
            requests: list[dict[str, object]] = []
            endpoint = f"http://127.0.0.1:{port}/v1/chat/completions"
            for prompt, limit in zip(PROMPTS, completion_limits, strict=True):
                request_started = time.monotonic()
                response = _json_request(
                    endpoint,
                    {
                        "model": "peregrine-pgrn",
                        "messages": [{"role": "user", "content": prompt}],
                        "temperature": 0,
                        "seed": 42,
                        "max_tokens": limit,
                        "stream": False,
                        "chat_template_kwargs": {"enable_thinking": True},
                    },
                    timeout=1800,
                )
                requests.append(
                    _parse_request(response, limit, time.monotonic() - request_started)
                )
            after = snapshot_vm_stat()
            pressure_after = snapshot_memory_pressure()
            if caffeinator.poll() is not None:
                raise QualificationError("sleep-prevention assertion ended during workload")
            report["sleep_prevention_active"] = True
            report["requests"] = requests
            report["vm"] = _vm_report(before, after)
            report["memory_free_percent_before"] = pressure_before
            report["memory_free_percent_after"] = pressure_after
            process.terminate()
            try:
                report["server_exit_code"] = process.wait(timeout=60)
                report["clean_shutdown"] = report["server_exit_code"] == 0
            except subprocess.TimeoutExpired:
                process.kill()
                report["server_exit_code"] = process.wait(timeout=10)
                report["clean_shutdown"] = False
        report["peak_rss_kb"] = peak_rss[0]
        report["telemetry"] = parse_native_telemetry(
            stderr_path.read_text(encoding="utf-8", errors="replace")
        )
    except Exception as error:  # the artifact must preserve every fail-closed path
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)
            report["server_exit_code"] = process.returncode
            report["clean_shutdown"] = False
        if caffeinator is not None and caffeinator.poll() is None:
            caffeinator.terminate()
            try:
                caffeinator.wait(timeout=5)
            except subprocess.TimeoutExpired:
                caffeinator.kill()
                caffeinator.wait(timeout=5)
        report["sleep_prevention_exit_code"] = (
            caffeinator.returncode if caffeinator is not None else None
        )
        stop_monitor.set()
        if monitor is not None:
            monitor.join(timeout=5)
        report["peak_rss_kb"] = max(int(report.get("peak_rss_kb", 0)), peak_rss[0])
        if stderr_path.is_file() and not report.get("telemetry"):
            report["telemetry"] = parse_native_telemetry(
                stderr_path.read_text(encoding="utf-8", errors="replace")
            )
        report["finished_at"] = datetime.now(timezone.utc).isoformat()
        report["decision"] = evaluate_report(
            report, require_mtp=config.draft_max > 0, require_thinking=True
        )
        _atomic_json(output_path, report)
    return report


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument("--model", type=Path, default=DEFAULT_MODEL)
    parser.add_argument("--pgrn", type=Path, default=DEFAULT_PGRN)
    parser.add_argument("--cache-gib", type=float, default=2.0)
    parser.add_argument("--headroom-gib", type=float, default=3.0)
    parser.add_argument("--context", type=int, default=512)
    parser.add_argument("--batch", type=int, default=32)
    parser.add_argument("--ubatch", type=int, default=32)
    parser.add_argument("--draft-max", type=int, default=4)
    parser.add_argument("--io-threads", type=int, default=1,
                        help="parallel PGRN cold-read threads per layer (--pgrn-io-threads); 1 = serial baseline")
    parser.add_argument("--compact-slots", action="store_true")
    parser.add_argument("--pgrn-coupling", type=Path, default=None,
                        help="PGCC1 coupled prefetch table (--pgrn-coupling); omit for the no-prefetch baseline")
    parser.add_argument("--page-cache", action="store_true",
                        help="Flash-MoE experiment: set PGRN_PAGECACHE=1 so expert reads use the OS page cache (no F_NOCACHE)")
    parser.add_argument("--first-tokens", type=int, default=64)
    parser.add_argument("--second-tokens", type=int, default=128)
    parser.add_argument("--maximum-reclaim-mib", type=float, default=4.0)
    parser.add_argument("--minimum-decode-tps", type=float, default=5.0)
    parser.add_argument("--output", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = args.output or REPO_ROOT / "bench" / "artifacts" / "m0d" / f"native-pgrn-35b-{timestamp}.json"
    config = NativeRunConfig(
        server=args.server,
        model=args.model,
        pgrn=args.pgrn,
        cache_gib=args.cache_gib,
        headroom_gib=args.headroom_gib,
        context=args.context,
        batch=args.batch,
        ubatch=args.ubatch,
        draft_max=args.draft_max,
        compact_slots=args.compact_slots,
        io_threads=args.io_threads,
        coupling_path=args.pgrn_coupling,
        page_cache=args.page_cache,
    )
    report = run_qualification(
        config,
        output,
        completion_limits=(args.first_tokens, args.second_tokens),
        maximum_reclaim_bytes=round(args.maximum_reclaim_mib * 1024 * 1024),
        minimum_decode_tokens_per_second=args.minimum_decode_tps,
    )
    print(json.dumps({"artifact": str(output), **report["decision"]}, sort_keys=True))
    return 0 if report["decision"]["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
