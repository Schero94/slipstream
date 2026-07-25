"""Track V1: decision logic for the self-configuring Peregrine VS Code plugin.

The VS Code extension is a thin shell. All decisions that must be correct and
regression-tested live here so the existing Python suite can cover them:

* building the exact `bench.m1.gateway` serve/status/stop command lines,
* interpreting a `gateway status` result into a status-bar state, and
* planning (and, only on explicit confirmation, applying) the settings
  injection that points an installed OpenAI-compatible agent extension at the
  resident local gateway.

Hard rules mirrored from Plan v3:
* loopback only,
* one slot / one endpoint,
* never read, echo, or write a user's real API key; the local endpoint needs no
  real secret, so a fixed local placeholder is injected instead,
* config writes are fail-closed and require explicit confirmation.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Mapping

GATEWAY_MODULE = "bench.m1.gateway"
DEFAULT_MODEL_NAME = "peregrine-qualification"
# The local gateway authenticates by loopback + instance token, not by API key.
# Clients that require a non-empty key get this fixed local placeholder; a real
# secret is never read from or written to any config file.
LOCAL_API_KEY_PLACEHOLDER = "peregrine-local"
PEREGRINE_MODEL_TITLE = "Peregrine Local"


class PluginError(Exception):
    """Raised for invalid gateway launch or status inputs."""


class AgentConfigError(PluginError):
    """Raised for unknown clients or unsafe/unconfirmed config operations."""


def _validate_port(port: int) -> int:
    if isinstance(port, bool) or not isinstance(port, int) or not (1 <= port <= 65535):
        raise PluginError(f"port must be an integer in 1..65535, got {port!r}")
    return port


def endpoint_base_url(port: int, host: str = "127.0.0.1") -> str:
    """The stable loopback OpenAI base URL the agent should target."""
    _validate_port(port)
    if host not in {"127.0.0.1", "::1", "localhost"}:
        raise PluginError("gateway endpoint must be loopback")
    return f"http://{host}:{port}/v1"


@dataclass(frozen=True)
class GatewayLaunchConfig:
    model: Path
    server: Path
    port: int
    state: Path
    python_executable: str = field(default_factory=lambda: sys.executable)
    host: str = "127.0.0.1"
    warmstart_evidence: Path | None = None
    warmstart_store: Path | None = None


def build_serve_command(config: GatewayLaunchConfig) -> list[str]:
    _validate_port(config.port)
    if config.host not in {"127.0.0.1", "::1", "localhost"}:
        raise PluginError("gateway host must be loopback")
    has_evidence = config.warmstart_evidence is not None
    has_store = config.warmstart_store is not None
    if has_evidence != has_store:
        raise PluginError("warmstart requires both --warmstart-evidence and --warmstart-store")
    command = [
        config.python_executable,
        "-m",
        GATEWAY_MODULE,
        "serve",
        "--model",
        str(config.model),
        "--server",
        str(config.server),
        "--host",
        config.host,
        "--port",
        str(config.port),
        "--state",
        str(config.state),
    ]
    if has_evidence and has_store:
        command += [
            "--warmstart-evidence",
            str(config.warmstart_evidence),
            "--warmstart-store",
            str(config.warmstart_store),
        ]
    return command


def _control_command(config: GatewayLaunchConfig, name: str) -> list[str]:
    return [
        config.python_executable,
        "-m",
        GATEWAY_MODULE,
        name,
        "--state",
        str(config.state),
    ]


def build_status_command(config: GatewayLaunchConfig) -> list[str]:
    return _control_command(config, "status")


def build_stop_command(config: GatewayLaunchConfig) -> list[str]:
    return _control_command(config, "stop")


@dataclass(frozen=True)
class GatewayStatus:
    state: str  # "running" | "unhealthy" | "offline"
    label: str
    detail: str
    listen_url: str | None = None
    pid: int | None = None


def interpret_status(exit_code: int, stdout: str) -> GatewayStatus:
    """Map a `gateway status` result to a status-bar state.

    * exit 0 + healthy, identity-consistent JSON -> running
    * exit 0 but malformed / not ok / identity mismatch -> unhealthy
    * non-zero exit (no state, dead process, failed health) -> offline
    """
    if exit_code != 0:
        return GatewayStatus(
            state="offline",
            label="Peregrine: off",
            detail="gateway not running",
        )
    try:
        payload = json.loads(stdout)
        state = payload["state"]
        health = payload["health"]
    except (json.JSONDecodeError, KeyError, TypeError):
        return GatewayStatus(
            state="unhealthy",
            label="Peregrine: unhealthy",
            detail="status output could not be parsed",
        )
    identity_ok = (
        isinstance(health, Mapping)
        and health.get("status") == "ok"
        and health.get("pid") == state.get("pid")
        and health.get("instance_token") == state.get("instance_token")
    )
    listen_url = state.get("listen_url")
    if not identity_ok:
        return GatewayStatus(
            state="unhealthy",
            label="Peregrine: unhealthy",
            detail="health identity does not match recorded state",
            listen_url=listen_url if isinstance(listen_url, str) else None,
        )
    model_sha = str(state.get("model_sha256", ""))[:12]
    engine = str(state.get("engine_commit", ""))[:12]
    warm = state.get("warmstart_tokens", 0)
    pid = state.get("pid")
    detail = f"model {model_sha} engine {engine} warm {warm} tokens"
    return GatewayStatus(
        state="running",
        label="Peregrine: on",
        detail=detail,
        listen_url=listen_url if isinstance(listen_url, str) else None,
        pid=pid if isinstance(pid, int) and not isinstance(pid, bool) else None,
    )


@dataclass(frozen=True)
class AgentConfigPlan:
    client: str
    target_path: Path
    merged_config: dict[str, Any]
    summary: str


def _strip_secrets(value: Any) -> Any:
    """Return a deep copy with any api-key-like fields removed.

    Existing user secrets are never carried into the plan we render or display.
    """
    secret_keys = {"apikey", "api_key", "apikey", "token", "secret", "authorization"}
    if isinstance(value, Mapping):
        return {
            key: _strip_secrets(item)
            for key, item in value.items()
            if key.lower() not in secret_keys
        }
    if isinstance(value, list):
        return [_strip_secrets(item) for item in value]
    return value


def _load_existing(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    if path.is_symlink() or not path.is_file():
        raise AgentConfigError(f"existing config is not a regular file: {path}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise AgentConfigError(f"cannot read existing config {path}: {error}") from error
    if not isinstance(data, dict):
        raise AgentConfigError("existing config is not a JSON object")
    return data


def _plan_continue(port: int, home: Path) -> AgentConfigPlan:
    target = home / ".continue" / "config.json"
    existing = _strip_secrets(_load_existing(target))
    models = list(existing.get("models", []) if isinstance(existing.get("models"), list) else [])
    entry = {
        "title": PEREGRINE_MODEL_TITLE,
        "provider": "openai",
        "model": DEFAULT_MODEL_NAME,
        "apiBase": endpoint_base_url(port),
        "apiKey": LOCAL_API_KEY_PLACEHOLDER,
    }
    models = [m for m in models if not (isinstance(m, Mapping) and m.get("title") == PEREGRINE_MODEL_TITLE)]
    models.insert(0, entry)
    merged = dict(existing)
    merged["models"] = models
    return AgentConfigPlan(
        client="continue",
        target_path=target,
        merged_config=merged,
        summary=f"Add/replace Continue model '{PEREGRINE_MODEL_TITLE}' -> {entry['apiBase']}",
    )


def _plan_openai_generic(port: int, home: Path) -> AgentConfigPlan:
    target = home / ".peregrine" / "openai-endpoint.json"
    merged = {
        "base_url": endpoint_base_url(port),
        "model": DEFAULT_MODEL_NAME,
        "api_key": LOCAL_API_KEY_PLACEHOLDER,
    }
    return AgentConfigPlan(
        client="openai-generic",
        target_path=target,
        merged_config=merged,
        summary=f"Write generic OpenAI-compatible endpoint file -> {merged['base_url']}",
    )


AGENT_CLIENTS: dict[str, Callable[[int, Path], AgentConfigPlan]] = {
    "continue": _plan_continue,
    "openai-generic": _plan_openai_generic,
}


def plan_agent_config(client: str, *, port: int, home: Path | None = None) -> AgentConfigPlan:
    if client not in AGENT_CLIENTS:
        raise AgentConfigError(
            f"unknown agent client {client!r}; known: {sorted(AGENT_CLIENTS)}"
        )
    _validate_port(port)
    resolved_home = home if home is not None else Path.home()
    return AGENT_CLIENTS[client](port, resolved_home)


def apply_agent_config(plan: AgentConfigPlan, *, confirm: bool) -> Path:
    if not confirm:
        raise AgentConfigError("config apply requires explicit confirmation")
    target = plan.target_path
    if target.is_symlink():
        raise AgentConfigError(f"refusing to write through a symlink: {target}")
    if target.exists() and not target.is_file():
        raise AgentConfigError(f"refusing to overwrite non-regular file: {target}")
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    body = json.dumps(plan.merged_config, indent=2, sort_keys=True) + "\n"
    fd, tmp_name = tempfile.mkstemp(dir=str(target.parent), prefix=".peregrine-config-")
    tmp_path = Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(body)
        os.chmod(tmp_path, 0o600)
        os.replace(tmp_path, target)
    finally:
        if tmp_path.exists():
            tmp_path.unlink()
    return target


def _plan_as_json(plan: AgentConfigPlan) -> dict[str, Any]:
    return {
        "client": plan.client,
        "target_path": str(plan.target_path),
        "merged_config": plan.merged_config,
        "summary": plan.summary,
    }


def main(argv: list[str] | None = None) -> int:
    """Thin CLI so the VS Code extension delegates every decision here."""
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    endpoint = sub.add_parser("endpoint")
    endpoint.add_argument("--port", type=int, default=8080)

    plan_parser = sub.add_parser("plan-config")
    plan_parser.add_argument("--client", required=True)
    plan_parser.add_argument("--port", type=int, default=8080)
    plan_parser.add_argument("--home", type=Path, default=None)

    apply_parser = sub.add_parser("apply-config")
    apply_parser.add_argument("--client", required=True)
    apply_parser.add_argument("--port", type=int, default=8080)
    apply_parser.add_argument("--home", type=Path, default=None)
    apply_parser.add_argument("--confirm", action="store_true")

    args = parser.parse_args(argv)
    try:
        if args.command == "endpoint":
            print(json.dumps({"base_url": endpoint_base_url(args.port)}))
            return 0
        if args.command == "plan-config":
            plan = plan_agent_config(args.client, port=args.port, home=args.home)
            print(json.dumps(_plan_as_json(plan), indent=2, sort_keys=True))
            return 0
        if args.command == "apply-config":
            plan = plan_agent_config(args.client, port=args.port, home=args.home)
            target = apply_agent_config(plan, confirm=args.confirm)
            print(json.dumps({"applied": True, "target_path": str(target)}))
            return 0
    except PluginError as error:
        print(f"plugin error: {error}", file=sys.stderr)
        return 2
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
