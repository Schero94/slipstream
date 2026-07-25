"""Measure fixed-MTP8 64K headroom under the recorded host state."""

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
from typing import Mapping

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


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
TARGET_WIRED_LIMIT_MB = 28_672
PROFILE = QUALIFICATION_PROFILES["baseline-f16-fa-mtp8"]


class HeadroomError(RuntimeError):
    pass


def parse_vm_stat(text: str) -> dict[str, int]:
    page_size = re.search(r"page size of (\d+) bytes", text)
    if page_size is None:
        raise HeadroomError("vm_stat page size is missing")
    values: dict[str, int] = {"page_size_bytes": int(page_size.group(1))}
    for key, label in (
        ("pages_free", "Pages free"),
        ("pageins", "Pageins"),
        ("pageouts", "Pageouts"),
        ("swapins", "Swapins"),
        ("swapouts", "Swapouts"),
    ):
        match = re.search(rf"^{label}:\s+(\d+)\.", text, re.MULTILINE)
        if match is None:
            raise HeadroomError(f"vm_stat {label} is missing")
        values[key] = int(match.group(1))
    return values


def parse_memory_pressure(text: str) -> int:
    match = re.search(r"System-wide memory free percentage:\s*(\d+)%", text)
    if match is None:
        raise HeadroomError("memory_pressure free percentage is missing")
    return int(match.group(1))


def evaluate_headroom(
    *,
    wired_limit_mb: int,
    point: Mapping[str, object],
    vm_before: Mapping[str, int],
    vm_after: Mapping[str, int],
    memory_pressure_before: int,
    memory_pressure_after: int,
) -> dict[str, object]:
    reasons: list[str] = []
    if point.get("context_tokens") != 64_000:
        reasons.append("context")
    if point.get("decoded_tokens") != 128 or point.get("stop_type") != "limit":
        reasons.append("decode-count")
    speed = float(point.get("decode_tokens_per_second", 0))
    rss = int(point.get("peak_rss_kb", 0))
    if speed <= 0:
        reasons.append("throughput")
    if rss <= 0:
        reasons.append("rss")
    if vm_before["page_size_bytes"] != vm_after["page_size_bytes"]:
        reasons.append("page-size")
    deltas = {
        f"{key}_delta": vm_after[key] - vm_before[key]
        for key in ("pageins", "pageouts", "swapins", "swapouts")
    }
    if any(delta < 0 for delta in deltas.values()):
        reasons.append("vm-counter")
    if deltas["pageouts_delta"] > 0:
        reasons.append("pageouts")
    if deltas["swapouts_delta"] > 0:
        reasons.append("swapouts")
    page_size_bytes = vm_before["page_size_bytes"]
    return {
        "schema": 1,
        "runtime_profile": asdict(PROFILE),
        "wired_limit_mb": wired_limit_mb,
        "requested_wired_limit_mb": TARGET_WIRED_LIMIT_MB,
        "requested_wired_limit_active": wired_limit_mb == TARGET_WIRED_LIMIT_MB,
        "point": dict(point),
        "page_size_bytes": page_size_bytes,
        **{f"{key}_before": vm_before[key] for key in ("pageins", "pageouts", "swapins", "swapouts")},
        **{f"{key}_after": vm_after[key] for key in ("pageins", "pageouts", "swapins", "swapouts")},
        **deltas,
        "pageout_bytes_delta": max(0, deltas["pageouts_delta"]) * page_size_bytes,
        "memory_free_percent_before": memory_pressure_before,
        "memory_free_percent_after": memory_pressure_after,
        "measurement_valid": not reasons,
        "reasons": reasons,
        "m0a_admitted_tokens": 0,
    }


def _command_output(command: list[str]) -> str:
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        raise HeadroomError(f"command failed: {' '.join(command)}")
    return completed.stdout


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run_headroom(model: Path, output_dir: Path, *, server: Path = DEFAULT_SERVER) -> dict[str, object]:
    manifest = _read_manifest(model)
    output_dir.mkdir(parents=True, exist_ok=False)
    wired_limit_mb = int(_command_output(["/usr/sbin/sysctl", "-n", "iogpu.wired_limit_mb"]).strip())
    vm_before = parse_vm_stat(_command_output(["vm_stat"]))
    pressure_before = parse_memory_pressure(_command_output(["memory_pressure"]))
    port = _unused_port()
    command = qualification_server_command(model, port, server=server, profile=PROFILE)
    stdout_path = output_dir / "server.stdout.log"
    stderr_path = output_dir / "server.stderr.log"
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
            tokenized = _json_request(
                f"http://127.0.0.1:{port}/tokenize",
                {"content": TOKEN_SEED_TEXT, "add_special": False, "parse_special": True},
            )
            seed = tokenized.get("tokens")
            if not isinstance(seed, list) or len(seed) < 64_000:
                raise HeadroomError("token seed is shorter than 64K")
            point = _run_point(process, port, seed[:64_000])
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
    vm_after = parse_vm_stat(_command_output(["vm_stat"]))
    pressure_after = parse_memory_pressure(_command_output(["memory_pressure"]))
    report = evaluate_headroom(
        wired_limit_mb=wired_limit_mb,
        point=point,
        vm_before=vm_before,
        vm_after=vm_after,
        memory_pressure_before=pressure_before,
        memory_pressure_after=pressure_after,
    )
    report.update(
        {
            "model_sha256": manifest["sha256"],
            "command": command,
            "llama_cpp_commit": subprocess.check_output(
                ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
                text=True,
            ).strip(),
        }
    )
    _write_json_atomic(output_dir / "headroom.json", report)
    return report


def append_results(path: Path, report: Mapping[str, object], evidence_hash: str) -> None:
    marker = f"S6 evidence SHA-256: `{evidence_hash}`"
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    if marker in existing:
        raise HeadroomError("S6 evidence is already present in RESULTS")
    point = report["point"]
    assert isinstance(point, Mapping)
    lines = [
        f"\n## Track S6 wired-limit and 64K headroom — {datetime.now(timezone.utc).isoformat()}\n",
        f"- {marker}",
        f"- Host wired limit observed: {report['wired_limit_mb']} MB; requested {TARGET_WIRED_LIMIT_MB} MB active: `{str(report['requested_wired_limit_active']).lower()}`; no system setting changed by the agent",
        f"- Fixed f16/FA/MTP8 at 64K: {point['decode_tokens_per_second']:.4f} tok/s, peak RSS {point['peak_rss_kb']:,} KiB",
        f"- Memory free: {report['memory_free_percent_before']}% -> {report['memory_free_percent_after']}%; pageouts delta: {report['pageouts_delta']} ({report['pageout_bytes_delta']} bytes); swapouts delta: {report['swapouts_delta']}",
        f"- Measurement valid: **{'YES' if report['measurement_valid'] else 'NO'}**; reasons: {', '.join(report['reasons']) or 'none'}; 0 M0a-admitted tokens",
        "",
    ]
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
        report = run_headroom(args.model, args.output_dir, server=args.server)
        evidence_path = args.output_dir / "headroom.json"
        append_results(args.results, report, _sha256(evidence_path))
    except (OSError, ValueError, subprocess.SubprocessError, HeadroomError) as error:
        print(f"headroom measurement failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["measurement_valid"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
