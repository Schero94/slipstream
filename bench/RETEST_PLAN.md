# RETEST_PLAN — re-validate rejected results at real operating points, ship the winners

**Trigger:** say *"run the retest plan"* (or a specific step) and I execute it end-to-end.

## Why this exists
Two thinking-error classes were found in the rejected/negative results:
- **Class 1 — rejected at the wrong operating point.** Levers were qualified/rejected at **cache 2 GiB** (22 % hit-rate — a regime almost no real Mac uses). At real cache sizes the conclusion can flip. Proven: the **compact/zero-copy** path was "rejected" at 2 GiB but is **+25 % at cache 6 GiB** (swap-safe, 0 MB swap). It is regime-specific though (−7 % at cache 10), so it must be *mapped*, not assumed.
- **Class 2 — "FAIL" on a pure-speed gate.** Long-context decode (14 tok/s @ 64k, 23 @ 32k) was stamped **FAIL** against an arbitrary **25 tok/s** admission floor. Running 64k context on a Mac at all is a *capability*; 14 tok/s is *usable*. The verdict is a speed-tunnel artifact, not a real failure.

**Rule (see memory `speed-usability-balance`):** judge OVERALL betterness (usability, capability, RAM fit, reliability), not tok/s alone; and re-measure at the regime a feature actually targets before writing it off.

## Proper test conditions (NOT 2 GiB)
Real Mac cache band the app actually recommends (io/ctx auto-derived per machine):
- 16 GB Mac → cache ~4 GiB · 24 GB → ~6–8 · 36 GB → ~10–14.
- **Test cache band: {4, 6, 8, 10, 14} GiB.** io-threads 8, ctx 4096, batch/ubatch 2048.
- Model: `Qwen3.6-35B-A3B` (fast iteration). Spot-check with Laguna 118B where relevant.
- GGUF: `/Volumes/Crucial X10/Modelle/qwen3.6-35b-a3b-q4/…UD-Q4_K_XL.gguf` · PGRN: `/Users/schero/Modelle/qwen3.6-35b-a3b-q4/…UD-Q4_K_XL.pgrn`
- Engine: the bundled static `llama-server` in `Slipstream.app`.

## Method (rigor — no thermal/noise artifacts)
- **Interleaved A/B** (base, variant, base, variant …) so thermal drift hits both sides.
- **≥ 2 reps** per (config × cache); a win must be consistent, not a single run.
- **Cooldown ≥ 25 s** between server runs; short decodes (100 tok) to limit thermal drift; discard runs whose baseline is anomalous (e.g. a 15-min stall).
- **Swap-safety:** capture `sysctl vm.swapusage` before/after each run — a lever is only shippable if **0 MB swap growth**.
- **Parity:** streamed output must stay bit-identical (CRC per read; `ctest -R test-peregrine-model-e2e` NMSE=0) — a lever that changes output is rejected regardless of speed.
- Report **all** numbers incl. negatives; state the regime where each holds.

## Class 1 re-tests (wrong operating point)
1. **Compact / zero-copy (`--pgrn-compact-slots`)** — *partially done.* Finish the **band sweep** {4,6,8,10,14}, ≥2 reps, find the exact win/lose crossover. Deliver a "win band" (e.g. cache 5–8 GiB).
2. **HOT/WARM tiering (`--pgrn-hot-percent`)** — rejected at 2 GiB with an explicit precondition: *"keep HOT=0 until the compact single-arena path changes the memory/copy cost."* **That precondition is now met.** Sweep `hot-percent ∈ {0,10,20}` × cache {6,10}, **both with and without `--pgrn-compact-slots`**.
3. **Any other 2-GiB-only rejection** (arena/tier variants in `bench/m0d/*`) — re-run at the band if cheap.

## Class 2 re-frame (speed-gate)
4. **Long-context throughput** — re-measure decode at context {4k, 32k, 64k} and present as an **honest tradeoff curve** ("usable at long context"), **not** pass/fail against 25 tok/s. Reconsider whether a fixed tok/s admission floor is the right gate at all (capability + swap-safety may matter more).

## Ship the winners (integration)
For every lever that consistently wins in a defined regime + is swap-safe + parity-clean:
1. **Engine:** it is already in the fork → confirm the flag; **rebuild the static engine** (`build-static`, the self-contained flags) and **bundle it** into `Slipstream.app` (`cp … resources/llama-server`). "Wir nehmen die mit."
2. **UI (always-UI principle):** an **Advanced toggle** with honest per-regime guidance (e.g. "Compact slots — faster at moderate cache ~5–8 GiB"). Where a clear win band exists, **auto-enable inside that band** (the app knows the machine's recommended cache); never blind-on outside it.
3. **Docs:** add the honest result (win band + where it doesn't help) to `README`/`BENCHMARKS.md` and the landing page's "what we tried" section; re-frame Class-2 "FAIL" language to "usable at long context".

## Acceptance criteria (per lever)
- ✅ consistent win (≥2 reps) in a stated regime · ✅ 0 MB swap growth · ✅ parity preserved (NMSE=0 / CRC) · then → engine-bundle + UI toggle (auto-on only in the win band) + docs.
- ❌ otherwise → recorded as an honest negative with its regime, no ship.

## Current status
- Compact: swap-safe ✓; +25 % @ cache 6 ✓ (×2 A/Bs); −7 % @ cache 10 ✗ → **needs the full band sweep to fix the crossover.**
- HOT-tiering: **not yet re-tested** with the new precondition.
- Long-context: **not yet re-framed.**
