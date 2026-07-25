"""Load repository-grounded coding tasks with strict path and command policy."""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
from typing import Mapping


ALLOWED_RESPONSE_KINDS = {"file", "json"}
ALLOWED_VERIFIER_PREFIX = ("python3", "-m", "unittest")


class WorkloadError(ValueError):
    """Raised when a coding workload is unsafe or malformed."""


@dataclass(frozen=True)
class WorkloadTask:
    task_id: str
    fixture_dir: Path
    prompt: str
    response_kind: str
    output_path: Path
    verifier: tuple[str, ...]


def _mapping(value: object, name: str) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise WorkloadError(f"{name} must be an object")
    return value


def _relative_path(value: object, name: str) -> Path:
    if not isinstance(value, str) or not value:
        raise WorkloadError(f"{name} must be a non-empty string")
    path = Path(value)
    if path.is_absolute() or ".." in path.parts:
        raise WorkloadError(f"{name} must stay within the workload")
    return path


def load_workload(path: Path) -> tuple[WorkloadTask, ...]:
    """Read and validate one immutable workload manifest."""

    if path.is_symlink() or not path.is_file():
        raise WorkloadError("workload manifest must be a regular non-symlink file")
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise WorkloadError(f"cannot read workload manifest: {error}") from error
    root = path.parent.resolve()
    manifest = _mapping(document, "manifest")
    if manifest.get("schema") != 1:
        raise WorkloadError("unsupported workload schema")
    raw_tasks = manifest.get("tasks")
    if not isinstance(raw_tasks, list) or not raw_tasks:
        raise WorkloadError("workload tasks must be a non-empty list")

    tasks: list[WorkloadTask] = []
    identifiers: set[str] = set()
    for index, raw_task in enumerate(raw_tasks):
        task = _mapping(raw_task, f"task[{index}]")
        task_id = task.get("id")
        if not isinstance(task_id, str) or not task_id.strip():
            raise WorkloadError(f"task[{index}] has no id")
        if task_id in identifiers:
            raise WorkloadError(f"duplicate task id: {task_id}")
        identifiers.add(task_id)

        fixture_relative = _relative_path(task.get("fixture_dir"), "fixture_dir")
        fixture_dir = (root / fixture_relative).resolve()
        if not fixture_dir.is_relative_to(root) or not fixture_dir.is_dir():
            raise WorkloadError(f"fixture directory is missing or unsafe: {fixture_relative}")

        prompt = task.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip():
            raise WorkloadError(f"task {task_id} has an empty prompt")

        response = _mapping(task.get("response"), "response")
        kind = response.get("kind")
        if kind not in ALLOWED_RESPONSE_KINDS:
            raise WorkloadError(f"task {task_id} has unsupported response kind")
        output_path = _relative_path(response.get("path"), "response.path")

        verifier_raw = task.get("verifier")
        if (
            not isinstance(verifier_raw, list)
            or not all(isinstance(part, str) and part for part in verifier_raw)
            or tuple(verifier_raw[:3]) != ALLOWED_VERIFIER_PREFIX
        ):
            raise WorkloadError(f"task {task_id} has an unapproved verifier")
        tasks.append(
            WorkloadTask(
                task_id=task_id,
                fixture_dir=fixture_dir,
                prompt=prompt,
                response_kind=str(kind),
                output_path=output_path,
                verifier=tuple(verifier_raw),
            )
        )
    return tuple(tasks)
