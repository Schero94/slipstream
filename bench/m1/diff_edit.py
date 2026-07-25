"""Diff-space editing — the output-efficiency lever (Blueprint §8.4).

Emitting edits as unified diffs instead of whole rewritten files cuts output
tokens 5–10x, which is the dominant latency/memory cost of a coding turn. This
module provides the backend primitive: a strict, fail-closed unified-diff
applier (no fuzz — a diff that doesn't match exactly is refused, so the
reliability ladder can retry) plus a token-savings estimate.

Line-based; reconstructs with '\\n'. The applier validates every context and
removed line against the source before producing output.
"""

from __future__ import annotations

import math
import os
import re
import tempfile
from pathlib import Path
from typing import Any

_HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")


class DiffEditError(Exception):
    """Raised when a diff is malformed or does not apply cleanly."""


def apply_unified_diff(source: str, diff: str) -> str:
    src = source.split("\n")
    out: list[str] = []
    si = 0  # 0-based index into src
    lines = diff.split("\n")
    i = 0
    saw_hunk = False

    while i < len(lines):
        line = lines[i]
        header = _HUNK.match(line)
        if not header:
            i += 1
            continue
        saw_hunk = True
        old_start = int(header.group(1))
        hunk_src_idx = old_start - 1
        if hunk_src_idx < si:
            raise DiffEditError("hunks out of order or overlapping")
        if hunk_src_idx > len(src):
            raise DiffEditError("hunk starts past end of source")
        out.extend(src[si:hunk_src_idx])
        si = hunk_src_idx
        i += 1

        while i < len(lines) and not _HUNK.match(lines[i]):
            hl = lines[i]
            i += 1
            if hl == "":
                # artifact of trailing newline in the diff text; not a hunk line
                continue
            tag, content = hl[0], hl[1:]
            if tag == " ":
                if si >= len(src) or src[si] != content:
                    raise DiffEditError(f"context mismatch at source line {si + 1}")
                out.append(src[si])
                si += 1
            elif tag == "-":
                if si >= len(src) or src[si] != content:
                    raise DiffEditError(f"removed-line mismatch at source line {si + 1}")
                si += 1
            elif tag == "+":
                out.append(content)
            elif tag == "\\":  # "\ No newline at end of file"
                continue
            else:
                raise DiffEditError(f"invalid hunk line: {hl!r}")

    if not saw_hunk:
        raise DiffEditError("no hunks found in diff")
    out.extend(src[si:])
    return "\n".join(out)


def estimate_savings(*, new_text: str, diff_text: str, chars_per_token: float = 4.0) -> dict[str, Any]:
    if not diff_text:
        raise DiffEditError("diff_text is empty")
    if chars_per_token <= 0:
        raise DiffEditError("chars_per_token must be > 0")
    full = math.ceil(len(new_text) / chars_per_token)
    diff = math.ceil(len(diff_text) / chars_per_token)
    return {
        "full_rewrite_tokens": full,
        "diff_tokens": diff,
        "ratio": round(full / diff, 2) if diff else 0.0,
    }


def extract_file_diffs(text: str) -> dict[str, str]:
    """Pull per-file unified diffs out of model output (fenced or bare).

    Returns {repo_relative_path: headerless_hunk_text}. Only segments that contain
    at least one `@@` hunk are kept. A `+++ b/path` header names the target; the
    `b/` prefix is stripped.
    """
    result: dict[str, str] = {}
    cur_path: str | None = None
    buf: list[str] = []

    def flush() -> None:
        nonlocal cur_path, buf
        if cur_path and any(line.startswith("@@") for line in buf):
            result[cur_path] = "\n".join(buf).strip("\n") + "\n"
        cur_path, buf = None, []

    for line in text.split("\n"):
        stripped = line.strip()
        if stripped.startswith("+++ "):
            flush()
            path = stripped[4:].strip()
            cur_path = path[2:] if path.startswith("b/") else path
            continue
        if stripped.startswith("--- "):
            continue
        if stripped.startswith("```"):
            flush()
            continue
        if cur_path is not None:
            if line.startswith(("@@", "+", "-", " ", "\\")):
                buf.append(line)
            else:
                flush()
    flush()
    return result


def _safe_target(root: Path, rel: str) -> Path | None:
    parts = Path(rel).parts
    if not rel or Path(rel).is_absolute() or ".." in parts:
        return None
    return root / rel


def apply_file_diffs(root: Path, diffs: dict[str, str], *, confirm: bool) -> dict[str, dict[str, Any]]:
    """Validate (and, only with confirm=True, apply) per-file diffs inside `root`.

    Fail-closed: a diff that does not match the file, a missing file, or a path that
    escapes the repo is reported and never written. Dry-run (confirm=False) validates
    without touching disk. Writes are atomic.
    """
    report: dict[str, dict[str, Any]] = {}
    for rel, hunk in diffs.items():
        target = _safe_target(root, rel)
        if target is None:
            report[rel] = {"status": "rejected", "reason": "path escapes repo or is absolute"}
            continue
        if target.is_symlink() or (target.exists() and not target.is_file()):
            report[rel] = {"status": "rejected", "reason": "target is not a regular file"}
            continue
        if not target.is_file():
            report[rel] = {"status": "missing", "reason": "target does not exist"}
            continue
        try:
            new_text = apply_unified_diff(target.read_text(encoding="utf-8"), hunk)
        except (DiffEditError, OSError) as error:
            report[rel] = {"status": "mismatch", "reason": str(error)}
            continue
        if not confirm:
            report[rel] = {"status": "would-apply", "new_line_count": len(new_text.split("\n"))}
            continue
        fd, tmp = tempfile.mkstemp(dir=str(target.parent), prefix=".peregrine-edit-")
        tmp_path = Path(tmp)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(new_text)
            os.replace(tmp_path, target)
        finally:
            if tmp_path.exists():
                tmp_path.unlink()
        report[rel] = {"status": "applied"}
    return report
