"""Bounded-context repo retrieval — the 32K active-window strategy.

Win-path context strategy (Plan v3): instead of a 200K raw context, Peregrine
fills a bounded active window with the repo content most relevant to the task,
using deterministic lexical (BM25) retrieval. No embeddings, no heavy deps, no
network — it runs anywhere the plugin runs and is fully unit-tested.

Token counts here are a deterministic byte-ratio estimate so budgeting works
offline; the runtime can substitute the gateway's exact `/tokenize` endpoint by
passing a different estimator. All decisions are pure; only the CLI touches disk.
"""

from __future__ import annotations

import argparse
import json
import math
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

DEFAULT_WINDOW_TOKENS = 32_768
DEFAULT_CHUNK_LINES = 40
DEFAULT_CHARS_PER_TOKEN = 4.0
BM25_K1 = 1.5
BM25_B = 0.75
MAX_FILE_BYTES = 1_000_000

# Directories that never carry task-relevant source and would blow the budget.
IGNORED_DIRS = {
    ".git",
    ".venv",
    "node_modules",
    "vendor",
    "models",
    "artifacts",
    "__pycache__",
    ".worktrees",
    ".superpowers",
    ".archive",
    "out",
}

_TOKEN_RE = re.compile(r"[a-z0-9]+")


class RetrievalError(Exception):
    """Raised for invalid retrieval configuration."""


@dataclass(frozen=True)
class RetrievalConfig:
    window_tokens: int = DEFAULT_WINDOW_TOKENS
    chunk_lines: int = DEFAULT_CHUNK_LINES
    chars_per_token: float = DEFAULT_CHARS_PER_TOKEN


@dataclass(frozen=True)
class Chunk:
    file: str
    start_line: int
    end_line: int
    text: str


def estimate_tokens(text: str, *, chars_per_token: float = DEFAULT_CHARS_PER_TOKEN) -> int:
    if chars_per_token <= 0:
        raise RetrievalError("chars_per_token must be > 0")
    return math.ceil(len(text) / chars_per_token)


def chunk_text(text: str, *, file: str, chunk_lines: int = DEFAULT_CHUNK_LINES) -> list[Chunk]:
    if chunk_lines < 1:
        raise RetrievalError("chunk_lines must be >= 1")
    lines = text.splitlines()
    if not lines:
        return []
    chunks: list[Chunk] = []
    for start in range(0, len(lines), chunk_lines):
        group = lines[start : start + chunk_lines]
        chunks.append(
            Chunk(
                file=file,
                start_line=start + 1,
                end_line=start + len(group),
                text="\n".join(group),
            )
        )
    return chunks


def tokenize_query(query: str) -> list[str]:
    terms = [term for term in _TOKEN_RE.findall(query.lower()) if len(term) >= 2]
    return list(dict.fromkeys(terms))  # unique, order-preserving


def _term_frequencies(text: str) -> dict[str, int]:
    counts: dict[str, int] = {}
    for term in _TOKEN_RE.findall(text.lower()):
        if len(term) < 2:
            continue
        counts[term] = counts.get(term, 0) + 1
    return counts


def _bm25_scores(chunks: list[Chunk], query_terms: list[str]) -> list[float]:
    tfs = [_term_frequencies(c.text) for c in chunks]
    lengths = [sum(tf.values()) for tf in tfs]
    n = len(chunks)
    avgdl = (sum(lengths) / n) if n else 0.0
    df = {term: sum(1 for tf in tfs if term in tf) for term in query_terms}
    scores = []
    for tf, dl in zip(tfs, lengths):
        score = 0.0
        for term in query_terms:
            freq = tf.get(term, 0)
            if freq == 0:
                continue
            idf = math.log(1 + (n - df[term] + 0.5) / (df[term] + 0.5))
            denom = freq + BM25_K1 * (1 - BM25_B + BM25_B * (dl / avgdl if avgdl else 0.0))
            score += idf * (freq * (BM25_K1 + 1)) / denom
        scores.append(score)
    return scores


def retrieve(
    chunks: list[Chunk],
    query: str,
    *,
    budget_tokens: int,
    config: RetrievalConfig | None = None,
) -> list[Chunk]:
    cfg = config or RetrievalConfig()
    query_terms = tokenize_query(query)
    if not query_terms or not chunks:
        return []
    scores = _bm25_scores(chunks, query_terms)
    ranked = [
        (score, chunk)
        for score, chunk in zip(scores, chunks)
        if score > 0
    ]
    # Deterministic: score desc, then file, then start_line.
    ranked.sort(key=lambda item: (-item[0], item[1].file, item[1].start_line))
    selected: list[Chunk] = []
    used = 0
    for _, chunk in ranked:
        cost = estimate_tokens(chunk.text, chars_per_token=cfg.chars_per_token)
        if used + cost > budget_tokens:
            continue
        selected.append(chunk)
        used += cost
    return selected


def pack_active_window(
    *,
    window_tokens: int,
    system_tokens: int,
    conversation_tokens: int,
    output_reserve: int,
) -> int:
    """Retrieval budget left in the active window after reserved regions."""
    budget = window_tokens - system_tokens - conversation_tokens - output_reserve
    if budget <= 0:
        raise RetrievalError(
            "no retrieval budget left: reserves exceed the active window"
        )
    return budget


def assemble_context(chunks: list[Chunk]) -> str:
    blocks = []
    for chunk in chunks:
        header = f"# {chunk.file}:{chunk.start_line}-{chunk.end_line}"
        blocks.append(f"{header}\n{chunk.text}")
    return "\n\n".join(blocks)


def _read_text_file(path: Path) -> str | None:
    try:
        if path.stat().st_size > MAX_FILE_BYTES:
            return None
        data = path.read_bytes()
    except OSError:
        return None
    if b"\x00" in data:
        return None
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return None


def _iter_repo_files(root: Path):
    for path in sorted(root.rglob("*")):
        if any(part in IGNORED_DIRS for part in path.relative_to(root).parts):
            continue
        if path.is_symlink() or not path.is_file():
            continue
        yield path


def retrieve_repo(
    root: Path,
    query: str,
    *,
    budget_tokens: int,
    config: RetrievalConfig | None = None,
) -> dict[str, Any]:
    cfg = config or RetrievalConfig()
    chunks: list[Chunk] = []
    files_scanned = 0
    for path in _iter_repo_files(root):
        text = _read_text_file(path)
        if text is None:
            continue
        files_scanned += 1
        rel = str(path.relative_to(root))
        chunks.extend(chunk_text(text, file=rel, chunk_lines=cfg.chunk_lines))
    selected = retrieve(chunks, query, budget_tokens=budget_tokens, config=cfg)
    tokens_used = sum(estimate_tokens(c.text, chars_per_token=cfg.chars_per_token) for c in selected)
    return {
        "query": query,
        "budget_tokens": budget_tokens,
        "files_scanned": files_scanned,
        "chunks_total": len(chunks),
        "selected": selected,
        "tokens_used": tokens_used,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--query", required=True)
    parser.add_argument("--budget", type=int, default=DEFAULT_WINDOW_TOKENS)
    parser.add_argument("--chunk-lines", type=int, default=DEFAULT_CHUNK_LINES)
    parser.add_argument("--chars-per-token", type=float, default=DEFAULT_CHARS_PER_TOKEN)
    parser.add_argument("--emit-context", action="store_true", help="also print the assembled context")
    args = parser.parse_args(argv)
    if not args.repo.is_dir():
        print(json.dumps({"error": f"not a directory: {args.repo}"}))
        return 2
    cfg = RetrievalConfig(chunk_lines=args.chunk_lines, chars_per_token=args.chars_per_token)
    try:
        result = retrieve_repo(args.repo, args.query, budget_tokens=args.budget, config=cfg)
    except RetrievalError as error:
        print(json.dumps({"error": str(error)}))
        return 2
    manifest = {
        "query": result["query"],
        "budget_tokens": result["budget_tokens"],
        "files_scanned": result["files_scanned"],
        "chunks_total": result["chunks_total"],
        "tokens_used": result["tokens_used"],
        "selected": [
            {
                "file": c.file,
                "start_line": c.start_line,
                "end_line": c.end_line,
                "tokens": estimate_tokens(c.text, chars_per_token=cfg.chars_per_token),
            }
            for c in result["selected"]
        ],
    }
    if args.emit_context:
        manifest["context"] = assemble_context(result["selected"])
    print(json.dumps(manifest, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
