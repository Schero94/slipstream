"""Run the frozen four-episode verifier against an isolated runtime profile."""

from __future__ import annotations

import argparse
from dataclasses import asdict
import json
from pathlib import Path
import subprocess
from typing import Mapping
from uuid import uuid4

from bench.m0a.qualify_model import (
    QUALIFICATION_PROFILES,
    QualificationProfile,
    _read_manifest,
    profile_environment,
    qualification_server_command,
)
from bench.m0a.run_agentic_episodes import DEFAULT_MANIFEST, run_episodes
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    SmokeError,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)


class ProfileVerifierError(RuntimeError):
    """Raised when the isolated verifier cannot produce complete evidence."""


def evaluate_verifier_quality(
    report: Mapping[str, object], *, expected_episodes: int = 4
) -> dict[str, object]:
    episodes = report.get("episodes")
    if not isinstance(episodes, list):
        return {"passed": False, "reasons": ["invalid-episodes"], "episode_passes": 0}
    reasons: list[str] = []
    identifiers = [
        episode.get("episode_id")
        for episode in episodes
        if isinstance(episode, Mapping)
    ]
    if len(episodes) != expected_episodes:
        reasons.append("episode-count")
    if len(identifiers) != len(episodes) or len(set(identifiers)) != len(identifiers):
        reasons.append("episode-identity")
    passes = 0
    for episode in episodes:
        if not isinstance(episode, Mapping):
            reasons.append("invalid-episode")
            continue
        if episode.get("passed") is True and episode.get("hidden_verifier_exit_code") == 0:
            passes += 1
        else:
            reasons.append(f"verifier@{episode.get('episode_id', 'unknown')}")
    return {
        "passed": not reasons,
        "reasons": reasons,
        "episode_passes": passes,
        "episode_count": len(episodes),
    }


def run_profile_verifier(
    model: Path,
    output_dir: Path,
    *,
    profile: QualificationProfile,
    manifest: Path = DEFAULT_MANIFEST,
    server: Path = DEFAULT_SERVER,
) -> dict[str, object]:
    model_manifest = _read_manifest(model)
    output_dir.mkdir(parents=True, exist_ok=False)
    port = _unused_port()
    command = qualification_server_command(
        model,
        port,
        server=server,
        profile=profile,
    )
    session_id = uuid4()
    with (output_dir / "server.stdout.log").open("wb") as stdout, (
        output_dir / "server.stderr.log"
    ).open("wb") as stderr:
        process = subprocess.Popen(
            command,
            env=profile_environment(profile),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            _wait_for_health(process, port)
            episode_report = run_episodes(
                manifest,
                output_dir / "episodes",
                base_url=f"http://127.0.0.1:{port}/v1",
                model="peregrine-qualification",
                session_id=session_id,
                server_pid=process.pid,
            )
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
    episodes = episode_report.get("episodes")
    if not isinstance(episodes, list):
        raise ProfileVerifierError("episode report is missing its episode list")
    quality = evaluate_verifier_quality(
        episode_report,
        expected_episodes=len(episodes),
    )
    report: dict[str, object] = {
        "schema": 1,
        "session_id": str(session_id),
        "model_sha256": model_manifest["sha256"],
        "manifest": str(manifest.resolve()),
        "runtime_profile": asdict(profile),
        "command": command,
        "decoded_tokens": episode_report["decoded_tokens"],
        "draft_generated": episode_report["draft_generated"],
        "draft_accepted": episode_report["draft_accepted"],
        "peak_rss_kb": episode_report["peak_rss_kb"],
        "performance_decision": episode_report["decision"],
        "quality_decision": quality,
        "episodes": episode_report["episodes"],
        "m0a_admission_eligible": False,
    }
    _write_json_atomic(output_dir / "profile-verifier.json", report)
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--profile", required=True, choices=tuple(QUALIFICATION_PROFILES))
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = run_profile_verifier(
            args.model,
            args.output_dir,
            profile=QUALIFICATION_PROFILES[args.profile],
            manifest=args.manifest,
            server=args.server,
        )
    except (OSError, ProfileVerifierError, SmokeError, ValueError) as error:
        print(f"profile verifier failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["quality_decision"]["passed"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
