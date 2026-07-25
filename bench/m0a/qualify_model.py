"""Fail-closed 4K/32K/64K admission gate for 36 GB coding models."""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import threading
import time
from typing import Mapping, Sequence

from bench.m0a.admission_policy import evaluate_qualification_policy
from bench.m0a.coding_telemetry import (
    MIN_DECODE_TOKENS_PER_SECOND,
    TARGET_DECODE_TOKENS_PER_SECOND,
)
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    PGR_ENV_NAMES,
    SmokeError,
    _json_request,
    _monitor_rss,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RESULTS = REPO_ROOT / "bench" / "RESULTS.md"
CONTEXT_POINTS = (4_000, 32_000, 64_000)
DECODE_TOKENS = 128
MAX_PEAK_RSS_KB = 31_000_000
TOKEN_SEED_TEXT = "x " * 70_000
SUPPORTED_CACHE_TYPES = frozenset(
    {"f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1"}
)


@dataclass(frozen=True)
class QualificationProfile:
    """Immutable runtime settings for a numerics-isolated qualification."""

    name: str
    flash_attention: str
    cache_type_k: str
    cache_type_v: str
    speculation: str
    draft_tokens: int | None
    spec_pin: bool = False
    adaptive_draft: bool = False

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("profile name must not be empty")
        if self.flash_attention not in {"on", "off", "auto"}:
            raise ValueError("unsupported flash attention mode")
        for cache_type in (self.cache_type_k, self.cache_type_v):
            if cache_type not in SUPPORTED_CACHE_TYPES:
                raise ValueError(f"unsupported cache type: {cache_type}")
        if self.speculation not in {"none", "draft-mtp"}:
            raise ValueError(f"unsupported qualification speculation mode: {self.speculation}")
        if self.speculation == "none" and self.draft_tokens is not None:
            raise ValueError("draft tokens require draft-mtp speculation")
        if self.speculation == "draft-mtp" and (
            self.draft_tokens is None or self.draft_tokens <= 0
        ):
            raise ValueError("draft-mtp requires a positive draft token count")
        if self.spec_pin and self.speculation != "draft-mtp":
            raise ValueError("SPEC_PIN requires draft-mtp speculation")
        if self.adaptive_draft and (
            self.speculation != "draft-mtp"
            or self.draft_tokens is None
            or self.draft_tokens < 12
        ):
            raise ValueError("adaptive draft requires draft-mtp with a maximum of at least 12")


QUALIFICATION_PROFILES = {
    profile.name: profile
    for profile in (
        QualificationProfile(
            name="baseline-f16-fa-mtp4",
            flash_attention="on",
            cache_type_k="f16",
            cache_type_v="f16",
            speculation="draft-mtp",
            draft_tokens=4,
        ),
        QualificationProfile(
            name="kv-q8_0-fa-mtp4",
            flash_attention="on",
            cache_type_k="q8_0",
            cache_type_v="q8_0",
            speculation="draft-mtp",
            draft_tokens=4,
        ),
        QualificationProfile(
            name="baseline-f16-fa-mtp8",
            flash_attention="on",
            cache_type_k="f16",
            cache_type_v="f16",
            speculation="draft-mtp",
            draft_tokens=8,
        ),
        QualificationProfile(
            name="spec-pin-f16-fa-mtp8",
            flash_attention="on",
            cache_type_k="f16",
            cache_type_v="f16",
            speculation="draft-mtp",
            draft_tokens=8,
            spec_pin=True,
        ),
        QualificationProfile(
            name="adaptive-f16-fa-mtp12",
            flash_attention="on",
            cache_type_k="f16",
            cache_type_v="f16",
            speculation="draft-mtp",
            draft_tokens=12,
            adaptive_draft=True,
        ),
    )
}


class QualificationError(RuntimeError):
    """Raised when the qualification run cannot produce trustworthy evidence."""


def qualification_server_command(
    model: Path,
    port: int,
    *,
    server: Path = DEFAULT_SERVER,
    speculation: str = "none",
    draft_tokens: int | None = None,
    profile: QualificationProfile | None = None,
) -> list[str]:
    if profile is not None:
        if speculation != "none" or draft_tokens is not None:
            raise ValueError("named profile cannot be mixed with ad-hoc runtime settings")
        speculation = profile.speculation
        draft_tokens = profile.draft_tokens
    if speculation not in {"none", "draft-mtp"}:
        raise ValueError(f"unsupported qualification speculation mode: {speculation}")
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
        str(port),
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
        "0",
        "--alias",
        "peregrine-qualification",
    ]
    if profile is not None:
        command.extend(
            [
                "--flash-attn",
                profile.flash_attention,
                "--cache-type-k",
                profile.cache_type_k,
                "--cache-type-v",
                profile.cache_type_v,
            ]
        )
    if draft_tokens is not None:
        command.extend(["--spec-draft-n-max", str(draft_tokens)])
    if profile is not None and profile.adaptive_draft:
        command.append("--spec-draft-adaptive")
    return command


def production_environment(base: Mapping[str, str] | None = None) -> dict[str, str]:
    environment = dict(os.environ if base is None else base)
    for name in PGR_ENV_NAMES:
        environment.pop(name, None)
    environment.pop("SPEC_PIN", None)
    return environment


def profile_environment(
    profile: QualificationProfile | None,
    base: Mapping[str, str] | None = None,
) -> dict[str, str]:
    environment = production_environment(base)
    if profile is not None and profile.spec_pin:
        environment["SPEC_PIN"] = "1"
    return environment


def evaluate_qualification(
    points: Sequence[Mapping[str, object]],
    *,
    contexts: Sequence[int] = CONTEXT_POINTS,
    decode_tokens: int = DECODE_TOKENS,
    minimum_speed: float = MIN_DECODE_TOKENS_PER_SECOND,
    maximum_rss_kb: int = MAX_PEAK_RSS_KB,
) -> dict[str, object]:
    return evaluate_qualification_policy(
        points,
        contexts=contexts,
        decode_tokens=decode_tokens,
        minimum_mean=minimum_speed,
        maximum_rss_kb=maximum_rss_kb,
    )


def _read_manifest(model: Path) -> dict[str, object]:
    manifest_path = model.parent / "manifest.json"
    if model.is_symlink() or not model.is_file():
        raise QualificationError("model must be a regular non-symlink file")
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise QualificationError(f"cannot read verified model manifest: {error}") from error
    if not isinstance(manifest, dict) or not isinstance(manifest.get("sha256"), str):
        raise QualificationError("verified model manifest is invalid")
    return manifest


def _run_point(
    process: subprocess.Popen[bytes],
    port: int,
    prompt_tokens: list[int],
    *,
    cache_prompt: bool = False,
    return_tokens: bool = False,
) -> dict[str, object]:
    maximum = [0]
    stop_monitor = threading.Event()
    monitor = threading.Thread(
        target=_monitor_rss,
        args=(process, stop_monitor, maximum),
        daemon=True,
    )
    monitor.start()
    started = time.monotonic()
    try:
        completion = _json_request(
            f"http://127.0.0.1:{port}/completion",
            {
                "prompt": prompt_tokens,
                "n_predict": DECODE_TOKENS,
                "ignore_eos": True,
                "temperature": 0,
                "seed": 42,
                "stream": False,
                "cache_prompt": cache_prompt,
                "return_tokens": return_tokens,
            },
            timeout=3600,
        )
    finally:
        stop_monitor.set()
        monitor.join(timeout=2)
    timings = completion.get("timings")
    if not isinstance(timings, dict):
        raise QualificationError("completion response has no timings")
    speed = timings.get("predicted_per_second")
    decoded = timings.get("predicted_n")
    prompt_evaluated = timings.get("prompt_n")
    if (
        not isinstance(speed, (int, float))
        or not isinstance(decoded, int)
        or not isinstance(prompt_evaluated, int)
    ):
        raise QualificationError("completion response has invalid decode timings")
    result: dict[str, object] = {
        "context_tokens": len(prompt_tokens),
        "decoded_tokens": decoded,
        "decode_tokens_per_second": float(speed),
        "prompt_tokens_per_second": timings.get("prompt_per_second"),
        "prompt_ms": timings.get("prompt_ms"),
        "prompt_tokens_evaluated": prompt_evaluated,
        "peak_rss_kb": maximum[0],
        "stop_type": completion.get("stop_type"),
        "wall_seconds": time.monotonic() - started,
    }
    if return_tokens:
        output_tokens = completion.get("tokens")
        if (
            not isinstance(output_tokens, list)
            or len(output_tokens) != decoded
            or not all(isinstance(token, int) for token in output_tokens)
        ):
            raise QualificationError("completion response has invalid output tokens")
        result["output_tokens"] = output_tokens
    return result


def _append_result(results: Path, report: Mapping[str, object]) -> None:
    decision = report["decision"]
    points = report["points"]
    speculation = report["speculation"]
    if (
        not isinstance(decision, dict)
        or not isinstance(points, list)
        or not isinstance(speculation, dict)
    ):
        raise QualificationError("qualification report shape is invalid")
    timestamp = datetime.now(timezone.utc).isoformat()
    status = "PASS" if decision["passed"] else "FAIL"
    lines = [
        f"\n## Long-context model qualification — {timestamp}\n",
        f"- Model SHA-256: `{report['model_sha256']}`",
        "- Runtime: Metal, one slot, no routing instrumentation, "
        f"speculation `{speculation['type']}`, draft tokens "
        f"`{speculation['draft_tokens']}`",
        "- Decode request: 128 tokens, temperature 0, seed 42, prompt cache disabled",
    ]
    runtime_profile = report.get("runtime_profile")
    if isinstance(runtime_profile, Mapping):
        lines.append(
            f"- Runtime profile: `{runtime_profile['name']}`; Flash-Attention "
            f"`{runtime_profile['flash_attention']}`; K/V cache "
            f"`{runtime_profile['cache_type_k']}`/`{runtime_profile['cache_type_v']}`"
        )
    for point in points:
        lines.append(
            f"- {point['context_tokens']:,} context tokens: "
            f"{point['decode_tokens_per_second']:.6f} tok/s, "
            f"{point['peak_rss_kb']:,} KiB peak RSS"
        )
    lines.extend(
        [
            "- Speed target: 25.0 tok/s; admission floor: 24.0 tok/s mean; "
            "64K profile floor: 20.0 tok/s; at most 31,000,000 KiB peak RSS",
            f"- Result: **{status}** — reasons: {', '.join(decision['reasons']) or 'none'}",
            "",
        ]
    )
    with results.open("a", encoding="utf-8") as stream:
        stream.write("\n".join(lines))
        stream.flush()
        os.fsync(stream.fileno())


def run_qualification(
    model: Path,
    output_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
    results: Path = DEFAULT_RESULTS,
    speculation: str = "none",
    draft_tokens: int | None = None,
    profile: QualificationProfile | None = None,
) -> dict[str, object]:
    manifest = _read_manifest(model)
    output_dir.mkdir(parents=True, exist_ok=False)
    port = _unused_port()
    command = qualification_server_command(
        model,
        port,
        server=server,
        speculation=speculation,
        draft_tokens=draft_tokens,
        profile=profile,
    )
    stdout_path = output_dir / "server.stdout.log"
    stderr_path = output_dir / "server.stderr.log"
    points: list[dict[str, object]] = []
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            env=profile_environment(profile),
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
            if not isinstance(seed, list) or not all(isinstance(token, int) for token in seed):
                raise QualificationError("token seed response is invalid")
            if len(seed) < max(CONTEXT_POINTS):
                raise QualificationError("token seed is shorter than the qualification matrix")
            for context in CONTEXT_POINTS:
                points.append(_run_point(process, port, seed[:context]))
        except SmokeError as error:
            raise QualificationError(str(error)) from error
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
    decision = evaluate_qualification(points)
    report: dict[str, object] = {
        "schema": 2,
        "model_path": str(model.resolve()),
        "model_sha256": manifest["sha256"],
        "speculation": {
            "type": profile.speculation if profile is not None else speculation,
            "draft_tokens": profile.draft_tokens if profile is not None else draft_tokens,
        },
        "runtime_profile": asdict(profile) if profile is not None else None,
        "command": command,
        "points": points,
        "decision": decision,
    }
    _write_json_atomic(output_dir / "qualification.json", report)
    _append_result(results, report)
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    parser.add_argument(
        "--speculation",
        choices=("none", "draft-mtp"),
        default="none",
    )
    parser.add_argument("--draft-tokens", type=int)
    parser.add_argument("--profile", choices=tuple(QUALIFICATION_PROFILES))
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = run_qualification(
            args.model,
            args.output_dir,
            server=args.server,
            results=args.results,
            speculation=args.speculation,
            draft_tokens=args.draft_tokens,
            profile=QUALIFICATION_PROFILES.get(args.profile),
        )
    except (QualificationError, OSError, ValueError) as error:
        print(f"qualification failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["decision"]["passed"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
