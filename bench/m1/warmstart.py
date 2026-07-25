"""Identity-bound llama.cpp slot persistence for Peregrine W1."""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
from typing import Mapping
from urllib.parse import urlsplit

from bench.m0a.qualify_model import (
    QUALIFICATION_PROFILES,
    TOKEN_SEED_TEXT,
    _read_manifest,
    profile_environment,
)
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    SmokeError,
    _json_request,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)
from bench.m1.context_schedule import _token_sha256, build_sweep_command
from bench.m1.headroom import _command_output, parse_memory_pressure, parse_vm_stat


HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX_COMMIT = re.compile(r"^[0-9a-f]{7,64}$")
SAFE_NAME = re.compile(r"^[A-Za-z0-9_.-]+$")
PROFILE = QUALIFICATION_PROFILES["baseline-f16-fa-mtp4"]
WARMSTART_CONTEXT = 32_000
MAX_RECLAIM_BYTES = 512 * 1024
REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
FIXTURE_SUFFIXES = (
    "\nFix the failing unit test and report the exact command used.",
    "\nRefactor the parser without changing its public behavior.",
    "\nFind the regression, apply the smallest patch, and verify it.",
)


class WarmstartError(RuntimeError):
    pass


@dataclass(frozen=True)
class WarmstartKey:
    model_sha256: str
    quantization: str
    cache_type_k: str
    cache_type_v: str
    engine_commit: str
    prompt_prefix_sha256: str

    def __post_init__(self) -> None:
        if not HEX64.fullmatch(self.model_sha256):
            raise WarmstartError("model SHA-256 must be lowercase hex")
        if not HEX64.fullmatch(self.prompt_prefix_sha256):
            raise WarmstartError("prompt-prefix SHA-256 must be lowercase hex")
        if not HEX_COMMIT.fullmatch(self.engine_commit):
            raise WarmstartError("engine commit must be lowercase hex")
        for field in ("quantization", "cache_type_k", "cache_type_v"):
            value = getattr(self, field)
            if not value or not SAFE_NAME.fullmatch(value):
                raise WarmstartError(f"unsafe or empty warmstart identity field: {field}")

    def canonical(self) -> dict[str, object]:
        return {"schema": 1, **asdict(self)}

    @property
    def digest(self) -> str:
        encoded = json.dumps(
            self.canonical(), sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        return hashlib.sha256(encoded).hexdigest()

    @property
    def filename(self) -> str:
        return f"w1-{self.digest}.bin"

    @property
    def sidecar_filename(self) -> str:
        return f"w1-{self.digest}.json"


@dataclass(frozen=True)
class WarmstartRecord:
    key: WarmstartKey
    filename: str
    n_saved: int
    n_written: int
    file_sha256: str

    def to_json(self) -> dict[str, object]:
        return {
            "schema": 1,
            "key": self.key.canonical(),
            "filename": self.filename,
            "n_saved": self.n_saved,
            "n_written": self.n_written,
            "file_sha256": self.file_sha256,
        }


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


class WarmstartStore:
    def __init__(self, root: Path):
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)

    def _regular(self, path: Path, label: str) -> None:
        if path.is_symlink() or not path.is_file():
            raise WarmstartError(f"{label} must be a regular non-symlink file")

    def register(self, key: WarmstartKey, *, n_saved: int, n_written: int) -> WarmstartRecord:
        if n_saved <= 0 or n_written <= 0:
            raise WarmstartError("slot save counts must be positive")
        slot = self.root / key.filename
        self._regular(slot, "slot state")
        if slot.stat().st_size != n_written:
            raise WarmstartError("slot byte count differs from server response")
        record = WarmstartRecord(
            key=key,
            filename=key.filename,
            n_saved=n_saved,
            n_written=n_written,
            file_sha256=_sha256(slot),
        )
        _write_json_atomic(self.root / key.sidecar_filename, record.to_json())
        return record

    def load(self, key: WarmstartKey) -> WarmstartRecord:
        sidecar = self.root / key.sidecar_filename
        slot = self.root / key.filename
        self._regular(sidecar, "warmstart sidecar")
        self._regular(slot, "slot state")
        try:
            data = json.loads(sidecar.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise WarmstartError(f"cannot read warmstart sidecar: {error}") from error
        if not isinstance(data, dict) or data.get("schema") != 1:
            raise WarmstartError("unsupported warmstart sidecar schema")
        key_data = data.get("key")
        if not isinstance(key_data, dict) or key_data != key.canonical():
            raise WarmstartError("warmstart identity mismatch")
        filename = data.get("filename")
        n_saved = data.get("n_saved")
        n_written = data.get("n_written")
        file_sha256 = data.get("file_sha256")
        if (
            filename != key.filename
            or isinstance(n_saved, bool)
            or not isinstance(n_saved, int)
            or n_saved <= 0
            or isinstance(n_written, bool)
            or not isinstance(n_written, int)
            or n_written <= 0
            or not isinstance(file_sha256, str)
            or not HEX64.fullmatch(file_sha256)
        ):
            raise WarmstartError("malformed warmstart sidecar")
        if slot.stat().st_size != n_written or _sha256(slot) != file_sha256:
            raise WarmstartError("warmstart slot file failed size/hash validation")
        return WarmstartRecord(key, filename, n_saved, n_written, file_sha256)


class SlotClient:
    def __init__(self, base_url: str):
        parsed = urlsplit(base_url)
        if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "localhost", "::1"}:
            raise WarmstartError("slot endpoint must be loopback HTTP")
        if parsed.path not in ("", "/") or parsed.query or parsed.fragment:
            raise WarmstartError("slot endpoint must not contain a path, query, or fragment")
        self.base_url = base_url.rstrip("/")

    def _action(self, slot: int, action: str, key: WarmstartKey | None = None) -> dict[str, object]:
        if isinstance(slot, bool) or not isinstance(slot, int) or slot < 0:
            raise WarmstartError("slot ID must be a non-negative integer")
        body = {} if key is None else {"filename": key.filename}
        try:
            result = _json_request(
                f"{self.base_url}/slots/{slot}?action={action}", body, timeout=3600
            )
        except SmokeError as error:
            raise WarmstartError(f"slot {action} request failed: {error}") from error
        if result.get("id_slot") != slot:
            raise WarmstartError(f"slot {action} response has the wrong slot ID")
        if key is not None and result.get("filename") != key.filename:
            raise WarmstartError(f"slot {action} response has the wrong filename")
        if action == "save":
            self._positive(result, "n_saved")
            self._positive(result, "n_written")
        elif action == "restore":
            self._positive(result, "n_restored")
            self._positive(result, "n_read")
        return result

    @staticmethod
    def _positive(result: Mapping[str, object], field: str) -> int:
        value = result.get(field)
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            raise WarmstartError(f"slot response field {field} must be positive")
        return value

    def save(self, slot: int, key: WarmstartKey) -> dict[str, object]:
        return self._action(slot, "save", key)

    def restore(self, slot: int, key: WarmstartKey) -> dict[str, object]:
        return self._action(slot, "restore", key)

    def erase(self, slot: int) -> dict[str, object]:
        return self._action(slot, "erase")


def build_warmstart_command(
    model: Path | str,
    port: int,
    slot_path: Path,
    *,
    server: Path = DEFAULT_SERVER,
) -> list[str]:
    command = build_sweep_command(
        Path(model), port, PROFILE, WARMSTART_CONTEXT, server=server
    )
    save_path = str(slot_path)
    if not save_path.endswith("/"):
        save_path += "/"
    command.extend(["--slot-save-path", save_path])
    return command


def evaluate_warmstart(report: Mapping[str, object]) -> dict[str, object]:
    fixtures = report.get("fixtures")
    memory = report.get("memory")
    if not isinstance(fixtures, list) or len(fixtures) != 3 or not isinstance(memory, Mapping):
        raise WarmstartError("warmstart report is incomplete")
    parity = all(
        isinstance(fixture, Mapping)
        and fixture.get("cold_output_sha256") == fixture.get("warm_output_sha256")
        for fixture in fixtures
    )
    reclaim = int(memory["pageout_bytes_delta"]) + int(memory["swapin_bytes_delta"])
    memory_valid = (
        reclaim <= MAX_RECLAIM_BYTES
        and int(memory["swapouts_delta"]) == 0
        and int(memory["free_percent_after"]) >= int(memory["free_percent_before"]) - 1
    )
    counts_match = (
        int(report["saved_tokens"]) > 0
        and int(report["saved_tokens"]) == int(report["restored_tokens"])
    )
    ttft = float(report["warm_ttft_seconds"])
    passed = (
        ttft < 2.0
        and parity
        and counts_match
        and memory_valid
        and report.get("store_valid") is True
    )
    return {
        "passed": passed,
        "gateway_gate": "ELIGIBLE" if passed else "REJECTED",
        "warm_ttft_seconds": ttft,
        "threshold_seconds": 2.0,
        "fixture_parity": parity,
        "restore_count_match": counts_match,
        "memory_valid": memory_valid,
        "decode_reclaim_bytes": reclaim,
    }


def _completion(port: int, prompt: list[int]) -> dict[str, object]:
    started = time.monotonic()
    response = _json_request(
        f"http://127.0.0.1:{port}/completion",
        {
            "prompt": prompt,
            "id_slot": 0,
            "n_predict": 1,
            "ignore_eos": True,
            "temperature": 0,
            "seed": 42,
            "stream": False,
            "cache_prompt": True,
            "return_tokens": True,
        },
        timeout=3600,
    )
    timings = response.get("timings")
    tokens = response.get("tokens")
    if (
        not isinstance(timings, Mapping)
        or not isinstance(tokens, list)
        or len(tokens) != 1
        or not all(isinstance(token, int) for token in tokens)
        or timings.get("predicted_n") != 1
    ):
        raise WarmstartError("completion response lacks exact one-token evidence")
    return {
        "wall_seconds": time.monotonic() - started,
        "prompt_tokens_evaluated": timings.get("prompt_n"),
        "prompt_ms": timings.get("prompt_ms"),
        "prompt_tokens_per_second": timings.get("prompt_per_second"),
        "decode_ms": timings.get("predicted_ms"),
        "output_tokens": tokens,
        "output_sha256": _token_sha256(tokens),
    }


def _quantization(model: Path) -> str:
    match = re.search(r"-UD-([A-Za-z0-9_]+)\.gguf$", model.name)
    if not match:
        raise WarmstartError("cannot derive verified quantization from model filename")
    return match.group(1)


def _vm_delta(before: Mapping[str, int], after: Mapping[str, int]) -> dict[str, int]:
    if before["page_size_bytes"] != after["page_size_bytes"]:
        raise WarmstartError("vm_stat page size changed during warmstart measurement")
    page_size = before["page_size_bytes"]
    deltas = {
        f"{name}_delta": after[name] - before[name]
        for name in ("pageins", "pageouts", "swapins", "swapouts")
    }
    return {
        **deltas,
        "pageout_bytes_delta": max(0, deltas["pageouts_delta"]) * page_size,
        "swapin_bytes_delta": max(0, deltas["swapins_delta"]) * page_size,
    }


def run_warmstart(
    model: Path,
    output_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
) -> dict[str, object]:
    manifest = _read_manifest(model)
    output_dir.mkdir(parents=True, exist_ok=False)
    slot_dir = output_dir / "slots"
    store = WarmstartStore(slot_dir)
    port = _unused_port()
    command = build_warmstart_command(model, port, slot_dir, server=server)
    stdout_path = output_dir / "server.stdout.log"
    stderr_path = output_dir / "server.stderr.log"
    engine_commit = subprocess.check_output(
        ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
        text=True,
    ).strip()
    identity = {
        "model_sha256": manifest["sha256"],
        "engine_commit": engine_commit,
        "server_sha256": _sha256(server),
        "quantization": _quantization(model),
        "cache_type_k": PROFILE.cache_type_k,
        "cache_type_v": PROFILE.cache_type_v,
    }

    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            env=profile_environment(PROFILE),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            server_started = time.monotonic()
            _wait_for_health(process, port)
            model_ready_seconds = time.monotonic() - server_started
            tokenized = _json_request(
                f"http://127.0.0.1:{port}/tokenize",
                {"content": TOKEN_SEED_TEXT, "add_special": False, "parse_special": True},
            )
            seed = tokenized.get("tokens")
            if not isinstance(seed, list) or not all(isinstance(token, int) for token in seed):
                raise WarmstartError("token seed response is invalid")
            base = seed[:30_000]
            if len(base) != 30_000:
                raise WarmstartError("deterministic token seed is shorter than 30K")
            warm_key = WarmstartKey(
                model_sha256=str(manifest["sha256"]),
                quantization=identity["quantization"],
                cache_type_k=PROFILE.cache_type_k,
                cache_type_v=PROFILE.cache_type_v,
                engine_commit=engine_commit,
                prompt_prefix_sha256=_token_sha256(base),
            )
            client = SlotClient(f"http://127.0.0.1:{port}")

            client.erase(0)
            cold_base = _completion(port, base)
            save_started = time.monotonic()
            save = client.save(0, warm_key)
            save_wall_seconds = time.monotonic() - save_started
            record = store.register(
                warm_key,
                n_saved=int(save["n_saved"]),
                n_written=int(save["n_written"]),
            )
            store_valid = store.load(warm_key) == record
            _write_json_atomic(
                output_dir / "partial.json",
                {"schema": 1, **identity, "cold_base": cold_base, "save": save},
            )

            client.erase(0)
            vm_before = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_before = parse_memory_pressure(_command_output(["memory_pressure"]))
            restore_started = time.monotonic()
            restore = client.restore(0, warm_key)
            restore_wall_seconds = time.monotonic() - restore_started
            warm_base = _completion(port, base)
            vm_after = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_after = parse_memory_pressure(_command_output(["memory_pressure"]))
            memory = {
                **_vm_delta(vm_before, vm_after),
                "free_percent_before": pressure_before,
                "free_percent_after": pressure_after,
            }

            fixtures: list[dict[str, object]] = []
            for index, suffix in enumerate(FIXTURE_SUFFIXES):
                suffix_response = _json_request(
                    f"http://127.0.0.1:{port}/tokenize",
                    {"content": suffix, "add_special": False, "parse_special": True},
                )
                suffix_tokens = suffix_response.get("tokens")
                if not isinstance(suffix_tokens, list) or not all(isinstance(token, int) for token in suffix_tokens):
                    raise WarmstartError("fixture suffix tokenization is invalid")
                prompt = base + suffix_tokens
                client.erase(0)
                cold = _completion(port, prompt)
                client.erase(0)
                fixture_restore = client.restore(0, warm_key)
                warm = _completion(port, prompt)
                fixture = {
                    "fixture": index + 1,
                    "suffix_sha256": hashlib.sha256(suffix.encode("utf-8")).hexdigest(),
                    "suffix_tokens": len(suffix_tokens),
                    "cold_output_sha256": cold["output_sha256"],
                    "warm_output_sha256": warm["output_sha256"],
                    "cold_wall_seconds": cold["wall_seconds"],
                    "warm_wall_seconds": warm["wall_seconds"],
                    "cold_prompt_tokens_evaluated": cold["prompt_tokens_evaluated"],
                    "warm_prompt_tokens_evaluated": warm["prompt_tokens_evaluated"],
                    "restored_tokens": fixture_restore["n_restored"],
                }
                fixtures.append(fixture)
                _write_json_atomic(
                    output_dir / "partial.json",
                    {
                        "schema": 1,
                        **identity,
                        "cold_base": cold_base,
                        "save": save,
                        "restore": restore,
                        "warm_base": warm_base,
                        "fixtures": fixtures,
                    },
                )

            report: dict[str, object] = {
                "schema": 1,
                **identity,
                "command": command,
                "runtime_profile": asdict(PROFILE),
                "model_ready_seconds": model_ready_seconds,
                "base_prompt_tokens": len(base),
                "prompt_prefix_sha256": warm_key.prompt_prefix_sha256,
                "slot_filename": warm_key.filename,
                "slot_file_sha256": record.file_sha256,
                "slot_file_bytes": record.n_written,
                "cold_base": cold_base,
                "warm_base": warm_base,
                "save": save,
                "restore": restore,
                "save_wall_seconds": save_wall_seconds,
                "restore_wall_seconds": restore_wall_seconds,
                "warm_ttft_seconds": restore_wall_seconds + float(warm_base["wall_seconds"]),
                "saved_tokens": save["n_saved"],
                "restored_tokens": restore["n_restored"],
                "fixtures": fixtures,
                "memory": memory,
                "store_valid": store_valid,
                "m0a_admitted_tokens": 0,
            }
            report["decision"] = evaluate_warmstart(report)
            _write_json_atomic(output_dir / "warmstart.json", report)
            return report
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def run_warmstart_resume(
    model: Path,
    output_dir: Path,
    source_evidence: Path,
    *,
    server: Path = DEFAULT_SERVER,
) -> dict[str, object]:
    try:
        source = json.loads(source_evidence.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise WarmstartError(f"cannot read source warmstart evidence: {error}") from error
    if not isinstance(source, dict) or source.get("schema") != 1:
        raise WarmstartError("source warmstart evidence is not schema 1")
    manifest = _read_manifest(model)
    engine_commit = subprocess.check_output(
        ["git", "-C", str(REPO_ROOT / "vendor" / "llama.cpp"), "rev-parse", "HEAD"],
        text=True,
    ).strip()
    server_sha = _sha256(server)
    expected: dict[str, object] = {
        "model_sha256": manifest["sha256"],
        "engine_commit": engine_commit,
        "server_sha256": server_sha,
        "quantization": _quantization(model),
        "cache_type_k": PROFILE.cache_type_k,
        "cache_type_v": PROFILE.cache_type_v,
    }
    for field, value in expected.items():
        if source.get(field) != value:
            raise WarmstartError(f"source warmstart identity mismatch: {field}")
    fixtures = source.get("fixtures")
    if not isinstance(fixtures, list) or len(fixtures) != 3:
        raise WarmstartError("source warmstart evidence lacks three fixtures")

    output_dir.mkdir(parents=True, exist_ok=False)
    slot_dir = source_evidence.parent / "slots"
    store = WarmstartStore(slot_dir)
    port = _unused_port()
    command = build_warmstart_command(model, port, slot_dir, server=server)
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
            server_started = time.monotonic()
            _wait_for_health(process, port)
            model_ready_seconds = time.monotonic() - server_started
            tokenized = _json_request(
                f"http://127.0.0.1:{port}/tokenize",
                {"content": TOKEN_SEED_TEXT, "add_special": False, "parse_special": True},
            )
            seed = tokenized.get("tokens")
            if not isinstance(seed, list) or not all(isinstance(token, int) for token in seed):
                raise WarmstartError("resume token seed response is invalid")
            base = seed[:30_000]
            warm_key = WarmstartKey(
                model_sha256=str(manifest["sha256"]),
                quantization=str(expected["quantization"]),
                cache_type_k=PROFILE.cache_type_k,
                cache_type_v=PROFILE.cache_type_v,
                engine_commit=engine_commit,
                prompt_prefix_sha256=_token_sha256(base),
            )
            record = store.load(warm_key)
            if record.file_sha256 != source.get("slot_file_sha256"):
                raise WarmstartError("source evidence slot hash differs from validated store")
            suffix_response = _json_request(
                f"http://127.0.0.1:{port}/tokenize",
                {"content": FIXTURE_SUFFIXES[0], "add_special": False, "parse_special": True},
            )
            suffix_tokens = suffix_response.get("tokens")
            if not isinstance(suffix_tokens, list) or not all(isinstance(token, int) for token in suffix_tokens):
                raise WarmstartError("resume fixture suffix tokenization is invalid")
            client = SlotClient(f"http://127.0.0.1:{port}")
            client.erase(0)
            vm_before = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_before = parse_memory_pressure(_command_output(["memory_pressure"]))
            restore_started = time.monotonic()
            restore = client.restore(0, warm_key)
            restore_wall_seconds = time.monotonic() - restore_started
            continuation = _completion(port, base + suffix_tokens)
            vm_after = parse_vm_stat(_command_output(["vm_stat"]))
            pressure_after = parse_memory_pressure(_command_output(["memory_pressure"]))
            memory = {
                **_vm_delta(vm_before, vm_after),
                "free_percent_before": pressure_before,
                "free_percent_after": pressure_after,
            }
            first_fixture = fixtures[0]
            if (
                not isinstance(first_fixture, Mapping)
                or continuation["output_sha256"] != first_fixture.get("cold_output_sha256")
            ):
                raise WarmstartError("resume continuation differs from source cold fixture")
            report: dict[str, object] = {
                **source,
                "schema": 2,
                "source_evidence_sha256": _sha256(source_evidence),
                "command": command,
                "model_ready_seconds": model_ready_seconds,
                "restore": restore,
                "restore_wall_seconds": restore_wall_seconds,
                "continuation_recheck": continuation,
                "warm_ttft_kind": "restore_plus_strict_prefix_continuation",
                "warm_ttft_seconds": restore_wall_seconds + float(continuation["wall_seconds"]),
                "restored_tokens": restore["n_restored"],
                "memory": memory,
                "store_valid": True,
                "m0a_admitted_tokens": 0,
            }
            report["decision"] = evaluate_warmstart(report)
            _write_json_atomic(output_dir / "warmstart.json", report)
            return report
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def append_results(path: Path, report: Mapping[str, object], evidence_hash: str) -> None:
    marker = f"W1 evidence SHA-256: `{evidence_hash}`"
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    if marker in existing:
        raise WarmstartError("W1 evidence is already present in RESULTS")
    decision = report.get("decision")
    fixtures = report.get("fixtures")
    if not isinstance(decision, Mapping) or not isinstance(fixtures, list):
        raise WarmstartError("W1 report shape is invalid")
    parity_count = sum(
        fixture["cold_output_sha256"] == fixture["warm_output_sha256"]
        for fixture in fixtures
    )
    lines = [
        "\n## Track W1 repo warmstart\n",
        f"- {marker}",
        f"- Production f16/FA/MTP4, 30K exact prompt prefix, model ready in {report['model_ready_seconds']:.3f} s",
        f"- Cold base request: {report['cold_base']['wall_seconds']:.3f} s; prompt {report['cold_base']['prompt_ms'] / 1000.0:.3f} s at {report['cold_base']['prompt_tokens_per_second']:.2f} tok/s",
        f"- Slot: {report['saved_tokens']:,} tokens, {report['slot_file_bytes']:,} bytes; save {report['save_wall_seconds']:.3f} s; restore {report['restore_wall_seconds']:.3f} s",
        f"- Warm restore+TTFT: {report['warm_ttft_seconds']:.3f} s (gate <2.000 s)",
        f"- Fixture parity: {parity_count}/{len(fixtures)}; store identity/hash valid: `{str(report['store_valid']).lower()}`",
        f"- Memory reclaim: {decision['decode_reclaim_bytes']:,} bytes; 0 M0a-admitted tokens",
        f"- W1 decision: **{'PASS' if decision['passed'] else 'FAIL'}**; gateway gate **{decision['gateway_gate']}**",
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
    parser.add_argument("--resume-evidence", type=Path)
    args = parser.parse_args()
    try:
        if args.resume_evidence is None:
            report = run_warmstart(args.model, args.output_dir, server=args.server)
        else:
            report = run_warmstart_resume(
                args.model, args.output_dir, args.resume_evidence, server=args.server
            )
        evidence = args.output_dir / "warmstart.json"
        append_results(args.results, report, _sha256(evidence))
    except (WarmstartError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"warmstart failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["decision"]["passed"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
