# Slipstream — Benchmarks

Real, reproducible numbers on a **36 GB Apple-Silicon Mac**. This is an evidence-first project: we publish the honest results, including the negatives. Raw logs live in [`bench/RESULTS.md`](bench/RESULTS.md).

## TL;DR

- A **35B MoE** coder runs at **~13–19 tok/s** streamed from SSD, with the Mac staying usable.
- A **118B MoE** (Laguna S 2.1) that does **not fit in 36 GB RAM** runs at **~2.8 tok/s** — from 0.72 before optimization (**3.9×**).
- The single biggest lever was **storage placement**, not a clever kernel.

---

## Qwen3.6-35B-A3B (Q4_K), internal NVMe

Cache size vs. decode throughput and cache hit-rate (single stream):

| Resident cache | Decode | Hit-rate | Notes |
|---:|---:|---:|---|
| 2 GiB | 5.5 tok/s | 21 % | fits a 16 GB Mac, more SSD reads |
| 10 GiB | ~13 tok/s | 78 % | recommended for interactive use on 36 GB |
| 14 GiB | ~19 tok/s | 86 % | needs a mostly-idle Mac |

**Prefill acceleration** (reading a large ~30k-token agent prompt):

| Config | Prefill |
|---|---:|
| `ubatch 512`, io=1 | ~75 tok/s |
| `ubatch 2048`, io=8–16 | **~208–238 tok/s (2.7–3.2×)** |

Parallel I/O threads (deeper SSD queue) + a larger prefill batch (more tokens share one expert fetch) are what move prefill; io=16 gave ~+14 % over io=8 (diminishing returns).

---

## Laguna S 2.1 (118B-A8B, Q4_K) — larger than RAM

A full optimization sweep on a 120-token generation. **Each row adds one lever:**

| Configuration | Decode | vs. baseline | Hit-rate |
|---|---:|---:|---:|
| PGRN on external USB SSD, no draft | 0.72 tok/s | 1.0× | 44 % |
| **PGRN on internal NVMe** | 1.95 tok/s | **2.7×** | 44 % |
| + DFlash speculative draft | 2.36 tok/s | 3.3× | 27 % |
| + larger cache (10 GiB) | **2.83 tok/s** | **3.9×** | 38 % |

### The finding

Decode is **~90 % SSD-fetch-bound**. The streamed file (PGRN) belongs on your **fastest** disk; the model file (GGUF) is read once at load and can live anywhere. Moving *only* the PGRN from an external USB SSD (~0.9 GB/s) to the internal NVMe cut fetch time from **166 s → 46 s** and gave **2.7× on its own** — more than the draft model and the cache combined.

The DFlash draft adds ~21 % but touches more experts (hit-rate drops); a larger cache then recovers it. Cache is capped near 10 GiB by the RAM admission gate on a 36 GB machine — the model refuses to load a cache that would swap the Mac.

### Honest ceiling

~2.8–3 tok/s for a 118B on 36 GB — **usable for batch/non-interactive coding**, marginal for fast chat. The 35B is the interactive workhorse. We could not find a lever that breaks the SSD-bandwidth wall; the storage split is the honest way to raise effective bandwidth.

---

## I/O levers, re-validated at real cache sizes

Several levers were originally qualified at **cache 2 GiB** (22 % hit-rate — a regime almost no Mac uses). Re-measuring across the band the app actually recommends — {4, 6, 8, 10 GiB} — **flipped one verdict** and confirmed the rest. Method: interleaved A/B, `sysctl vm.swapusage` before/after each run, parity preserved.

### Compact "zero-copy" slots — a win that was hiding (now default-on)

The GPU reads streamed experts **directly** from the cache arena (strided views) — no second CPU→GPU upload copy. Rejected at cache 2 GiB (few hits → nothing to save). At the real band it's a clear, swap-safe win:

| Cache | Hit-rate | Baseline | Compact | Δ |
|---:|---:|---:|---:|---:|
| 4 GiB | 69 % | 4.84 | 5.53 | **+14 %** |
| 6 GiB | 78 % | 5.77 | 7.17 | **+24 %** |
| 8 GiB | 83 % | 7.85 | 8.85 | **+13 %** |
| 10 GiB | 87 % | 11.32 | 11.48 | +1 % (neutral) |

Swap growth: **0 MB** at every point. Parity: exact logits on CPU + Metal. **The lesson: a lever rejected at one operating point can be a real win at another — always re-measure in the regime it targets.** (An early single run showed −7 % at cache 10; the clean interleaved sweep corrected it to neutral — hence ≥1 controlled rep, never one shot.)

### Confirmed negatives (re-tested at the real band, verdict held)

- **Online co-activation prefetch predictor** — −8 % at cache 6. Prediction isn't accurate enough to beat the cost of the speculative reads on a fetch-bound path.
- **HOT/WARM tier reservation** (`--pgrn-hot-percent`) — −1 to −6 %, *even on top of compact*. Reserving slots for "hot" experts shrinks the general working set → lower hit-rate. Pure CLOX (hot = 0) stays best.
- **Dual-SSD striping** — tok/s-negative on internal-NVMe + slow-USB (shared bus, not independent). It is a real **capacity / flexibility** feature (spread a model across two disks) for setups with two comparably-fast SSDs — shipped opt-in, not a speed default.

---

## Structured output — grammar-forced drafts (lossless, default-on)

When a request carries a `grammar` / `json_schema` (tool calls, structured extraction), the grammar
*itself* is a draft source: wherever it admits exactly one legal next character (braces, quotes, key
names, separators), I inject that forced span as **pre-accepted draft tokens** into the same verify
step as the draft model. It never constrains sampling — the target verifies every token, so the output
is **byte-identical** with the feature off (I parity-gate it on real models). On a fetch-bound decode,
fewer target forwards means **fewer expert fetches off the SSD** — so this saves the scarce resource,
not just compute.

Measured with an interleaved A/B, greedy. `tok/forward` is the regime-independent metric; `tok/s` is the payoff:

| Model / regime | Workload | forwards | tok/forward | Decode | Δ |
|---|---:|---:|---:|---:|---:|
| Qwen3.6-35B, fits in RAM (not fetch-bound) | easy JSON (draft already nails it) | 17 → 17 | 4.1 → 4.1 | 38.1 → 38.0 | **neutral** |
| Qwen3.6-35B, in RAM | rigid schema (long, unusual keys) | 48 → 34 | 2.9 → 4.1 | 25.2 → 33.2 | **+32 %** |
| **Laguna 118B, streamed, ~97 % fetch-bound** | rigid schema | **54 → 37** | 2.1 → 3.0 | 0.37 → 0.53 | **+45 %** |

The win **tracks structural rigidity × how weak the draft model is there**: on easy JSON the speculative
draft (MTP) already predicts the scaffolding, so there is nothing to add; on rigid or unusual schemas the
draft mispredicts (DFlash on Laguna accepted only 27 %), so the grammar's *certain* tokens cut forwards by
a third — and because Laguna decode is ~97 % fetch-bound, that −31 % forwards becomes **+45 % tok/s**.

An **adaptive guard** only engages the grammar draft when its forced span beats the draft model's recent
mean accepted length — that's why the easy-JSON row is *neutral, not negative*: the guard stands the
feature down where it wouldn't help. So it's **on by default** and safe (`--spec-grammar-draft`; a no-op
when there's no grammar). This is a *targeted* win for the real coding-agent case — tool calls and
schema-constrained output — not a universal speedup, and I'd rather say that plainly than oversell it.

---

## What did *not* work (recorded honestly)

- **OS page-cache instead of our bounded arena** — ~75 % faster, but 1000+ swapouts: it breaks "the Mac stays usable." Rejected.
- **Naive coupled-layer prefetch** — 5–15 % *slower* (cache thrash). Rejected.
- **Lossless expert compression / expert-pinning** — low ROI *after* the storage split made fetch cheap; not implemented.
- **ANE / MLX compute paths** — real 1.2–1.4× gains, but only for models that fit in RAM; they don't help the >RAM streaming regime.

Method: `python -m bench.m0d.run_native_streaming_ab` (per-turn decode tok/s, hits/misses, memory). A/B = same model, same cache, change one variable. Parity gate (`ctest -R test-peregrine-model-e2e`) asserts streamed output is bit-identical to resident (NMSE = 0) — it passes.
