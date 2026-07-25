"""Resident OpenAI-compatible localhost gateway for Peregrine W1."""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import hashlib
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import signal
import subprocess
import threading
from typing import Mapping
from urllib.parse import urlsplit

from bench.m0a.qualify_model import (
    QUALIFICATION_PROFILES,
    _read_manifest,
    profile_environment,
    qualification_server_command,
)
from bench.m0a.smoke_server import DEFAULT_SERVER, _json_request, _unused_port, _wait_for_health, _write_json_atomic
from bench.m1.warmstart import SlotClient, WarmstartKey, WarmstartStore


PROFILE = QUALIFICATION_PROFILES["baseline-f16-fa-mtp4"]
MAX_BODY_BYTES = 16 * 1024 * 1024
GENERATION_PATHS = {
    "/completion",
    "/v1/completions",
    "/chat/completions",
    "/v1/chat/completions",
    "/responses",
    "/v1/responses",
}
HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
}
HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX_COMMIT = re.compile(r"^[0-9a-f]{7,64}$")
REPO_ROOT = Path(__file__).resolve().parents[2]


class GatewayError(RuntimeError):
    pass


def validate_loopback(host: str) -> None:
    if host == "localhost":
        return
    try:
        address = ipaddress.ip_address(host)
    except ValueError as error:
        raise GatewayError("gateway addresses must be loopback IPs or localhost") from error
    if not address.is_loopback:
        raise GatewayError("gateway addresses must be loopback")


@dataclass(frozen=True)
class GatewayState:
    pid: int
    instance_token: str
    listen_url: str
    upstream_url: str
    model_sha256: str
    engine_commit: str
    server_sha256: str
    started_at: str
    warmstart_restored: bool
    warmstart_tokens: int


def _validate_state(data: Mapping[str, object]) -> GatewayState:
    if data.get("schema") != 1:
        raise GatewayError("unsupported gateway state schema")
    try:
        state = GatewayState(**{field: data[field] for field in GatewayState.__dataclass_fields__})
    except (KeyError, TypeError) as error:
        raise GatewayError("malformed gateway state") from error
    if (
        isinstance(state.pid, bool)
        or not isinstance(state.pid, int)
        or state.pid <= 0
        or not isinstance(state.instance_token, str)
        or not HEX64.fullmatch(state.instance_token)
        or not isinstance(state.model_sha256, str)
        or not HEX64.fullmatch(state.model_sha256)
        or not isinstance(state.server_sha256, str)
        or not HEX64.fullmatch(state.server_sha256)
        or not isinstance(state.engine_commit, str)
        or not HEX_COMMIT.fullmatch(state.engine_commit)
        or not isinstance(state.warmstart_restored, bool)
        or isinstance(state.warmstart_tokens, bool)
        or not isinstance(state.warmstart_tokens, int)
        or state.warmstart_tokens < 0
    ):
        raise GatewayError("invalid gateway state fields")
    for url in (state.listen_url, state.upstream_url):
        parsed = urlsplit(url)
        if parsed.scheme != "http" or parsed.hostname is None:
            raise GatewayError("gateway state contains an invalid URL")
        validate_loopback(parsed.hostname)
    return state


def save_state(path: Path, state: GatewayState) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    _write_json_atomic(path, {"schema": 1, **asdict(state)})
    path.chmod(0o600)


def load_state(path: Path) -> GatewayState:
    if path.is_symlink() or not path.is_file():
        raise GatewayError("gateway state must be a regular non-symlink file")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise GatewayError(f"cannot read gateway state: {error}") from error
    if not isinstance(data, dict):
        raise GatewayError("gateway state is not an object")
    return _validate_state(data)


def transform_request_body(
    path: str,
    content_type: str,
    body: bytes,
    *,
    warm_slot: bool,
) -> bytes:
    if not warm_slot:
        return body
    clean_path = path.split("?", 1)[0]
    if clean_path not in GENERATION_PATHS or "application/json" not in content_type.lower():
        return body
    try:
        data = json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return body
    if not isinstance(data, dict) or "id_slot" in data:
        return body
    data["id_slot"] = 0
    data["cache_prompt"] = True
    return json.dumps(data, separators=(",", ":")).encode("utf-8")


@dataclass
class GatewayRuntime:
    upstream_host: str
    upstream_port: int
    instance_token: str
    warm_slot: bool
    state: GatewayState | None = None


def _handler(runtime: GatewayRuntime):
    class GatewayHandler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format: str, *args: object) -> None:
            return

        def _json(self, status: int, value: Mapping[str, object]) -> None:
            body = json.dumps(value, separators=(",", ":")).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True

        def _proxy(self) -> None:
            if self.path.split("?", 1)[0] in {"/peregrine/health", "/peregrine/status"}:
                state = runtime.state
                self._json(
                    200,
                    {
                        "status": "ok",
                        "pid": os.getpid(),
                        "instance_token": runtime.instance_token,
                        "warmstart_restored": runtime.warm_slot,
                        "warmstart_tokens": state.warmstart_tokens if state else 0,
                    },
                )
                return
            raw_length = self.headers.get("Content-Length", "0")
            try:
                length = int(raw_length)
            except ValueError:
                self._json(400, {"error": "invalid Content-Length"})
                return
            if length < 0 or length > MAX_BODY_BYTES:
                self._json(413, {"error": "request body too large"})
                return
            body = self.rfile.read(length) if length else b""
            body = transform_request_body(
                self.path,
                self.headers.get("Content-Type", ""),
                body,
                warm_slot=runtime.warm_slot,
            )
            headers = {
                key: value
                for key, value in self.headers.items()
                if key.lower() not in HOP_HEADERS and key.lower() not in {"host", "content-length"}
            }
            if body or self.command in {"POST", "PUT", "PATCH"}:
                headers["Content-Length"] = str(len(body))
            connection = HTTPConnection(runtime.upstream_host, runtime.upstream_port, timeout=3600)
            try:
                connection.request(self.command, self.path, body=body or None, headers=headers)
                response = connection.getresponse()
                self.send_response(response.status, response.reason)
                for key, value in response.getheaders():
                    if key.lower() not in HOP_HEADERS and key.lower() not in {"content-length"}:
                        self.send_header(key, value)
                self.send_header("Connection", "close")
                self.end_headers()
                while chunk := response.read(64 * 1024):
                    self.wfile.write(chunk)
                    self.wfile.flush()
            except (OSError, TimeoutError) as error:
                if not self.wfile.closed:
                    try:
                        self._json(502, {"error": f"upstream unavailable: {error}"})
                    except OSError:
                        pass
            finally:
                connection.close()
                self.close_connection = True

        do_GET = _proxy
        do_POST = _proxy
        do_PUT = _proxy
        do_PATCH = _proxy
        do_DELETE = _proxy
        do_OPTIONS = _proxy

    return GatewayHandler


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except (OSError, ProcessLookupError):
        return False
    return True


def _warmstart_record(
    evidence_path: Path,
    store_path: Path,
    model: Path,
    server: Path,
    engine_commit: str,
):
    try:
        evidence = json.loads(evidence_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise GatewayError(f"cannot read warmstart evidence: {error}") from error
    manifest = _read_manifest(model)
    if (
        not isinstance(evidence, dict)
        or evidence.get("schema") != 2
        or not isinstance(evidence.get("decision"), dict)
        or evidence["decision"].get("passed") is not True
        or evidence.get("model_sha256") != manifest["sha256"]
        or evidence.get("engine_commit") != engine_commit
        or evidence.get("server_sha256") != _sha256(server)
    ):
        raise GatewayError("warmstart evidence identity or PASS gate is invalid")
    key = WarmstartKey(
        model_sha256=str(evidence["model_sha256"]),
        quantization=str(evidence["quantization"]),
        cache_type_k=str(evidence["cache_type_k"]),
        cache_type_v=str(evidence["cache_type_v"]),
        engine_commit=str(evidence["engine_commit"]),
        prompt_prefix_sha256=str(evidence["prompt_prefix_sha256"]),
    )
    record = WarmstartStore(store_path).load(key)
    if record.file_sha256 != evidence.get("slot_file_sha256"):
        raise GatewayError("warmstart store hash differs from evidence")
    return key, record


# Measured on this host (Q4-35B): a 65536-ctx slot pre-allocates ~6.1 GiB of KV +
# compute buffers, i.e. ~100 KiB per context token. Used only as an admission
# estimate (a safety net), overridable per model.
DEFAULT_KV_BYTES_PER_TOKEN = 102_400
DEFAULT_CTX_SIZE = 32_768  # headroom-safe default (was a hardcoded 65536 that starved RAM)


def gateway_resident_estimate(
    model_bytes: int, ctx_size: int, *, kv_bytes_per_token: int, overhead_bytes: int
) -> int:
    """Estimated resident footprint of a load: model file + KV(ctx) + runtime overhead."""
    if model_bytes <= 0 or ctx_size <= 0 or kv_bytes_per_token < 0 or overhead_bytes < 0:
        raise GatewayError("invalid resident-estimate inputs")
    return model_bytes + ctx_size * kv_bytes_per_token + overhead_bytes


def _admission_preflight(args: argparse.Namespace, model_bytes: int) -> dict | None:
    """Refuse/​warn a load that would starve the usability headroom (Plan v3 gate)."""
    from bench.m1.memory_admission import (
        GIB,
        detect_available_bytes,
        detect_total_bytes,
        evaluate_admission,
    )

    if getattr(args, "skip_admission", False):
        return None
    total = detect_total_bytes()
    if total is None:
        print("gateway: could not detect total memory; skipping admission check")
        return None
    available = detect_available_bytes(total)
    resident = gateway_resident_estimate(
        model_bytes,
        args.ctx_size,
        kv_bytes_per_token=args.kv_bytes_per_token,
        overhead_bytes=int(args.overhead_gb * GIB),
    )
    verdict = evaluate_admission(
        total_bytes=total,
        available_bytes=available,
        expected_resident_bytes=resident,
        min_headroom_bytes=int(args.min_headroom_gb * GIB),
    )
    if verdict["status"] == "REFUSE":
        rec_ctx = args.ctx_size
        # suggest the largest ctx that fits the ceiling, rounded down to 4096
        budget = verdict["static_ceiling_bytes"] - model_bytes - int(args.overhead_gb * GIB)
        if budget > 0 and args.kv_bytes_per_token > 0:
            rec_ctx = max(4096, (budget // args.kv_bytes_per_token) // 4096 * 4096)
        raise GatewayError(
            f"admission REFUSE: {verdict['reason']}. Would leave the Mac unusable. "
            f"Levers: --ctx-size {rec_ctx} (small effect on hybrid models where the "
            f"wired model weights dominate over KV), --n-gpu-layers <N> to offload some "
            f"layers off the GPU (lower wired footprint, slower decode), a smaller "
            f"resident model, or the streaming path. Recommended wired-limit "
            f"{verdict['recommended_wired_limit_mb']} MB. Use --skip-admission to override."
        )
    if verdict["status"] == "WARN":
        print(f"gateway: admission WARN — {verdict['reason']}")
    return verdict


def serve(args: argparse.Namespace) -> int:
    validate_loopback(args.host)
    if args.state.exists():
        try:
            old = load_state(args.state)
        except GatewayError:
            raise GatewayError("refusing malformed existing gateway state")
        if _pid_alive(old.pid):
            raise GatewayError(f"gateway is already running as PID {old.pid}")
        args.state.unlink()
    manifest = _read_manifest(args.model)
    engine_commit = subprocess.check_output(
        ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
        text=True,
    ).strip()
    upstream_port = _unused_port()
    command = qualification_server_command(
        args.model, upstream_port, server=args.server, profile=PROFILE
    )
    command.remove("--no-warmup")
    size_index = command.index("--ctx-size") + 1
    command[size_index] = str(args.ctx_size)
    if getattr(args, "n_gpu_layers", -1) >= 0 and "--gpu-layers" in command:
        command[command.index("--gpu-layers") + 1] = str(args.n_gpu_layers)
    _admission_preflight(args, args.model.stat().st_size)
    warm = None
    if args.warmstart_evidence:
        if not args.warmstart_store:
            raise GatewayError("--warmstart-store is required with evidence")
        warm = _warmstart_record(
            args.warmstart_evidence,
            args.warmstart_store,
            args.model,
            args.server,
            engine_commit,
        )
        save_path = str(args.warmstart_store)
        if not save_path.endswith("/"):
            save_path += "/"
        command.extend(["--slot-save-path", save_path])
    args.state.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    stdout_path = args.state.with_suffix(".upstream.stdout.log")
    stderr_path = args.state.with_suffix(".upstream.stderr.log")
    with stdout_path.open("ab") as stdout, stderr_path.open("ab") as stderr:
        child = subprocess.Popen(
            command,
            env=profile_environment(PROFILE),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            _wait_for_health(child, upstream_port)
            restored_tokens = 0
            if warm:
                key, record = warm
                restored = SlotClient(f"http://127.0.0.1:{upstream_port}").restore(0, key)
                restored_tokens = int(restored["n_restored"])
                if restored_tokens != record.n_saved:
                    raise GatewayError("gateway restore token count mismatch")
            token = secrets.token_hex(32)
            state = GatewayState(
                pid=os.getpid(),
                instance_token=token,
                listen_url=f"http://{args.host}:{args.port}",
                upstream_url=f"http://127.0.0.1:{upstream_port}",
                model_sha256=str(manifest["sha256"]),
                engine_commit=engine_commit,
                server_sha256=_sha256(args.server),
                started_at=datetime.now(timezone.utc).isoformat(),
                warmstart_restored=warm is not None,
                warmstart_tokens=restored_tokens,
            )
            runtime = GatewayRuntime("127.0.0.1", upstream_port, token, warm is not None, state)
            gateway = ThreadingHTTPServer((args.host, args.port), _handler(runtime))
            gateway.daemon_threads = True
            save_state(args.state, state)

            def shutdown(_signum: int, _frame: object) -> None:
                threading.Thread(target=gateway.shutdown, daemon=True).start()

            signal.signal(signal.SIGINT, shutdown)
            signal.signal(signal.SIGTERM, shutdown)
            gateway.serve_forever(poll_interval=0.2)
            gateway.server_close()
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=10)
            if args.state.exists():
                try:
                    current = load_state(args.state)
                    if current.pid == os.getpid():
                        args.state.unlink()
                except GatewayError:
                    pass
    return 0


def verified_state(path: Path) -> tuple[GatewayState, dict[str, object]]:
    state = load_state(path)
    try:
        health = _json_request(f"{state.listen_url}/peregrine/health", timeout=2)
    except Exception as error:
        raise GatewayError(f"gateway health verification failed: {error}") from error
    if health.get("pid") != state.pid or health.get("instance_token") != state.instance_token:
        raise GatewayError("gateway health identity differs from state")
    return state, health


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    serve_parser = sub.add_parser("serve")
    serve_parser.add_argument("--model", required=True, type=Path)
    serve_parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    serve_parser.add_argument("--host", default="127.0.0.1")
    serve_parser.add_argument("--port", type=int, default=8080)
    serve_parser.add_argument("--state", type=Path, default=Path("artifacts/gateway/state.json"))
    serve_parser.add_argument("--warmstart-evidence", type=Path)
    serve_parser.add_argument("--warmstart-store", type=Path)
    serve_parser.add_argument(
        "--ctx-size", type=int, default=DEFAULT_CTX_SIZE,
        help=f"context slot size (default {DEFAULT_CTX_SIZE}; larger pre-allocates more KV and can starve RAM)",
    )
    serve_parser.add_argument("--kv-bytes-per-token", type=int, default=DEFAULT_KV_BYTES_PER_TOKEN)
    serve_parser.add_argument(
        "--n-gpu-layers", type=int, default=-1,
        help="GPU-offloaded layers (default -1 = all). Lower it to reduce the wired "
        "footprint and keep the Mac usable, at a decode-speed cost (CPU layers).",
    )
    serve_parser.add_argument("--overhead-gb", type=float, default=1.0)
    serve_parser.add_argument("--min-headroom-gb", type=float, default=9.0)
    serve_parser.add_argument(
        "--skip-admission", action="store_true",
        help="bypass the usability-headroom pre-flight (may make the Mac unresponsive)",
    )
    for name in ("status", "stop"):
        item = sub.add_parser(name)
        item.add_argument("--state", type=Path, default=Path("artifacts/gateway/state.json"))
    args = parser.parse_args()
    try:
        if args.command == "serve":
            return serve(args)
        state, health = verified_state(args.state)
        if args.command == "status":
            print(json.dumps({"state": asdict(state), "health": health}, indent=2, sort_keys=True))
            return 0
        os.kill(state.pid, signal.SIGTERM)
        print(f"stopping Peregrine gateway PID {state.pid}")
        return 0
    except (GatewayError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"gateway failed: {error}")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
