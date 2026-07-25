"""Progress benchmark — one repeatable result that shows the optimization arc.

Ingests native-streaming qualification artifacts (produced by run_native_streaming_ab)
and renders ONE comparison: baseline -> optimized, with decode / prefill / hit-rate /
peak-RSS / swapouts / PASS per config, plus the headline speedup. Works on the
artifacts already on disk (no new run needed), or on fresh ones you pass in.

Usage:
    python -m bench.m0d.progress_benchmark --preset qualified
    python -m bench.m0d.progress_benchmark --artifact "2 GiB io1=bench/artifacts/m0d/x.json" \
                                            --artifact "14 GiB io4=bench/artifacts/m0d/y.json" \
                                            --output bench/artifacts/m0d/progress.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

GIB_KB = 1024 * 1024  # peak_rss is in KiB; /GIB_KB -> GiB

# Built-in lineup over the qualified artifacts kept in bench/artifacts/m0d.
QUALIFIED_PRESET: list[tuple[str, str]] = [
    ("2 GiB · io1 (baseline)", "bench/artifacts/m0d/native-pgrn-tier-arena-20260722.json"),
    ("10 GiB · io4", "bench/artifacts/m0d/rq10-io4-a.json"),
    ("14 GiB · io1", "bench/artifacts/m0d/q14-io1-a.json"),
    ("14 GiB · io4", "bench/artifacts/m0d/q14-io4-a.json"),
]


def summarize_artifact(label: str, path: str | Path) -> dict[str, Any]:
    """Extract the progress-relevant summary from one qualification artifact."""
    p = Path(path)
    if not p.exists():
        return {"label": label, "path": str(p), "present": False, "status": "MISSING"}
    d = json.loads(p.read_text())
    cfg = d.get("configuration", {}) or {}
    reqs = d.get("requests", []) or []
    tel = d.get("telemetry", []) or []
    vm = d.get("vm", {}) or {}
    decode = [round(float(r.get("decode_tokens_per_second", 0.0)), 2) for r in reqs]
    prefill = [round(float(r.get("prompt_tokens_per_second", 0.0)), 2) for r in reqs]
    hit = [round(float(t.get("hit_percent", 0.0)), 1) for t in tel]
    return {
        "label": label,
        "path": str(p),
        "present": True,
        "status": (d.get("decision", {}) or {}).get("status", "?"),
        "reasons": (d.get("decision", {}) or {}).get("reasons", []),
        "cache_gib": cfg.get("cache_gib"),
        "io_threads": cfg.get("io_threads", 1),
        "headroom_gib": cfg.get("headroom_gib"),
        "decode_tok_s": decode,
        "decode_best": max(decode) if decode else 0.0,
        "prefill_tok_s": prefill,
        "hit_percent": hit,
        "peak_rss_gib": round(d.get("peak_rss_kb", 0) / GIB_KB, 1),
        "swapouts": vm.get("swapouts_delta"),
        "free_after_pct": d.get("memory_free_percent_after"),
        "thinking_ok": bool(reqs) and all(r.get("thinking_observed") for r in reqs),
    }


def build_progress(items: list[tuple[str, str]]) -> dict[str, Any]:
    rows = [summarize_artifact(label, path) for label, path in items]
    passing = [r for r in rows if r.get("present") and r.get("decode_best")]
    baseline = passing[0] if passing else None
    best = max(passing, key=lambda r: r["decode_best"]) if passing else None
    speedup = (
        round(best["decode_best"] / baseline["decode_best"], 2)
        if baseline and best and baseline["decode_best"] > 0
        else None
    )
    return {
        "rows": rows,
        "baseline": baseline["label"] if baseline else None,
        "best": best["label"] if best else None,
        "baseline_decode_tok_s": baseline["decode_best"] if baseline else None,
        "best_decode_tok_s": best["decode_best"] if best else None,
        "speedup_x": speedup,
        "all_zero_swapouts": all(r.get("swapouts") == 0 for r in passing) if passing else None,
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "| Config | Status | Decode tok/s | Prefill tok/s | Hit% | Peak-RSS | Swap | Free after |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for r in report["rows"]:
        if not r.get("present"):
            lines.append(f"| {r['label']} | MISSING | — | — | — | — | — | — |")
            continue
        dec = " / ".join(f"{x:g}" for x in r["decode_tok_s"]) or "—"
        pre = " / ".join(f"{x:g}" for x in r["prefill_tok_s"]) or "—"
        hit = " / ".join(f"{x:g}" for x in r["hit_percent"]) or "—"
        lines.append(
            f"| {r['label']} | {r['status']} | {dec} | {pre} | {hit} | "
            f"{r['peak_rss_gib']:g} GiB | {r['swapouts']} | {r['free_after_pct']}% |"
        )
    if report.get("speedup_x"):
        lines.append("")
        lines.append(
            f"**Progress:** {report['baseline']} → {report['best']} = "
            f"{report['baseline_decode_tok_s']:g} → {report['best_decode_tok_s']:g} tok/s "
            f"(**{report['speedup_x']}×**)"
            + (", 0 swapouts across all runs" if report.get("all_zero_swapouts") else "")
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Progress benchmark over qualification artifacts")
    ap.add_argument("--preset", choices=["qualified"], help="use the built-in qualified lineup")
    ap.add_argument("--artifact", action="append", default=[],
                    help='add a row as "label=path" (repeatable)')
    ap.add_argument("--output", type=Path, help="write the progress artifact JSON here")
    ap.add_argument("--json", action="store_true", help="print JSON instead of the table")
    args = ap.parse_args(argv)

    items: list[tuple[str, str]] = []
    if args.preset == "qualified":
        items.extend(QUALIFIED_PRESET)
    for spec in args.artifact:
        if "=" not in spec:
            ap.error(f"--artifact must be 'label=path', got: {spec}")
        label, path = spec.split("=", 1)
        items.append((label.strip(), path.strip()))
    if not items:
        ap.error("provide --preset qualified and/or one or more --artifact label=path")

    report = build_progress(items)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2) if args.json else render_markdown(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
