"""Track V2: self-configuration wizard backend for the Peregrine plugin.

Turns a fresh install into a working local coding agent, data-driven and
model-agnostic (Plan v3 Section 6.1). All decisions are pure and unit-tested;
the VS Code wizard is a thin shell that renders these steps and confirms each
write. Nothing here downloads a model or touches credentials — model download
is a separate confirmation-gated, disk-gated step, and CLI detection only checks
availability on PATH.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
from pathlib import Path
from typing import Any, Callable

GIB = 1024 * 1024 * 1024

# The one confirmed, qualified Stufe-1 model. A manifest, not hardcoded behavior:
# every model fact is data so the underlying GGUF can be swapped by config alone.
STUFE1_PROFILE: dict[str, Any] = {
    "alias": "peregrine-qualification",
    "gguf_filename": "Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf",
    "expected_bytes": 22_853_663_008,
    "sha256": "55983c5a75a1ab969824077b3bb3de4146e82a9234072b48ad4e8f92ad3fe9f1",
    "source": "unsloth/Qwen3.6-35B-A3B-MTP-GGUF",
    "min_ram_bytes": 34 * GIB,
    "cache_path": "/Users/schero/.cache/peregrine/models/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf",
}

# RAM classes with a qualified profile. Bands without a qualified model return
# None honestly rather than inventing a weaker fallback (Q3-Coder was rejected).
MODEL_PROFILES: dict[str, dict[str, Any] | None] = {
    "36GB": STUFE1_PROFILE,
    "large": STUFE1_PROFILE,
    "small": None,
}


class WizardError(Exception):
    """Raised for unsafe/unconfirmed onboarding operations."""


def classify_ram(ram_bytes: int) -> str:
    if ram_bytes >= 48 * GIB:
        return "large"
    if ram_bytes >= 34 * GIB:
        return "36GB"
    return "small"


def recommend_model(ram_bytes: int) -> tuple[dict[str, Any] | None, str]:
    ram_class = classify_ram(ram_bytes)
    profile = MODEL_PROFILES.get(ram_class)
    if profile is None:
        return None, (
            f"no qualified model for RAM class {ram_class!r}; only the 36 GB-class "
            "Stufe-1 model is qualified. A smaller-Mac fallback is not yet proven."
        )
    return profile, f"recommended Stufe-1 model for RAM class {ram_class!r}"


def detect_ram_bytes() -> int:
    try:
        return os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
    except (ValueError, OSError):  # pragma: no cover - platform fallback
        return 0


def detect_cli(name: str, *, which: Callable[[str], str | None] = shutil.which) -> dict[str, Any]:
    """Availability-only CLI probe. Never inspects login state or credentials."""
    path = which(name)
    return {"name": name, "available": path is not None, "path": path}


def _escalation_toml() -> str:
    return (
        "# Peregrine escalation policy. Cloud boost uses the user's own official\n"
        "# CLIs (claude, codex) and requires per-incident consent.\n"
        '# mode: "ask" (confirm each escalation) | "off" (never) \n'
        'mode = "ask"\n'
    )


def _lessons_md() -> str:
    return (
        "# Peregrine lessons\n\n"
        "Cause-plus-lesson notes from resolved hard cases. Included in the\n"
        "Stufe-1 system context. Appended by the escalation lessons flow.\n"
    )


def plan_repo_onboarding(repo_root: Path) -> list[dict[str, Any]]:
    base = repo_root / ".peregrine"
    return [
        {
            "kind": "file",
            "target": str(base / "escalation.toml"),
            "content": _escalation_toml(),
            "preserve_existing": True,
        },
        {
            "kind": "file",
            "target": str(base / "lessons.md"),
            "content": _lessons_md(),
            "preserve_existing": True,
        },
        {
            "kind": "dir",
            "target": str(base / "snapshots"),
            "content": None,
            "preserve_existing": True,
        },
    ]


def apply_repo_onboarding(plan: list[dict[str, Any]], *, confirm: bool) -> list[str]:
    if not confirm:
        raise WizardError("repo onboarding requires explicit confirmation")
    created: list[str] = []
    for step in plan:
        target = Path(step["target"])
        parent = target.parent
        # Refuse to write into or through a symlinked .peregrine directory.
        for ancestor in (parent, target):
            if ancestor.is_symlink():
                raise WizardError(f"refusing to write through a symlink: {ancestor}")
        parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        if step["kind"] == "dir":
            target.mkdir(parents=True, exist_ok=True, mode=0o700)
            created.append(str(target))
            continue
        if target.exists():
            if target.is_symlink() or not target.is_file():
                raise WizardError(f"refusing to overwrite non-regular file: {target}")
            if step.get("preserve_existing"):
                created.append(str(target))
                continue
        target.write_text(step["content"], encoding="utf-8")
        target.chmod(0o600)
        created.append(str(target))
    return created


def build_wizard_plan(
    *,
    ram_bytes: int,
    repo_root: Path,
    which: Callable[[str], str | None] = shutil.which,
    model_present: bool,
) -> dict[str, Any]:
    profile, reason = recommend_model(ram_bytes)
    cli = {name: detect_cli(name, which=which) for name in ("claude", "codex")}
    cloud_available = any(entry["available"] for entry in cli.values())
    return {
        "ram_bytes": ram_bytes,
        "ram_class": classify_ram(ram_bytes),
        "model": profile,
        "model_reason": reason,
        "model_present": model_present,
        "cli": cli,
        "cloud_available": cloud_available,
        "onboarding": plan_repo_onboarding(repo_root),
        "notes": [
            "Model download is a separate disk-gated, confirmation-gated step.",
            "Set iogpu.wired_limit_mb via tools/wired-limit.sh (human runs it).",
            "Context strategy: bounded 32K active window + retrieval + W1 snapshots.",
        ],
    }


def _model_present(profile: dict[str, Any] | None) -> bool:
    if not profile:
        return False
    path = Path(profile.get("cache_path", ""))
    try:
        return path.is_file() and path.stat().st_size == profile["expected_bytes"]
    except OSError:
        return False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    plan_parser = sub.add_parser("plan")
    plan_parser.add_argument("--repo-root", type=Path, default=Path.cwd())

    onboard_parser = sub.add_parser("onboard")
    onboard_parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    onboard_parser.add_argument("--confirm", action="store_true")

    args = parser.parse_args(argv)
    try:
        if args.command == "plan":
            ram = detect_ram_bytes()
            profile, _ = recommend_model(ram)
            plan = build_wizard_plan(
                ram_bytes=ram,
                repo_root=args.repo_root,
                model_present=_model_present(profile),
            )
            print(json.dumps(plan, indent=2, sort_keys=True))
            return 0
        if args.command == "onboard":
            plan = plan_repo_onboarding(args.repo_root)
            created = apply_repo_onboarding(plan, confirm=args.confirm)
            print(json.dumps({"created": created}, indent=2))
            return 0
    except WizardError as error:
        print(f"wizard error: {error}")
        return 2
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
