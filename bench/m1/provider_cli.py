"""Consent-bound official CLI adapters for Peregrine Track E.

This module can probe installed CLI builds without sending a prompt. Provider
execution is possible only with a matching, single-use consent grant and an
isolated linked Git worktree.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from typing import Callable, Mapping, Sequence


PROVIDERS = {"claude", "codex"}
SAFE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
MAX_BRIEFING_BYTES = 128 * 1024
REQUIRED_FLAGS = {
    "claude": (
        "--print",
        "--output-format",
        "--json-schema",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--allowed-tools",
        "--permission-mode",
    ),
    "codex": (
        "--ignore-user-config",
        "--strict-config",
        "--sandbox",
        "--ephemeral",
        "--output-schema",
        "--json",
    ),
}
OUTPUT_SCHEMA: Mapping[str, object] = {
    "type": "object",
    "properties": {
        "patch": {"type": "string"},
        "local_failure_cause": {"type": "string"},
        "generalizable_lesson": {"type": "string"},
    },
    "required": ["patch", "local_failure_cause", "generalizable_lesson"],
    "additionalProperties": False,
}


class ProviderCliError(RuntimeError):
    pass


Runner = Callable[..., subprocess.CompletedProcess[str]]


@dataclass(frozen=True)
class CliProbe:
    provider: str
    executable: str
    version: str


@dataclass(frozen=True)
class ProviderInvocation:
    provider: str
    incident_id: str
    worktree: Path
    briefing_sha256: str
    argv: tuple[str, ...]
    stdin: str
    instruction_filename: str
    schema_filename: str = ".peregrine-response-schema.json"


@dataclass(frozen=True)
class ConsentGrant:
    grant_id: str
    incident_id: str
    provider: str
    briefing_sha256: str
    consent: str = "granted"
    schema: int = 1

    @classmethod
    def create(cls, incident_id: str, provider: str, briefing_sha256: str) -> "ConsentGrant":
        _validate_identity(incident_id, provider, briefing_sha256)
        identity = {
            "incident_id": incident_id,
            "provider": provider,
            "briefing_sha256": briefing_sha256,
            "consent": "granted",
            "schema": 1,
        }
        encoded = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
        return cls(hashlib.sha256(encoded).hexdigest(), **identity)

    def validate(self) -> None:
        _validate_identity(self.incident_id, self.provider, self.briefing_sha256)
        if self.schema != 1 or self.consent != "granted" or HEX64.fullmatch(self.grant_id) is None:
            raise ProviderCliError("consent grant is invalid")
        identity = asdict(self)
        identity.pop("grant_id")
        encoded = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
        if hashlib.sha256(encoded).hexdigest() != self.grant_id:
            raise ProviderCliError("consent grant identity mismatch")


def _validate_identity(incident_id: str, provider: str, briefing_sha256: str) -> None:
    if SAFE_ID.fullmatch(incident_id) is None:
        raise ProviderCliError("incident ID is unsafe")
    if provider not in PROVIDERS:
        raise ProviderCliError(f"unsupported provider: {provider}")
    if HEX64.fullmatch(briefing_sha256) is None:
        raise ProviderCliError("briefing SHA-256 is invalid")


def _run_probe(runner: Runner, argv: Sequence[str]) -> subprocess.CompletedProcess[str]:
    try:
        result = runner(
            tuple(argv),
            input="",
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
            shell=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise ProviderCliError(f"cannot probe provider CLI: {error}") from error
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ProviderCliError(f"provider CLI probe failed: {detail}")
    return result


def probe_cli(
    provider: str,
    *,
    executable: str | None = None,
    runner: Runner = subprocess.run,
) -> CliProbe:
    if provider not in PROVIDERS:
        raise ProviderCliError(f"unsupported provider: {provider}")
    binary = executable or provider
    version = _run_probe(runner, (binary, "--version")).stdout.strip()
    help_argv = (binary, "--help") if provider == "claude" else (binary, "exec", "--help")
    help_text = _run_probe(runner, help_argv).stdout
    missing = [flag for flag in REQUIRED_FLAGS[provider] if flag not in help_text]
    if missing:
        raise ProviderCliError(f"provider CLI is missing required flags: {', '.join(missing)}")
    return CliProbe(provider, binary, version)


def _validate_worktree_path(worktree: Path) -> Path:
    if not worktree.is_absolute():
        raise ProviderCliError("worktree path must be absolute")
    if worktree.is_symlink() or not worktree.is_dir():
        raise ProviderCliError("worktree must be a real directory")
    return worktree


def build_invocation(
    provider: str,
    *,
    incident_id: str,
    worktree: Path,
    briefing: str,
    executable: str | None = None,
) -> ProviderInvocation:
    digest = hashlib.sha256(briefing.encode("utf-8")).hexdigest()
    _validate_identity(incident_id, provider, digest)
    worktree = _validate_worktree_path(worktree)
    if not briefing.strip():
        raise ProviderCliError("briefing is empty")
    if len(briefing.encode("utf-8")) > MAX_BRIEFING_BYTES:
        raise ProviderCliError("briefing exceeds 128 KiB")
    binary = executable or provider
    schema = json.dumps(OUTPUT_SCHEMA, sort_keys=True, separators=(",", ":"))
    if provider == "claude":
        argv = (
            binary,
            "-p",
            "--output-format",
            "json",
            "--json-schema",
            schema,
            "--no-session-persistence",
            "--strict-mcp-config",
            "--mcp-config",
            "{}",
            "--permission-mode",
            "acceptEdits",
            "--allowed-tools",
            "Read,Edit,Write,Glob,Grep,Bash(python3 *)",
        )
        instruction_filename = "CLAUDE.md"
    else:
        schema_path = worktree / ".peregrine-response-schema.json"
        argv = (
            binary,
            "exec",
            "--ignore-user-config",
            "--strict-config",
            "-c",
            'model_provider="openai"',
            "--sandbox",
            "workspace-write",
            "-C",
            str(worktree),
            "--ephemeral",
            "--output-schema",
            str(schema_path),
            "--json",
            "-",
        )
        instruction_filename = "AGENTS.md"
    return ProviderInvocation(
        provider,
        incident_id,
        worktree,
        digest,
        argv,
        briefing,
        instruction_filename,
    )


def _validate_linked_worktree(worktree: Path) -> None:
    marker = worktree / ".git"
    if marker.is_symlink() or not marker.is_file():
        raise ProviderCliError("provider execution requires an isolated linked Git worktree")
    try:
        value = marker.read_text(encoding="utf-8").strip()
    except OSError as error:
        raise ProviderCliError(f"cannot inspect worktree marker: {error}") from error
    if not value.startswith("gitdir: "):
        raise ProviderCliError("provider execution requires an isolated linked Git worktree")
    git_dir = Path(value.removeprefix("gitdir: "))
    if not git_dir.is_absolute():
        git_dir = (worktree / git_dir).resolve()
    if git_dir.is_symlink() or not git_dir.is_dir() or "worktrees" not in git_dir.parts:
        raise ProviderCliError("provider execution requires an isolated linked Git worktree")


def _write_exclusive(path: Path, data: bytes, *, mode: int = 0o600) -> None:
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    except FileExistsError as error:
        raise ProviderCliError(f"refusing to replace provider artifact: {path.name}") from error
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        path.unlink(missing_ok=True)
        raise


def execute_invocation(
    invocation: ProviderInvocation,
    consent: ConsentGrant | None,
    *,
    receipt_root: Path,
    runner: Runner = subprocess.run,
    timeout_seconds: int = 900,
) -> subprocess.CompletedProcess[str]:
    if consent is None:
        raise ProviderCliError("explicit per-incident consent is required")
    consent.validate()
    if (
        consent.incident_id != invocation.incident_id
        or consent.provider != invocation.provider
        or consent.briefing_sha256 != invocation.briefing_sha256
    ):
        raise ProviderCliError("consent grant does not match this provider invocation")
    if not invocation.argv:
        raise ProviderCliError("provider invocation is not canonical")
    canonical = build_invocation(
        invocation.provider,
        incident_id=invocation.incident_id,
        worktree=invocation.worktree,
        briefing=invocation.stdin,
        executable=invocation.argv[0],
    )
    if invocation != canonical:
        raise ProviderCliError("provider invocation is not canonical")
    worktree = _validate_worktree_path(invocation.worktree)
    _validate_linked_worktree(worktree)
    receipt_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    if receipt_root.is_symlink() or not receipt_root.is_dir():
        raise ProviderCliError("consent receipt root must be a real directory")
    receipt_root.chmod(0o700)
    receipt = receipt_root / f"{consent.grant_id}.json"
    if receipt.exists():
        raise ProviderCliError("consent grant was already consumed")

    instruction = worktree / invocation.instruction_filename
    schema_path = worktree / invocation.schema_filename
    _write_exclusive(instruction, invocation.stdin.encode("utf-8"))
    _write_exclusive(
        schema_path,
        (json.dumps(OUTPUT_SCHEMA, indent=2, sort_keys=True) + "\n").encode("utf-8"),
    )
    receipt_data = {
        "grant_id": consent.grant_id,
        "incident_id": consent.incident_id,
        "provider": consent.provider,
        "briefing_sha256": consent.briefing_sha256,
        "state": "consumed_before_launch",
        "schema": 1,
    }
    _write_exclusive(
        receipt,
        (json.dumps(receipt_data, indent=2, sort_keys=True) + "\n").encode("utf-8"),
    )
    try:
        return runner(
            invocation.argv,
            input=invocation.stdin,
            text=True,
            capture_output=True,
            timeout=timeout_seconds,
            check=False,
            shell=False,
            cwd=worktree,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise ProviderCliError(f"provider CLI launch failed: {error}") from error
