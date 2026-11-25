# Slipstream — project rules for agents

Slipstream = llama.cpp fork that streams MoE expert weights from SSD (PGRN sidecar,
bounded per-layer CLOCK-LRU-K cache) + a Tauri 2 control app. Target host: Apple
M3 Pro, 36 GB unified memory. Read README.md, BENCHMARKS.md and docs/ARCH.md first.

## Source of truth (most important rule)

- The engine is **reconstructed**, not vendored: `./apply.sh` clones upstream
  llama.cpp @ the pinned commit (`PIN` in apply.sh; ported to upstream 2026-11-24 =
  `33aeb17f8586`, see upstream-port notes in bench/RESULTS.md) into `llama.cpp-slipstream/` (gitignored), copies
  `engine/` over it and applies `patches/slipstream-seams.patch`.
- You may develop inside `llama.cpp-slipstream/`, but work is **lost** unless it is
  synced back before committing. Use the `engine-sync` skill:
  - new/changed Slipstream-owned files (`src/peregrine_*`, `tools/pgrn-convert`,
    `tools/server/peregrine-routing.*`, `tests/test-peregrine-*`) → `engine/`
    (since PIN 33aeb17f8586 upstream ships Laguna; our Laguna changes live in the patch)
  - every change to an upstream file → regenerated `patches/slipstream-seams.patch`
- After syncing, prove it: a fresh `./apply.sh /tmp/x` must apply cleanly and build.

## Build

Use the `engine-build` skill. Self-contained static Metal build only:
`-DBUILD_SHARED_LIBS=OFF -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF -DGGML_METAL_EMBED_LIBRARY=ON
-DCMAKE_OSX_ARCHITECTURES=arm64`. `otool -L` must show only system frameworks.
Never edit files while a build is compiling them (it produces mixed binaries).

## Evidence rules (from BENCHMARKS.md / bench/RETEST_PLAN.md)

- A performance number is a hypothesis until recorded in `bench/RESULTS.md`
  (append-only) with method, regime and raw values. Record negatives too.
- A/B = same binary, same model, same cache, one variable; interleave arms
  (A,B,A,B…), ≥2 reps, ≥25 s cooldown. Use `bench/fetchwall/run_arm.sh` (skill
  `bench-ab`).
- Measure in the regime the app actually runs: llama-server with the app's flags incl.
  MTP speculation (`bench/fetchwall/run_arm_server.sh`), not only llama-completion
  (single-token decode hid a lookahead bug that only appears with multi-token verify).
- Measure in the regime a feature targets: cache band {4,6,8,10,14} GiB, PGRN on
  **internal NVMe** (`~/Modelle/...`). **Both GGUF and PGRN must be loaded from the internal SSD** (user rule) — copy from `/Volumes/Crucial X10/Modelle` first, never load from the external drive.
- Shippable only if: consistent win · **0 MB swap growth** · parity:
  - lossless features (prefetch, eviction, layout) must produce **byte-identical**
    greedy output to the baseline (same output hash in run_arm.sh);
  - lossy features (cache-steered routing, top-k changes, low-bit fallback) must be
    **opt-in, off by default**, and come with a measured quality cost (KL divergence /
    top-1 agreement vs exact routing via `llama-perplexity --kl-divergence`) shown
    honestly in the UI.
- Wrap every heavy job (model run, benchmark sweep, engine build) in `scripts/heavy.sh`
  so parallel agents never build during a measurement.
- "The Mac stays usable" is a hard invariant: never bypass RAM admission, never run
  two model processes at once, never benchmark while another heavy job runs.

## Experiment switches (engine)

- `PGRN_LOOKAHEAD=<m>` — lookahead gating (ON by default with --pgrn, m=0; `-1` disables):
  layer L+1's router on layer L's MoE input,
  top-(k+m) prefetched during layer L compute. Lossless. Read at the existing top-k
  sync, so it adds no GPU sync point.
- `PGRN_STEER=<bonus>` — cache-steered routing (Cache-Prior arXiv 2412.00099 / CECAR
  2611.20119 / PSR 2610.34721): bonus on resident experts' *selection* probability,
  mask snapshotted at ubatch start (deterministic); mixing weights stay unbiased. Lossy.

## Autonomous improvement loop

When working unattended (resume pings), follow `docs/IMPROVEMENT_LOOP.md`: pick the top item
from `docs/BACKLOG.md`, measure it against `bench/SCOREBOARD.md`, have a fresh reviewer agent
challenge every win before keeping it, and record winners and losers.

## Workflow per idea

Annahme → Recherche (alphaxiv MCP, Colibrì-first per docs/COLIBRI_REFERENCE.md) →
Plan → Umsetzung → Test → validate: keep only what wins under the rules above;
revert the rest and record it as a negative.

## Repo hygiene

- Work on a feature branch, never on `main`. Commit only when asked.
- App frontend is vanilla JS in `app/dist/` (no build step); keep EN+DE strings, a11y
  (labels, tablist, live regions) and the `app/scripts/test_*.mjs` contracts green.
- Never commit models (`*.gguf`, `*.pgrn`), binaries, or `llama.cpp-slipstream/`.
