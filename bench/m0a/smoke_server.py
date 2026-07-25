"""Run deterministic logging-off/on llama-server smoke comparisons."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import socket
import subprocess
import threading
import time
from typing import Mapping
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
from uuid import UUID, uuid4

from bench.m0a.cache_sim import TraceValidationError, group_decode_events
from bench.m0a.routing_format import RoutingFormatError, iter_records, read_header


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_SERVER = REPO_ROOT / "vendor" / "llama.cpp" / "build" / "bin" / "llama-server"
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
PGR_ENV_NAMES = (
    "PGR_ROUTING_LOG",
    "PGR_SESSION_UUID",
    "PGR_MODEL_SHA256",
    "PGR_EXPECT_LAYERS",
    "PGR_EXPECT_EXPERTS",
    "PGR_EXPECT_TOP_K",
)
SMOKE_PROMPT = (
    "Complete the following Python code with a concise implementation and no markdown:\n\n"
    "def stable_unique(values):\n"
    "    # Return items in first-seen order with duplicates removed.\n"
)


class SmokeError(RuntimeError):
    """Raised when observability changes output or produces incomplete evidence."""


def server_command(
    model: Path,
    port: int,
    *,
    server: Path = DEFAULT_SERVER,
) -> list[str]:
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
        "4096",
        "--fit",
        "off",
        "--gpu-layers",
        "99",
        "--no-warmup",
        "--alias",
        "peregrine-m0",
    ]


def routing_environment(
    manifest: Mapping[str, object] | None,
    routing_path: Path | None,
    session_id: UUID | None,
    *,
    base: Mapping[str, str] | None = None,
) -> dict[str, str]:
    environment = dict(os.environ if base is None else base)
    for name in PGR_ENV_NAMES:
        environment.pop(name, None)
    if manifest is None:
        return environment
    if routing_path is None or session_id is None:
        raise ValueError("logging mode requires routing_path and session_id")
    geometry = manifest.get("geometry")
    if not isinstance(geometry, dict):
        raise SmokeError("model manifest has no geometry mapping")
    environment.update(
        {
            "PGR_ROUTING_LOG": str(routing_path),
            "PGR_SESSION_UUID": str(session_id),
            "PGR_MODEL_SHA256": str(manifest["sha256"]),
            "PGR_EXPECT_LAYERS": str(geometry["layers"]),
            "PGR_EXPECT_EXPERTS": str(geometry["experts"]),
            "PGR_EXPECT_TOP_K": str(geometry["top_k"]),
        }
    )
    return environment


def _unused_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def _json_request(url: str, body: dict[str, object] | None = None, timeout: float = 900) -> dict[str, object]:
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = Request(url, data=data, headers={"Content-Type": "application/json"})
    try:
        with urlopen(request, timeout=timeout) as response:
            parsed = json.loads(response.read().decode("utf-8"))
    except HTTPError as error:
        detail = error.read().decode("utf-8", errors="replace")
        raise SmokeError(f"HTTP {error.code} from {url}: {detail}") from error
    except (URLError, TimeoutError) as error:
        raise SmokeError(f"request failed for {url}: {error}") from error
    if not isinstance(parsed, dict):
        raise SmokeError(f"expected JSON object from {url}")
    return parsed


def _wait_for_health(process: subprocess.Popen[bytes], port: int) -> None:
    deadline = time.monotonic() + 900
    url = f"http://127.0.0.1:{port}/health"
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise SmokeError(f"llama-server exited during load with code {process.returncode}")
        try:
            health = _json_request(url, timeout=2)
            if health.get("status") in ("ok", "no slot available"):
                return
        except SmokeError:
            pass
        time.sleep(0.5)
    raise SmokeError("llama-server did not become healthy within 900 seconds")


def _monitor_rss(process: subprocess.Popen[bytes], stop: threading.Event, maximum: list[int]) -> None:
    while not stop.wait(0.1):
        result = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(process.pid)],
            capture_output=True,
            text=True,
            check=False,
        )
        try:
            maximum[0] = max(maximum[0], int(result.stdout.strip()))
        except ValueError:
            pass


def _run_mode(
    *,
    mode: str,
    model: Path,
    manifest: Mapping[str, object],
    output_dir: Path,
    logging: bool,
    routing_path: Path,
    server: Path,
) -> dict[str, object]:
    port = _unused_port()
    command = server_command(model, port, server=server)
    session_id = uuid4() if logging else None
    environment = routing_environment(
        manifest if logging else None,
        routing_path if logging else None,
        session_id,
    )
    stdout_path = output_dir / f"server-{mode}.stdout.log"
    stderr_path = output_dir / f"server-{mode}.stderr.log"
    peak_rss = [0]
    stop_monitor = threading.Event()

    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(command, env=environment, stdout=stdout, stderr=stderr)
        monitor = threading.Thread(
            target=_monitor_rss,
            args=(process, stop_monitor, peak_rss),
            daemon=True,
        )
        monitor.start()
        try:
            _wait_for_health(process, port)
            completion = _json_request(
                f"http://127.0.0.1:{port}/completion",
                {
                    "prompt": SMOKE_PROMPT,
                    "n_predict": 128,
                    "ignore_eos": True,
                    "temperature": 0,
                    "seed": 42,
                    "stream": False,
                },
            )
            content = completion.get("content")
            if not isinstance(content, str):
                raise SmokeError("completion response has no string content")
            tokenized = _json_request(
                f"http://127.0.0.1:{port}/tokenize",
                {"content": content, "add_special": False, "parse_special": True},
            )
            token_ids = tokenized.get("tokens")
            if not isinstance(token_ids, list) or not all(isinstance(token, int) for token in token_ids):
                raise SmokeError("tokenize response has invalid token ids")
            timings = completion.get("timings")
            if not isinstance(timings, dict):
                raise SmokeError("completion response has no timings")
            predicted_per_second = timings.get("predicted_per_second")
            if not isinstance(predicted_per_second, (int, float)):
                raise SmokeError("completion timings have no predicted_per_second")
            return {
                "mode": mode,
                "command": command,
                "token_ids": token_ids,
                "tokens_per_second": float(predicted_per_second),
                "predicted_tokens": timings.get("predicted_n"),
                "peak_rss_kb": peak_rss[0],
                "session_id": str(session_id) if session_id else None,
            }
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
            stop_monitor.set()
            monitor.join(timeout=2)


def compare_smoke_runs(
    logging_off: Mapping[str, object],
    logging_on: Mapping[str, object],
    routing_path: Path,
    *,
    minimum_decode_tokens: int = 127,
) -> dict[str, object]:
    if logging_off.get("token_ids") != logging_on.get("token_ids"):
        raise SmokeError("logging changed generated token ids")
    try:
        with routing_path.open("rb") as stream:
            header = read_header(stream)
        if (header.layer_count, header.expert_count, header.top_k) != (40, 256, 8):
            raise SmokeError("routing log geometry is not 40/256/top-8")
        events = group_decode_events(iter_records(routing_path))
    except (OSError, RoutingFormatError, TraceValidationError) as error:
        raise SmokeError(f"routing log validation failed: {error}") from error
    decode_tokens = len(events) // 40
    if decode_tokens < minimum_decode_tokens:
        raise SmokeError(
            f"routing log has {decode_tokens} complete decode tokens; need {minimum_decode_tokens}"
        )
    off_speed = float(logging_off["tokens_per_second"])
    on_speed = float(logging_on["tokens_per_second"])
    return {
        "token_ids_identical": True,
        "decode_tokens": decode_tokens,
        "logging_off_tokens_per_second": off_speed,
        "logging_on_tokens_per_second": on_speed,
        "slowdown_ratio": off_speed / on_speed if on_speed else float("inf"),
        "logging_off_peak_rss_kb": int(logging_off["peak_rss_kb"]),
        "logging_on_peak_rss_kb": int(logging_on["peak_rss_kb"]),
    }


def enforce_performance_gate(comparison: Mapping[str, object]) -> None:
    logging_on_speed = float(comparison["logging_on_tokens_per_second"])
    slowdown_ratio = float(comparison["slowdown_ratio"])
    if logging_on_speed < 5.0:
        raise SmokeError(f"logging throughput is {logging_on_speed:.6f} tok/s; need at least 5.0")
    if slowdown_ratio > 2.0:
        raise SmokeError(f"logging slowdown ratio is {slowdown_ratio:.6f}; maximum is 2.0")


def _write_json_atomic(path: Path, value: object) -> None:
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def _append_result(results: Path, comparison: Mapping[str, object]) -> None:
    timestamp = datetime.now(timezone.utc).isoformat()
    entry = (
        f"\n## M0a routing smoke — {timestamp}\n\n"
        f"- Token IDs identical: {comparison['token_ids_identical']}\n"
        f"- Complete logged decode tokens: {comparison['decode_tokens']}\n"
        f"- Logging off tok/s: {comparison['logging_off_tokens_per_second']:.6f}\n"
        f"- Logging on tok/s: {comparison['logging_on_tokens_per_second']:.6f}\n"
        f"- Slowdown ratio (off/on): {comparison['slowdown_ratio']:.6f}\n"
        f"- Logging off peak RSS KiB: {comparison['logging_off_peak_rss_kb']}\n"
        f"- Logging on peak RSS KiB: {comparison['logging_on_peak_rss_kb']}\n"
    )
    with results.open("a", encoding="utf-8") as stream:
        stream.write(entry)
        stream.flush()
        os.fsync(stream.fileno())


def run_smoke(
    model: Path,
    output_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
    results: Path = DEFAULT_RESULTS,
) -> dict[str, object]:
    manifest_path = model.parent / "manifest.json"
    if not manifest_path.is_file():
        raise SmokeError(f"verified model manifest not found: {manifest_path}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise SmokeError("model manifest is not a JSON object")
    output_dir.mkdir(parents=True, exist_ok=False)
    routing_path = output_dir / "routing-smoke.bin"
    logging_off = _run_mode(
        mode="off",
        model=model,
        manifest=manifest,
        output_dir=output_dir,
        logging=False,
        routing_path=routing_path,
        server=server,
    )
    logging_on = _run_mode(
        mode="on",
        model=model,
        manifest=manifest,
        output_dir=output_dir,
        logging=True,
        routing_path=routing_path,
        server=server,
    )
    comparison = compare_smoke_runs(logging_off, logging_on, routing_path)
    enforce_performance_gate(comparison)
    report = {"schema": 1, "logging_off": logging_off, "logging_on": logging_on, "comparison": comparison}
    _write_json_atomic(output_dir / "smoke.json", report)
    _append_result(results, comparison)
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = run_smoke(args.model, args.output_dir, server=args.server, results=args.results)
    except (SmokeError, OSError, ValueError, json.JSONDecodeError) as error:
        print(f"smoke failed: {error}")
        return 2
    print(json.dumps(report["comparison"], sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
