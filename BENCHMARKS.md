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

## What did *not* work (recorded honestly)

- **OS page-cache instead of our bounded arena** — ~75 % faster, but 1000+ swapouts: it breaks "the Mac stays usable." Rejected.
- **Naive coupled-layer prefetch** — 5–15 % *slower* (cache thrash). Rejected.
- **Lossless expert compression / expert-pinning** — low ROI *after* the storage split made fetch cheap; not implemented.
- **ANE / MLX compute paths** — real 1.2–1.4× gains, but only for models that fit in RAM; they don't help the >RAM streaming regime.

Method: `python -m bench.m0d.run_native_streaming_ab` (per-turn decode tok/s, hits/misses, memory). A/B = same model, same cache, change one variable. Parity gate (`ctest -R test-peregrine-model-e2e`) asserts streamed output is bit-identical to resident (NMSE = 0) — it passes.
