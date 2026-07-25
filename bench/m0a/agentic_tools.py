"""Filesystem and verifier tools for one isolated coding-agent workspace."""

from __future__ import annotations

from pathlib import Path
import os
import re
import subprocess
from tempfile import NamedTemporaryFile

from bench.m0a.agentic_episode import Episode


PATCH_TARGET = re.compile(r"^(?:---|\+\+\+) [ab]/([^\t\n]+)", re.MULTILINE)
HUNK_HEADER = re.compile(
    r"^@@ -(?P<old_start>\d+)(?:,(?P<old_claim>\d+))? "
    r"\+(?P<new_start>\d+)(?:,(?P<new_claim>\d+))? "
    r"@@(?P<suffix>.*?)(?P<newline>\r?\n)?$"
)
HIDDEN_ROOTS = {"hidden_tests"}


class ToolError(RuntimeError):
    """Raised when an agent tool request is unsafe or cannot be completed."""


def _recount_unified_diff(patch_text: str) -> str:
    """Correct model-generated hunk counts without changing patch content."""

    lines = patch_text.splitlines(keepends=True)
    index = 0
    while index < len(lines):
        match = HUNK_HEADER.match(lines[index])
        if match is None:
            index += 1
            continue
        end = index + 1
        old_count = 0
        new_count = 0
        while end < len(lines):
            line = lines[end]
            if HUNK_HEADER.match(line) is not None:
                break
            if (
                line.startswith("--- a/")
                and end + 1 < len(lines)
                and lines[end + 1].startswith("+++ b/")
            ):
                break
            if line.startswith("\\ No newline at end of file"):
                end += 1
                continue
            if not line or line[0] not in " +-":
                break
            old_count += line[0] in " -"
            new_count += line[0] in " +"
            end += 1
        old_claim = int(match.group("old_claim") or 1)
        if old_claim > max(old_count + 3, old_count * 2):
            raise ToolError("patch hunk omits too many declared old lines")
        newline = match.group("newline") or ""
        lines[index] = (
            f"@@ -{match.group('old_start')},{old_count} "
            f"+{match.group('new_start')},{new_count} @@"
            f"{match.group('suffix')}{newline}"
        )
        index = end
    return "".join(lines)


class AgenticSandbox:
    def __init__(
        self,
        episode: Episode,
        root: Path,
        *,
        max_output_chars: int = 16_000,
    ) -> None:
        self.episode = episode
        self.root = root.resolve()
        self.max_output_chars = max_output_chars
        self.test_runs = 0
        if max_output_chars <= 0 or not self.root.is_dir():
            raise ToolError("invalid sandbox configuration")

    def _cap(self, text: str) -> str:
        return text[: self.max_output_chars]

    def _relative(self, value: str, *, allow_root: bool = False) -> Path:
        if not isinstance(value, str) or not value:
            raise ToolError("path must be a non-empty string")
        relative = Path(value)
        if relative.is_absolute() or ".." in relative.parts:
            raise ToolError("path escapes the sandbox")
        if relative == Path("."):
            if allow_root:
                return relative
            raise ToolError("path must name a file")
        if relative.parts and relative.parts[0] in HIDDEN_ROOTS:
            raise ToolError("path is hidden from the agent")
        current = self.root
        for part in relative.parts:
            current = current / part
            if current.is_symlink():
                raise ToolError("symlinks are forbidden in the sandbox")
        resolved = (self.root / relative).resolve()
        if not resolved.is_relative_to(self.root):
            raise ToolError("path escapes the sandbox")
        return relative

    def list_files(self, directory: str = ".") -> str:
        relative = self._relative(directory, allow_root=True)
        base = (self.root / relative).resolve()
        if not base.is_dir():
            raise ToolError("list_files path is not a directory")
        files: list[str] = []
        for path in base.rglob("*"):
            rel = path.relative_to(self.root)
            if rel.parts and rel.parts[0] in HIDDEN_ROOTS:
                continue
            if path.is_symlink():
                raise ToolError("symlinks are forbidden in the sandbox")
            if path.is_file():
                files.append(rel.as_posix())
        return self._cap("\n".join(sorted(files)))

    def read_file(self, path: str) -> str:
        relative = self._relative(path)
        target = self.root / relative
        if not target.is_file():
            raise ToolError("read_file path is not a file")
        data = target.read_bytes()
        if b"\x00" in data:
            raise ToolError("binary files cannot be read")
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ToolError("file is not UTF-8") from error
        numbered = "\n".join(
            f"{index}: {line}" for index, line in enumerate(text.splitlines(), 1)
        )
        return self._cap(numbered)

    def apply_patch(self, patch_text: str) -> str:
        if not isinstance(patch_text, str) or not patch_text.strip():
            raise ToolError("patch must be a non-empty string")
        targets = {Path(value) for value in PATCH_TARGET.findall(patch_text)}
        if not targets:
            raise ToolError("patch has no valid file targets")
        allowed = set(self.episode.writable_paths)
        for target in targets:
            self._relative(target.as_posix())
            if target not in allowed:
                raise ToolError(f"patch target is not writable: {target}")
        result = subprocess.run(
            ["/usr/bin/patch", "--batch", "--forward", "-p1"],
            cwd=self.root,
            input=_recount_unified_diff(patch_text),
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )
        output = (result.stdout + result.stderr).strip()
        if result.returncode != 0:
            raise ToolError(self._cap(f"patch failed: {output}"))
        return self._cap(f"patch success: {output}")

    def write_file(self, path: str, content: str) -> str:
        relative = self._relative(path)
        if relative not in set(self.episode.writable_paths):
            raise ToolError(f"write target is not writable: {relative}")
        if not isinstance(content, str) or "\x00" in content:
            raise ToolError("content must be UTF-8 text")
        encoded = content.encode("utf-8")
        if len(encoded) > 64_000:
            raise ToolError("write content exceeds 64,000 bytes")
        destination = self.root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        with NamedTemporaryFile(
            mode="wb", dir=destination.parent, prefix=".agent-write-", delete=False
        ) as handle:
            temporary = Path(handle.name)
            handle.write(encoded)
        try:
            os.replace(temporary, destination)
        finally:
            temporary.unlink(missing_ok=True)
        return self._cap(f"write success: {relative} ({len(encoded)} bytes)")

    def run_tests(self, *, timeout: float = 60) -> dict[str, object]:
        if self.test_runs >= self.episode.max_test_runs:
            raise ToolError("visible test-run limit exceeded")
        self.test_runs += 1
        try:
            result = subprocess.run(
                list(self.episode.visible_verifier),
                cwd=self.root,
                capture_output=True,
                text=True,
                check=False,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as error:
            raise ToolError("visible verifier timeout") from error
        return {
            "test_run": self.test_runs,
            "exit_code": result.returncode,
            "stdout": self._cap(result.stdout),
            "stderr": self._cap(result.stderr),
        }
