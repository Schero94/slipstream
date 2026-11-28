# Slipstream — project rules for agents

Slipstream = llama.cpp fork that streams MoE expert weights from SSD (PGRN sidecar,
bounded per-layer CLOCK-LRU-K cache) + a Tauri 2 control app. Target host: Apple
M3 Pro, 36 GB unified memory. Read README.md, BENCHMARKS.md and docs/ARCH.md first.

## Source of truth (most important rule)

- The engine is **reconstructed**, not vendored: `./apply.sh` clones upstream
  llama.cpp @ the pinned commit (`PIN` in apply.sh; ported to upstream 2026-11-28 =
  `8e23278c7c36`, see docs/PORT_UPSTREAM_8e23278.md and bench/RESULTS.md) into `llama.cpp-slipstream/` (gitignored), copies
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
- Measure in the regime the app actually runs: llama-server with the app's flags
  (`bench/fetchwall/run_arm_server.sh`; default = streamed app regime, lookahead on, NO MTP;
  `SPEC_ARGS="--spec-type draft-mtp --spec-draft-n-max 4"` for the MTP regime), not only
  llama-completion (single-token decode hid a lookahead bug that only appears with multi-token verify).
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
- `PGRN_EVICT=lp|decay[:H]` — cache eviction experiment (opt-in): `lp` pins experts a lookahead
  kick requested until the layer's next batch; `decay` = lowest decayed use count (half-life H
  ubatches, default 16, finite, clamped ≥ 1e-3) + lp. Lossless. Values matched exactly
  (case-insensitive); unknown → WARN + default; decay is inert with --pgrn-hot-percent > 0 (WARN).
  The app sets `decay` for streamed runs with cache < 0.45 × PGRN size. `PGRN_TRACE=<path>` writes a
  cache trace for `bench/fetchwall/sim_eviction.py` (`--engine-summary` = per-arm outcomes incl. wasted reads).
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

## Public docs in plain language (user rule, 2026-11-26)

The website (`docs/index.html`, GitHub Pages), README.md and BENCHMARKS.md must always show
the current state, understandable for a non-expert. A change is not done until they match.

- **When:** every release, and every win kept on the scoreboard that changes what a user
  feels (speed, first-answer time, RAM, new model, new setting). Update the page in the same
  change, then merge to `main` so Pages redeploys, and check the live page.
- **How to write it:** say what the user notices first ("the next answer starts in under a
  second"), then the number, then the technical name in passing. Short sentences, no unexplained
  jargon (explain "decode", "prefill", "cache", "MoE" in a few words or avoid them). Use
  before → after tables with units, the machine and the model named.
- **Honest:** only numbers recorded in `bench/RESULTS.md`, in the app regime; say what is
  opt-in or lossy; keep losses and limits visible; remove or date stale claims (old cards,
  "active research" text that is no longer true).
- **Check:** after each release, read the whole page top to bottom as a newcomer: version in
  the metadata, the "New in" box, the benchmark tables, the feature cards and the download link
  all match the release notes.
- **Papers:** every arXiv paper behind a kept feature is linked on the website (papers
  section) and in README "Research behind Slipstream", with one plain sentence on what it does.

## Repo hygiene

- Work on `feat/fetch-wall`; merge to `main`, push and release autonomously only for verified
  milestones, and every release ships a verified `.dmg` (plus zip + node tarball).
- App frontend is vanilla JS in `app/dist/` (no build step); keep EN+DE strings, a11y
  (labels, tablist, live regions) and the `app/scripts/test_*.mjs` contracts green.
- Never commit models (`*.gguf`, `*.pgrn`), binaries, or `llama.cpp-slipstream/`.

## Lessons
- The app runs streamed models with lookahead and WITHOUT MTP. `bench/fetchwall/run_arm_server.sh`
  enabled MTP (draft 4) by default until 2026-11-28 (now no MTP by default) — check the speculative flags of any harness
  against `app/src-tauri/src/main.rs` before calling a run "app regime".

- After a release merge, switch back to `feat/fetch-wall` immediately; check
  `git branch --show-current` before every commit (2026-11-26: 12 commits landed on local `main`).
- A verification only counts for sources older than the binary: compare `stat` of the binary
  with the newest source change before trusting a test or benchmark (2026-11-26 stale binary).
