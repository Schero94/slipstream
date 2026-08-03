"""Every component the runtime manifest marks required must exist before bundling.

test_runtime_manifest.py validates the manifest's *shape*. Nothing validated that the
files it declares are actually staged, so a 0.3.4 candidate bundle was built without
`omlx-pgrn/uv` — declared `required: true` — and looked healthy: the manifest test was
green, codesign --deep --strict passed, and the app launched. Only the 22 MB drop in
.dmg size against the previous release gave it away. Without uv the MLX runtime
bootstrap cannot install anything on a user's machine.

This test fails the build instead of shipping the hole.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest


RESOURCES = Path(__file__).resolve().parents[1] / "src-tauri" / "resources"
MANIFEST = RESOURCES / "runtime-manifest.json"


def _manifest() -> dict:
    return json.loads(MANIFEST.read_text(encoding="utf-8"))


def test_manifest_is_present() -> None:
    assert MANIFEST.is_file(), f"missing {MANIFEST}"


def test_every_required_component_is_staged() -> None:
    missing = []
    for name, spec in _manifest()["components"].items():
        if not spec.get("required"):
            continue
        target = RESOURCES / spec["path"]
        if not target.exists():
            missing.append(f"{name} -> {spec['path']}")
    assert not missing, (
        "required runtime components are not staged; the bundle would ship "
        f"incomplete: {missing}. For uv run app/scripts/stage_uv_runtime.sh."
    )


def test_executable_components_are_executable() -> None:
    not_executable = []
    for name, spec in _manifest()["components"].items():
        if not spec.get("executable"):
            continue
        target = RESOURCES / spec["path"]
        if target.exists() and not os.access(target, os.X_OK):
            not_executable.append(f"{name} -> {spec['path']}")
    assert not not_executable, f"declared executable but not +x: {not_executable}"


def test_staged_uv_matches_the_pinned_version() -> None:
    """The staging script pins uv; a different build of it is a silent drift."""
    uv = RESOURCES / "omlx-pgrn" / "uv"
    if not uv.exists():
        pytest.fail("omlx-pgrn/uv is not staged; run app/scripts/stage_uv_runtime.sh")
    expected = "uv 0.11.10"
    script = (Path(__file__).resolve().parents[0] / "stage_uv_runtime.sh").read_text(encoding="utf-8")
    assert f'EXPECTED="{expected}"' in script, (
        "stage_uv_runtime.sh no longer pins " + expected + "; update this test deliberately"
    )
