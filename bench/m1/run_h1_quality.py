"""Run the two-stage local H1 quality qualification on one resident server."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import time
from uuid import uuid4

from bench.m0a.qualify_model import production_environment
from bench.m0a.run_agentic_episodes import AgenticRunError, run_episodes
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    SmokeError,
    _json_request,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)
from bench.m1.h1_quality import (
    FULL_MANIFEST,
    H1QualityError,
    OPEN_MANIFEST,
    evaluate_h1_quality,
)
from bench.m1.memory_window import build_window, capture_snapshot
from bench.m1.model_bakeoff import track_m_server_command, verify_candidate


class H1QualityRunError(RuntimeError):
    pass


def h1_quality_server_command(
    model: Path,
    port: int,
    *,
    server: Path = DEFAULT_SERVER,
) -> list[str]:
    command = track_m_server_command(
        model,
        port,
        server=server,
        context_tokens=32_000,
    )
    command[command.index("--alias") + 1] = "peregrine-h1"
    return command


def _smoke_passes(path: Path, expected_sha256: str) -> None:
    if path.is_symlink() or not path.is_file():
        raise H1QualityRunError("H1 smoke report must be a regular file")
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise H1QualityRunError(f"cannot read H1 smoke report: {error}") from error
    if (
        not isinstance(report, dict)
        or report.get("decision") != "PASS"
        or report.get("gate") != "h1-hard-case-v1"
        or not isinstance(report.get("candidate"), dict)
        or report["candidate"].get("sha256") != expected_sha256
    ):
        raise H1QualityRunError("H1 resource smoke is not an identity-bound PASS")


def run_h1_quality(
    model: Path,
    smoke_path: Path,
    output_dir: Path,
    *,
    expected_size: int,
    expected_sha256: str,
    server: Path = DEFAULT_SERVER,
) -> dict[str, object]:
    verify_candidate(model, expected_size=expected_size, expected_sha256=expected_sha256)
    _smoke_passes(smoke_path, expected_sha256)
    output_dir.mkdir(parents=True, exist_ok=False, mode=0o700)
    port = _unused_port()
    base_url = f"http://127.0.0.1:{port}/v1"
    command = h1_quality_server_command(model, port, server=server)
    stdout_path = output_dir / "server.stdout.log"
    stderr_path = output_dir / "server.stderr.log"
    process: subprocess.Popen[bytes] | None = None
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
            _json_request(
                f"http://127.0.0.1:{port}/completion",
                {
                    "prompt": "Peregrine H1 resident warmup.",
                    "n_predict": 1,
                    "ignore_eos": True,
                    "temperature": 0,
                    "seed": 42,
                    "stream": False,
                    "cache_prompt": False,
                },
                timeout=3600,
            )
            time.sleep(2.0)
            memory_before = capture_snapshot()
            open_report = run_episodes(
                OPEN_MANIFEST,
                output_dir / "open",
                base_url=base_url,
                model="peregrine-h1",
                session_id=uuid4(),
                server_pid=process.pid,
                verifier_retry=True,
            )
            open_passes = int(open_report["decision"]["episode_passes"])
            if open_passes == 0:
                memory_after = capture_snapshot()
                memory = build_window(memory_before, memory_after)
                _write_json_atomic(output_dir / "quality-memory.json", memory)
                decision = {
                    "schema": 1,
                    "decision": "H1_REJECTED",
                    "reasons": ["open-quality"],
                    "model_sha256": expected_sha256,
                    "server_pid": process.pid,
                    "open_passes": 0,
                    "open_count": 2,
                    "full_passes": None,
                    "full_count": 6,
                    "quality_memory": memory,
                    "m0a_admitted_tokens": 0,
                }
                _write_json_atomic(output_dir / "h1-decision.json", decision)
                return decision

            run_episodes(
                FULL_MANIFEST,
                output_dir / "full",
                base_url=base_url,
                model="peregrine-h1",
                session_id=uuid4(),
                server_pid=process.pid,
                verifier_retry=True,
            )
            memory_after = capture_snapshot()
            memory = build_window(memory_before, memory_after)
            memory_path = output_dir / "quality-memory.json"
            _write_json_atomic(memory_path, memory)
            decision = evaluate_h1_quality(
                smoke_path,
                output_dir / "open" / "report.json",
                output_dir / "full" / "report.json",
                memory_path,
                expected_model_sha256=expected_sha256,
            )
            _write_json_atomic(output_dir / "h1-decision.json", decision)
            return decision
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--smoke", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--expected-size", required=True, type=int)
    parser.add_argument("--expected-sha256", required=True)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    args = parser.parse_args()
    try:
        decision = run_h1_quality(
            args.model,
            args.smoke,
            args.output_dir,
            expected_size=args.expected_size,
            expected_sha256=args.expected_sha256,
            server=args.server,
        )
    except (
        AgenticRunError,
        H1QualityError,
        H1QualityRunError,
        OSError,
        SmokeError,
        ValueError,
        subprocess.SubprocessError,
    ) as error:
        print(f"H1 quality run failed: {error}")
        return 2
    print(json.dumps(decision, indent=2, sort_keys=True))
    return 0 if decision["decision"] == "H1_ADMITTED" else 2


if __name__ == "__main__":
    raise SystemExit(main())
