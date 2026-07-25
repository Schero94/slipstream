"""Load bounded agentic coding episodes with a fail-closed safety policy."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
from typing import Mapping


EPISODE_ID = re.compile(r"[a-z0-9][a-z0-9-]*")
VERIFIER_PREFIX = ("python3", "-m", "unittest")
LIMIT_NAMES = (
    "max_steps",
    "max_output_tokens",
    "wall_timeout_seconds",
    "max_test_runs",
)


class EpisodeError(ValueError):
    """Raised when an agentic episode is malformed or unsafe."""


@dataclass(frozen=True)
class Episode:
    episode_id: str
    category: str
    fixture_dir: Path
    task: str
    task_sha256: str
    writable_paths: tuple[Path, ...]
    visible_verifier: tuple[str, ...]
    hidden_verifier: tuple[str, ...]
    max_steps: int
    max_output_tokens: int
    wall_timeout_seconds: int
    max_test_runs: int


def _mapping(value: object, name: str) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise EpisodeError(f"{name} must be an object")
    return value


def _relative(value: object, name: str) -> Path:
    if not isinstance(value, str) or not value:
        raise EpisodeError(f"{name} must be a non-empty relative path")
    path = Path(value)
    if path == Path(".") or path.is_absolute() or ".." in path.parts:
        raise EpisodeError(f"{name} must stay inside the episode")
    return path


def _verifier(value: object, name: str) -> tuple[str, ...]:
    if (
        not isinstance(value, list)
        or not all(isinstance(part, str) and part for part in value)
        or tuple(value[:3]) != VERIFIER_PREFIX
    ):
        raise EpisodeError(f"{name} is not an approved verifier")
    for argument in value[3:]:
        argument_path = Path(argument)
        if argument_path.is_absolute() or ".." in argument_path.parts:
            raise EpisodeError(f"{name} contains an unsafe argument")
    return tuple(value)


def _positive_limit(limits: Mapping[str, object], name: str) -> int:
    value = limits.get(name)
    if type(value) is not int or value <= 0:
        raise EpisodeError(f"{name} must be a positive integer")
    return value


def _reject_fixture_symlinks(fixture: Path) -> None:
    if fixture.is_symlink():
        raise EpisodeError(f"fixture is a symlink: {fixture}")
    for path in fixture.rglob("*"):
        if path.is_symlink():
            raise EpisodeError(f"fixture contains a symlink: {path}")


def load_episodes(path: Path) -> tuple[Episode, ...]:
    """Read an immutable episode manifest and validate every execution boundary."""

    if path.is_symlink() or not path.is_file():
        raise EpisodeError("episode manifest must be a regular non-symlink file")
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EpisodeError(f"cannot read episode manifest: {error}") from error
    manifest = _mapping(document, "manifest")
    if manifest.get("schema") != 1:
        raise EpisodeError("unsupported episode schema")
    raw_episodes = manifest.get("episodes")
    if not isinstance(raw_episodes, list) or not raw_episodes:
        raise EpisodeError("episodes must be a non-empty list")

    root = path.parent.resolve()
    identifiers: set[str] = set()
    episodes: list[Episode] = []
    for index, raw in enumerate(raw_episodes):
        item = _mapping(raw, f"episode[{index}]")
        episode_id = item.get("id")
        if not isinstance(episode_id, str) or EPISODE_ID.fullmatch(episode_id) is None:
            raise EpisodeError(f"episode[{index}] has an invalid id")
        if episode_id in identifiers:
            raise EpisodeError(f"duplicate episode id: {episode_id}")
        identifiers.add(episode_id)

        category = item.get("category")
        if not isinstance(category, str) or not category.strip():
            raise EpisodeError(f"episode {episode_id} has no category")
        task = item.get("task")
        if not isinstance(task, str) or not task.strip():
            raise EpisodeError(f"episode {episode_id} has an empty task")

        fixture_relative = _relative(item.get("fixture_dir"), "fixture_dir")
        fixture_unresolved = root / fixture_relative
        if fixture_unresolved.is_symlink():
            raise EpisodeError(f"fixture is a symlink: {fixture_relative}")
        fixture = fixture_unresolved.resolve()
        if not fixture.is_relative_to(root) or not fixture.is_dir():
            raise EpisodeError(f"fixture is missing or unsafe: {fixture_relative}")
        _reject_fixture_symlinks(fixture)

        raw_writable = item.get("writable_paths")
        if not isinstance(raw_writable, list) or not raw_writable:
            raise EpisodeError(f"episode {episode_id} has no writable paths")
        writable = tuple(_relative(value, "writable_path") for value in raw_writable)
        if len(set(writable)) != len(writable):
            raise EpisodeError(f"episode {episode_id} has duplicate writable paths")

        visible = _verifier(item.get("visible_verifier"), "visible_verifier")
        hidden = _verifier(item.get("hidden_verifier"), "hidden_verifier")
        if visible == hidden:
            raise EpisodeError("visible and hidden verifiers must differ")
        limits = _mapping(item.get("limits"), "limits")
        values = {name: _positive_limit(limits, name) for name in LIMIT_NAMES}
        episodes.append(
            Episode(
                episode_id=episode_id,
                category=category.strip(),
                fixture_dir=fixture,
                task=task.strip(),
                task_sha256=hashlib.sha256(task.strip().encode("utf-8")).hexdigest(),
                writable_paths=writable,
                visible_verifier=visible,
                hidden_verifier=hidden,
                **values,
            )
        )
    return tuple(episodes)
