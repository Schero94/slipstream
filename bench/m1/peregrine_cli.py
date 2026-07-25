"""peregrine — one front door to the offline coding engine.

Dispatches to the built components so the whole thing is drivable from a single
command:

    peregrine serve      # start the resident gateway (headroom-gated)
    peregrine panel      # local control-panel UI
    peregrine status     # gateway health + memory headroom
    peregrine retrieve Q # bounded-window repo retrieval
    peregrine plan       # resident-vs-streaming load plan
    peregrine turn TASK  # retrieve context -> gateway generate -> show output

Each subcommand delegates to the module that owns the logic; only `turn` and
`status` compose here. Loopback only.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, Callable

from bench.m1.gateway_client import DEFAULT_GATEWAY_URL, complete
from bench.m1.retrieval import assemble_context, estimate_tokens, retrieve_repo


class PeregrineCliError(Exception):
    pass


def run_turn(
    task: str,
    *,
    repo_root: Path,
    gateway_url: str = DEFAULT_GATEWAY_URL,
    budget_tokens: int = 6000,
    max_tokens: int = 512,
    request_fn: Callable[[str, bytes, dict], bytes] | None = None,
) -> dict[str, Any]:
    if not task or not task.strip():
        raise PeregrineCliError("task must be non-empty")
    if not repo_root.is_dir():
        raise PeregrineCliError(f"repo_root is not a directory: {repo_root}")
    retrieval = retrieve_repo(repo_root, task, budget_tokens=budget_tokens)
    context = assemble_context(retrieval["selected"])
    from bench.m1.gateway_client import _build_prompt

    result = complete(
        _build_prompt(task, context, None),
        gateway_url=gateway_url,
        max_tokens=max_tokens,
        request_fn=request_fn,
    )
    return {
        "output": result["content"],
        "decode_tok_s": result["decode_tok_s"],
        "tokens": result["tokens"],
        "retrieval": {
            "files_scanned": retrieval["files_scanned"],
            "tokens_used": retrieval["tokens_used"],
            "context_files": [c.file for c in retrieval["selected"]],
        },
    }


def run_code(
    task: str,
    *,
    repo_root: Path,
    generate: Callable[[str, str, "str | None"], str],
    verify: Callable[[str], "tuple[bool, str]"],
    budget_tokens: int = 8000,
    max_retries: int = 1,
) -> dict[str, Any]:
    """Full product loop: retrieval -> generate -> verify -> one T1 retry -> escalation.

    `generate(task, context, feedback)` and `verify(output) -> (passed, detail)` are
    injected (gateway + apply/test in production; canned in tests). Returns the outcome,
    the winning output, and the retrieval manifest. Never contacts a cloud provider — a
    final failure is reported as consent-gated `escalation_required`.
    """
    if not task or not task.strip():
        raise PeregrineCliError("task must be non-empty")
    if not repo_root.is_dir():
        raise PeregrineCliError(f"repo_root is not a directory: {repo_root}")
    retrieval = retrieve_repo(repo_root, task, budget_tokens=budget_tokens)
    context = assemble_context(retrieval["selected"])
    manifest = {
        "files_scanned": retrieval["files_scanned"],
        "context_files": [c.file for c in retrieval["selected"]],
    }

    output = generate(task, context, None)
    passed, detail = verify(output)
    if passed:
        return {"outcome": "solved_first_pass", "output": output, "attempts": 1, "retrieval": manifest}

    if max_retries >= 1:
        output = generate(task, context, detail)  # retry fed the verifier failure
        passed, detail = verify(output)
        if passed:
            return {"outcome": "solved_after_retry", "output": output, "attempts": 2, "retrieval": manifest}

    return {
        "outcome": "escalation_required",
        "output": output,
        "attempts": 1 + max_retries,
        "last_failure": detail,
        "escalation": {"action": "AWAITING_CONSENT", "consent": "required", "provider_invocations": 0},
        "retrieval": manifest,
    }


# ---- subcommand handlers ----

def _cmd_serve(rest: list[str]) -> int:
    from bench.m1.gateway import main as gw
    return gw(["serve", *rest])


def _cmd_panel(rest: list[str]) -> int:
    from bench.m1.control_panel import main as panel
    return panel(rest)


def _cmd_retrieve(rest: list[str]) -> int:
    from bench.m1.retrieval import main as retr
    return retr(rest)


def _cmd_plan(rest: list[str]) -> int:
    from bench.m1.memory_admission import main as mem
    return mem(rest)


def _cmd_status(rest: list[str]) -> int:
    import subprocess
    import urllib.request

    from bench.m1.control_panel import parse_vm_stat, status_view

    health = None
    try:
        with urllib.request.urlopen(f"{DEFAULT_GATEWAY_URL}/peregrine/health", timeout=2) as r:
            health = json.loads(r.read())
    except Exception:
        health = None
    vm = subprocess.run(["vm_stat"], capture_output=True, text=True).stdout
    total = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout.strip())
    print(json.dumps(status_view(health, parse_vm_stat(vm), total_bytes=total), indent=2))
    return 0


def _cmd_turn(rest: list[str]) -> int:
    import argparse

    parser = argparse.ArgumentParser(prog="peregrine turn")
    parser.add_argument("task", nargs="+")
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--budget", type=int, default=6000)
    parser.add_argument("--max-tokens", type=int, default=512)
    parser.add_argument("--apply", action="store_true", help="apply extracted diffs to files (default: dry-run)")
    args = parser.parse_args(rest)
    try:
        result = run_turn(
            " ".join(args.task), repo_root=args.repo,
            budget_tokens=args.budget, max_tokens=args.max_tokens,
        )
    except PeregrineCliError as error:
        print(f"turn failed: {error}", file=sys.stderr)
        return 2
    print(f"# context: {result['retrieval']['files_scanned']} files scanned, "
          f"used {', '.join(result['retrieval']['context_files'][:6]) or '(none)'}")
    print(f"# decode: {result['decode_tok_s']} tok/s, {result['tokens']} tokens\n")
    print(result["output"])

    from bench.m1.diff_edit import apply_file_diffs, extract_file_diffs

    diffs = extract_file_diffs(result["output"])
    if diffs:
        report = apply_file_diffs(args.repo, diffs, confirm=args.apply)
        mode = "APPLIED" if args.apply else "DRY-RUN (use --apply to write)"
        print(f"\n# edits [{mode}]:")
        for path, r in report.items():
            print(f"#   {path}: {r['status']}" + (f" — {r['reason']}" if r.get("reason") else ""))
    return 0


def _cmd_code(rest: list[str]) -> int:
    import argparse
    import subprocess

    from bench.m1.diff_edit import apply_file_diffs, extract_file_diffs
    from bench.m1.gateway_client import make_generate

    parser = argparse.ArgumentParser(prog="peregrine code")
    parser.add_argument("task", nargs="+")
    parser.add_argument("--verify", required=True, help="shell command that passes (exit 0) when the task is done, e.g. tests")
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--apply", action="store_true", help="persist the winning edit (default: dry-run, always reverted)")
    parser.add_argument("--allow-dirty", action="store_true", help="skip the clean-working-tree safety check")
    parser.add_argument("--budget", type=int, default=8000)
    parser.add_argument("--max-tokens", type=int, default=1024)
    args = parser.parse_args(rest)
    repo = args.repo
    verify_cmd = args.verify

    # Safety: the loop reverts touched files via git, so refuse a dirty tree unless forced.
    dirty = subprocess.run(["git", "-C", str(repo), "status", "--porcelain"],
                           capture_output=True, text=True).stdout.strip()
    if dirty and not args.allow_dirty:
        print("code: refusing to run on a dirty working tree (uncommitted changes could be "
              "reverted by the verify loop). Commit/stash first, or pass --allow-dirty.", file=sys.stderr)
        return 2

    def verify(output: str) -> tuple[bool, str]:
        diffs = extract_file_diffs(output)
        if not diffs:
            return (False, "the model produced no applicable unified-diff edits")
        apply_file_diffs(repo, diffs, confirm=True)
        try:
            proc = subprocess.run(verify_cmd, shell=True, cwd=str(repo),
                                  capture_output=True, text=True, timeout=600)
            ok, detail = proc.returncode == 0, (proc.stdout + proc.stderr)[-2000:]
        finally:
            subprocess.run(["git", "-C", str(repo), "checkout", "--", *diffs.keys()],
                           capture_output=True)  # revert every attempt; the loop stays clean
        return (ok, detail)

    generate = make_generate(gateway_url=DEFAULT_GATEWAY_URL, max_tokens=args.max_tokens)
    try:
        result = run_code(" ".join(args.task), repo_root=repo, generate=generate, verify=verify,
                          budget_tokens=args.budget)
    except PeregrineCliError as error:
        print(f"code failed: {error}", file=sys.stderr)
        return 2

    print(f"# context: {result['retrieval']['files_scanned']} files scanned, "
          f"used {', '.join(result['retrieval']['context_files'][:6]) or '(none)'}")
    print(f"# outcome: {result['outcome']} (attempts: {result['attempts']})")
    solved = result["outcome"].startswith("solved")
    if solved and args.apply:
        applied = apply_file_diffs(repo, extract_file_diffs(result["output"]), confirm=True)
        print("# edits APPLIED:")
        for path, r in applied.items():
            print(f"#   {path}: {r['status']}")
    elif solved:
        print("# solved (DRY-RUN, reverted). Re-run with --apply to persist the edit.")
    else:
        print(f"# not solved locally -> {result['escalation']['action']} "
              f"(provider_invocations={result['escalation']['provider_invocations']}); "
              f"consent-gated cloud boost required.")
    return 0 if solved else 1


SUBCOMMANDS: dict[str, Callable[[list[str]], int]] = {
    "serve": _cmd_serve,
    "panel": _cmd_panel,
    "status": _cmd_status,
    "retrieve": _cmd_retrieve,
    "plan": _cmd_plan,
    "turn": _cmd_turn,
    "code": _cmd_code,
}

_USAGE = "usage: peregrine {serve|panel|status|retrieve|plan|turn|code} [args]"


def main(argv: list[str] | None = None) -> int:
    argv = argv if argv is not None else sys.argv[1:]
    if not argv:
        print(_USAGE, file=sys.stderr)
        return 2
    cmd, rest = argv[0], argv[1:]
    handler = SUBCOMMANDS.get(cmd)
    if handler is None:
        print(f"unknown command {cmd!r}\n{_USAGE}", file=sys.stderr)
        return 2
    return handler(rest)


if __name__ == "__main__":
    raise SystemExit(main())
