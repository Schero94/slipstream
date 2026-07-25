"""Start one private, explicitly bounded M0a interactive collection session."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import shlex
import signal
import subprocess
import sys
import threading
from typing import Callable, Mapping
from uuid import UUID, uuid4

from bench.m0a.smoke_server import DEFAULT_SERVER, _monitor_rss, routing_environment
from scripts.verify_model import sha256_file


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_ARTIFACTS = REPO_ROOT / "bench" / "artifacts" / "m0a"
DEFAULT_PATCH = REPO_ROOT / "patches" / "llama.cpp" / "0001-peregrine-routing.patch"


class SessionError(RuntimeError):
    """Raised when an interactive session cannot be started safely."""


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _write_json_atomic(path: Path, value: Mapping[str, object]) -> None:
    temporary = path.with_name(path.name + ".tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        os.chmod(path, 0o600)
    except Exception:
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass
        raise


def session_command(
    model: Path,
    session_prompt_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
    speculation: str = "none",
    draft_tokens: int | None = None,
    server_log_path: Path | None = None,
) -> list[str]:
    if speculation not in {"none", "draft-mtp"}:
        raise ValueError(f"unsupported speculation mode: {speculation}")
    if speculation == "none" and draft_tokens is not None:
        raise ValueError("draft tokens require draft-mtp speculation")
    if speculation == "draft-mtp" and (draft_tokens is None or draft_tokens <= 0):
        raise ValueError("draft-mtp requires a positive draft token count")
    command = [
        str(server),
        "--model",
        str(model),
        "--host",
        "127.0.0.1",
        "--port",
        "8080",
        "--alias",
        "peregrine-m0",
        "--parallel",
        "1",
        "--ctx-size",
        "65536",
        "--fit",
        "off",
        "--gpu-layers",
        "99",
        "--no-warmup",
        "--spec-type",
        speculation,
        "--temp",
        "0.6",
        "--top-p",
        "0.95",
        "--top-k",
        "20",
        "--min-p",
        "0.0",
        "--presence-penalty",
        "0.0",
        "--repeat-penalty",
        "1.0",
        "--log-prompts-dir",
        str(session_prompt_dir),
    ]
    if draft_tokens is not None:
        command.extend(["--spec-draft-n-max", str(draft_tokens)])
    if server_log_path is not None:
        command.extend(["--log-file", str(server_log_path)])
    return command


def _read_manifest(model: Path) -> dict[str, object]:
    if model.is_symlink() or not model.is_file():
        raise SessionError("model must be a regular non-symlink file")
    manifest_path = model.parent / "manifest.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SessionError(f"cannot read verified model manifest: {error}") from error
    if not isinstance(manifest, dict) or not isinstance(manifest.get("sha256"), str):
        raise SessionError("verified model manifest is invalid")
    geometry = manifest.get("geometry")
    if not isinstance(geometry, dict):
        raise SessionError("verified model manifest has no geometry")
    return manifest


def _host_facts() -> dict[str, object]:
    facts: dict[str, object] = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
    }
    for key, name in (("memory_bytes", "hw.memsize"), ("wired_limit_mb", "iogpu.wired_limit_mb")):
        result = subprocess.run(
            ["sysctl", "-n", name],
            capture_output=True,
            text=True,
            check=False,
        )
        value = result.stdout.strip()
        facts[key] = int(value) if result.returncode == 0 and value.isdigit() else None
    return facts


def _run_pre_session_disk_gate() -> None:
    result = subprocess.run(
        [
            sys.executable,
            str(REPO_ROOT / "scripts" / "disk_gate.py"),
            "--label",
            "Before M0a interactive session",
            "--expected-bytes",
            "500000000",
        ],
        check=False,
    )
    if result.returncode != 0:
        raise SessionError("pre-session disk gate failed")


def _private_directory(path: Path, *, parents: bool = False, exist_ok: bool = False) -> None:
    path.mkdir(mode=0o700, parents=parents, exist_ok=exist_ok)
    os.chmod(path, 0o700)


def run_session(
    model: Path,
    client_profile: str,
    *,
    artifacts: Path = DEFAULT_ARTIFACTS,
    server: Path = DEFAULT_SERVER,
    patch_path: Path = DEFAULT_PATCH,
    session_id: UUID | None = None,
    run_disk_gate: Callable[[], None] = _run_pre_session_disk_gate,
    popen_factory: Callable[..., subprocess.Popen[bytes]] = subprocess.Popen,
    speculation: str = "none",
    draft_tokens: int | None = None,
    monitor_rss: Callable[
        [subprocess.Popen[bytes], threading.Event, list[int]], None
    ] = _monitor_rss,
) -> dict[str, object]:
    old_umask = os.umask(0o077)
    child = None
    old_handlers: dict[int, object] = {}
    received_signal: int | None = None
    sidecar: dict[str, object] | None = None
    sidecar_path: Path | None = None
    peak_rss = [0]
    stop_monitor = threading.Event()
    monitor: threading.Thread | None = None
    try:
        manifest = _read_manifest(model)
        run_disk_gate()
        _private_directory(artifacts, parents=True, exist_ok=True)
        actual_session_id = uuid4() if session_id is None else session_id
        routing_path = artifacts / f"routing-{actual_session_id}.bin"
        sidecar_path = artifacts / f"routing-{actual_session_id}.json"
        prompt_dir = artifacts / f"prompts-{actual_session_id}"
        server_log_path = (artifacts / f"server-{actual_session_id}.log").resolve()
        collisions = [
            path
            for path in (routing_path, sidecar_path, prompt_dir, server_log_path)
            if path.exists()
        ]
        if collisions:
            raise SessionError(f"session path already exists: {collisions[0]}")
        _private_directory(prompt_dir)

        command = session_command(
            model,
            prompt_dir,
            server=server,
            speculation=speculation,
            draft_tokens=draft_tokens,
            server_log_path=server_log_path,
        )
        patch_hash = sha256_file(patch_path) if patch_path.is_file() else None
        sidecar = {
            "schema": 2,
            "status": "running",
            "session_id": str(actual_session_id),
            "client_profile": client_profile,
            "speculation": {
                "type": speculation,
                "draft_tokens": draft_tokens,
            },
            "started_at": _utc_now(),
            "ended_at": None,
            "exit_code": None,
            "server_pid": None,
            "peak_rss_kb": None,
            "command": command,
            "host": _host_facts(),
            "model_path": str(model.resolve()),
            "model_sha256": manifest["sha256"],
            "patch_path": str(patch_path.resolve()),
            "patch_sha256": patch_hash,
            "prompt_directory": str(prompt_dir.resolve()),
            "server_log_path": str(server_log_path),
            "routing_path": str(routing_path.resolve()),
            "routing_sha256": None,
        }
        _write_json_atomic(sidecar_path, sidecar)
        environment = routing_environment(manifest, routing_path, actual_session_id)
        child = popen_factory(command, env=environment, start_new_session=True)
        sidecar["server_pid"] = child.pid
        _write_json_atomic(sidecar_path, sidecar)
        monitor = threading.Thread(
            target=monitor_rss,
            args=(child, stop_monitor, peak_rss),
            daemon=True,
        )
        monitor.start()

        def forward(signum: int, frame: object) -> None:
            del frame
            nonlocal received_signal
            received_signal = signum
            if child is not None and child.poll() is None:
                child.send_signal(signum)

        if threading.current_thread() is threading.main_thread():
            for signum in (signal.SIGINT, signal.SIGTERM):
                old_handlers[signum] = signal.getsignal(signum)
                signal.signal(signum, forward)

        try:
            exit_code = child.wait()
        except KeyboardInterrupt:
            received_signal = signal.SIGINT
            if child.poll() is None:
                child.send_signal(signal.SIGINT)
            exit_code = child.wait()
        sidecar["exit_code"] = exit_code
        sidecar["status"] = "complete" if exit_code == 0 and received_signal is None else "interrupted"
        return sidecar
    except SessionError:
        raise
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        if sidecar is None:
            raise SessionError(str(error)) from error
        sidecar["status"] = "interrupted"
        sidecar["error"] = str(error)
        raise SessionError(str(error)) from error
    finally:
        stop_monitor.set()
        if monitor is not None:
            monitor.join(timeout=2)
        if sidecar is not None:
            sidecar["peak_rss_kb"] = peak_rss[0] or None
        for signum, handler in old_handlers.items():
            signal.signal(signum, handler)
        if sidecar is not None and sidecar_path is not None:
            sidecar["ended_at"] = _utc_now()
            routing_path = Path(str(sidecar["routing_path"]))
            if routing_path.is_file():
                sidecar["routing_sha256"] = sha256_file(routing_path)
            _write_json_atomic(sidecar_path, sidecar)
        os.umask(old_umask)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--client-profile", required=True)
    parser.add_argument("--artifacts", type=Path, default=DEFAULT_ARTIFACTS)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument(
        "--speculation",
        choices=("none", "draft-mtp"),
        default="none",
    )
    parser.add_argument("--draft-tokens", type=int)
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        _read_manifest(args.model)
        if args.dry_run:
            preview_id = uuid4()
            prompt_dir = args.artifacts / f"prompts-{preview_id}"
            server_log_path = (args.artifacts / f"server-{preview_id}.log").resolve()
            print(
                shlex.join(
                    session_command(
                        args.model,
                        prompt_dir,
                        server=args.server,
                        speculation=args.speculation,
                        draft_tokens=args.draft_tokens,
                        server_log_path=server_log_path,
                    )
                )
            )
            return 0
        sidecar = run_session(
            args.model,
            args.client_profile,
            artifacts=args.artifacts,
            server=args.server,
            speculation=args.speculation,
            draft_tokens=args.draft_tokens,
        )
    except (SessionError, ValueError) as error:
        print(f"session failed: {error}")
        return 2
    print(json.dumps(sidecar, sort_keys=True))
    return 0 if sidecar["status"] == "complete" else 2


if __name__ == "__main__":
    raise SystemExit(main())
