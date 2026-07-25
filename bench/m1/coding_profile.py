"""Peregrine coding profile (Phase 6).

Composes one `llama-server` command tuned for local coding: it sizes the PGRN expert
cache to the available RAM via the Phase-4 fit calculator and stacks the levers that
matter for a coding agent's real workload:

  - PGRN streaming + parallel cold reads (--pgrn-io-threads)  -> big model fits, fast
  - Flash-Attention (--flash-attn on)                          -> Metal speed, less KV RAM
  - KV-cache quantization (--cache-type-k/v q8_0)              -> longer context / more room
  - KV reuse across turns (--cache-reuse)                      -> no re-prefill each turn
  - MTP speculative decode (--spec-type draft-mtp)             -> faster decode

For coding, avoiding the re-prefill of a large, mostly-unchanged context each turn is
the single biggest practical speed-up — hence prompt/KV reuse is on by default.
"""

from __future__ import annotations

import argparse
import shlex
import sys
from pathlib import Path
from typing import Any

from bench.m1.memory_admission import GIB, LoadPlanError
from bench.m1.model_fit import fit, headroom_for_ram, spec_from_pgrn


def build_coding_command(
    *,
    server: Path,
    model: Path,
    pgrn: Path,
    ram_bytes: int,
    io_threads: int = 4,
    ctx: int = 8192,
    kv_quant: bool = True,
    flash_attn: bool = True,
    mtp_draft_max: int = 4,
    cache_reuse: int = 256,
    n_expert_used: int = 8,
    host: str = "127.0.0.1",
    port: int = 8080,
) -> dict[str, Any]:
    """Build the coding server command + the fit plan it is based on."""
    if kv_quant and not flash_attn:
        # llama.cpp requires flash-attn for a quantized V cache; keep the combo valid.
        raise LoadPlanError("kv_quant requires flash_attn (quantized V cache needs Flash Attention)")

    gguf_bytes = model.stat().st_size if model.exists() else None
    spec = spec_from_pgrn(pgrn, n_expert_used=n_expert_used, gguf_bytes=gguf_bytes)
    plan = fit(spec, ram_bytes, io_threads)
    if plan["mode"] == "refuse":
        raise LoadPlanError(f"model does not fit on {ram_bytes / GIB:.0f} GiB: {plan['reason']}")

    headroom_gib = round(headroom_for_ram(ram_bytes) / GIB, 1)
    # In resident mode the whole expert set fits; cache it all. In streaming mode use the
    # fit budget. Either way the same bounded native path runs, sized to the RAM.
    cache_gib = round(spec.expert_total_bytes / GIB, 2) if plan["mode"] == "resident" else plan["cache_gib"]

    cmd: list[str] = [
        str(server),
        "--model", str(model),
        "--pgrn", str(pgrn),
        "--pgrn-cache-gb", f"{cache_gib:g}",
        "--pgrn-headroom-gb", f"{headroom_gib:g}",
        "--gpu-layers", "99",
        "--ctx-size", str(ctx),
        "--host", host,
        "--port", str(port),
    ]
    if io_threads > 1:
        cmd += ["--pgrn-io-threads", str(io_threads)]
    if flash_attn:
        cmd += ["--flash-attn", "on"]
    if kv_quant:
        cmd += ["--cache-type-k", "q8_0", "--cache-type-v", "q8_0"]
    if cache_reuse > 0:
        cmd += ["--cache-reuse", str(cache_reuse)]
    if mtp_draft_max > 0:
        cmd += ["--spec-type", "draft-mtp", "--spec-draft-n-max", str(mtp_draft_max)]

    return {
        "command": cmd,
        "mode": plan["mode"],
        "cache_gib": cache_gib,
        "headroom_gib": headroom_gib,
        "predicted_decode_tok_s": plan.get("predicted_decode_tok_s", 0.0),
        "hit_rate": plan.get("hit_rate", 0.0),
        "levers": {
            "flash_attn": flash_attn,
            "kv_quant": kv_quant,
            "io_threads": io_threads,
            "cache_reuse": cache_reuse,
            "mtp": mtp_draft_max > 0,
        },
    }


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description="Generate the Peregrine coding server command")
    p.add_argument("--server", type=Path, required=True)
    p.add_argument("--model", type=Path, required=True, help="the .gguf")
    p.add_argument("--pgrn", type=Path, required=True, help="the .pgrn sidecar")
    p.add_argument("--ram-gb", type=float, required=True)
    p.add_argument("--io-threads", type=int, default=4)
    p.add_argument("--ctx", type=int, default=8192)
    p.add_argument("--port", type=int, default=8080)
    p.add_argument("--no-kv-quant", action="store_true")
    p.add_argument("--n-expert-used", type=int, default=8)
    args = p.parse_args(argv)

    out = build_coding_command(
        server=args.server, model=args.model, pgrn=args.pgrn,
        ram_bytes=round(args.ram_gb * GIB), io_threads=args.io_threads,
        ctx=args.ctx, kv_quant=not args.no_kv_quant, port=args.port,
        n_expert_used=args.n_expert_used,
    )
    print(f"# mode={out['mode']} cache={out['cache_gib']}G reserve={out['headroom_gib']}G "
          f"~{out['predicted_decode_tok_s']} tok/s (hit {out['hit_rate']*100:.0f}%)")
    print(f"# levers: {out['levers']}")
    print(" ".join(shlex.quote(c) for c in out["command"]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
