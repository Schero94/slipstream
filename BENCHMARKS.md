# Slipstream — Benchmarks

Real, reproducible numbers on a **36 GB Apple-Silicon Mac**. This is an evidence-first project: we publish the honest results, including the negatives. Raw logs live in [`bench/RESULTS.md`](bench/RESULTS.md).

## TL;DR

- A **35B MoE** coder runs at **~13–19 tok/s** streamed from SSD, with the Mac staying usable.
- A **118B MoE** (Laguna S 2.1) that does **not fit in 36 GB RAM** runs at **~2.8 tok/s** — from 0.72 before optimization (**3.9×**).
- The single biggest lever was **storage placement**, not a clever kernel.
- The engine now also **streams correctly on Linux/CPU** — same tokens as resident, 487 vs 853 MiB. Correctness only; no tuned Linux numbers, CUDA untested.

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

## Conversion: two bottlenecks that were not the SSD

Converting a GGUF to a PGRN sidecar is a one-off cost, but it is minutes long, so I
finally measured its phases instead of assuming. Qwen3.6-35B-A3B Q4_K_XL, 22.85 GB
source on an external SSD, 20.13 GB output on internal NVMe, M3 Pro / 36 GB:

| Phase | Was | Now | Why |
|---|---|---|---|
| `sha256` (hash the source) | 262 MB/s | **726 MB/s** | portable SHA-256 replaced by CommonCrypto (ARMv8 SHA-2) |
| `verify` (CRC sweep) | 318 MB/s | **1188 MB/s** | the sweep now runs on `--io-threads`, like the write phase already did |
| whole conversion | 3 min 43 s | **2 min 13 s** | −40 % |

The hash was the surprise: it took 87 s of a 223 s run at 262 MB/s, while the *write*
phase was reading the same file at over 500 MB/s. So the limit was the CPU, not the
disk — isolated in RAM, the vendored portable SHA-256 does 366 MB/s against
CommonCrypto's 3073 MB/s on this chip. Same algorithm, so the digest and the
byte-parity gate are untouched; the portable path stays as the fallback for non-Apple
targets.

The CRC sweep was the same kind of oversight in the opposite direction: it re-read
every record single-threaded on a device the write phase was already pulling 1.2 GB/s
from. Records are independent, so it parallelises for free — the only care needed is
reporting the *lowest* mismatch rather than the first one noticed, because the resume
path rolls back to exactly what it is told.

| `--io-threads` | sha256 | write | verify | total |
|---|---|---|---|---|
| 1 | 691 MB/s | 212 MB/s | 318 MB/s | 191.3 s |
| 4 | 687 MB/s | 245 MB/s | 1188 MB/s | 132.6 s |

`sha256` staying flat at 687 vs 691 MB/s is the control: it is single-threaded by
design, so the thread count should not move it, and it doesn't. Output SHA is
identical at both settings.

**Resumability, verified on the same model:** two runs, each interrupted twice with
SIGTERM and resumed, produced byte-identical output to an uninterrupted run — and to
the digest recorded when the native converter was first proved byte-equal to the
Python reference. 16/16 converter tests, including one that corrupts two records at
once and checks the rollback lands on the earlier one.

---

## Linux: a second compiler found four bugs, three of which were ours on macOS too

Building the engine for Linux was meant to be a port. It turned into an audit, and
that is the more useful result. The parity number comes after the bugs, because the
bugs are what a single platform cannot show you.

- **Four standard headers** were missing their include (`memcpy`, `std::isfinite`,
  `mkstemp` twice). Apple's libc++ supplies them transitively — invisible here, a
  compile error anywhere else.
- **Admission was memory-blind in a container.** It asked `sysinfo`, which answers
  for the machine, not the cgroup. Capped at 2 GiB it reported the host's 7 GiB: a
  3.5× overestimate in the one mechanism whose whole purpose is refusing early.
- **The page-cache hint was a no-op on Linux, not merely absent.** Every read path
  set `fcntl(F_NOCACHE)`, guarded by an `#ifndef` that defined the Apple-only
  constant to `0` elsewhere. So the call compiled, ran, failed, and changed nothing:
  readahead stayed on for a deliberately random access pattern, and pages the
  streamer will never revisit stayed resident. The premise this project rests on —
  that streaming, not the kernel, decides what is in memory — did not hold there.
- **The expert cache asked for a buffer that cannot hold it.** It took the head of
  the layer's buffer-type list, and the CPU backend puts its *repacking* type there.
  Repacking rewrites a whole tensor when set; streaming writes one expert at an
  offset. The abort (`GGML_ASSERT(offset == 0)`) was the buffer saying so.

Then the gate. `granite-3.0-1b-a400m-instruct` Q4_K_M, a real 32-expert MoE, both
arms on the same Linux CPU backend, 4 threads, greedy, seed 1234, PGRN cache set to
0.25 GiB against 0.69 GiB of experts so misses genuinely happen:

| Arm | Peak RSS | Output |
|---|---|---|
| resident (`-nr`) | 853 MiB | reference |
| **streamed (`-nr`)** | **487 MiB** | **identical** |
| resident, repacking on | 1594 MiB | *differs — see below* |

The same prompt on macOS/Metal, resident and streamed, gives that same text. Four
runs across two platforms and two backends agree, so streaming is delivering the
same bytes, not similar ones.

Both gated arms run with `-nr` (no repacking) on purpose. Repacking changes the
accumulation order of the matmul, which moves greedy output by itself — visible in
the third row. Comparing a repacked resident arm against a plain streamed arm would
have measured the kernel and said nothing about the bytes.

The cost, plainly: streamed experts cannot live in a repacked buffer, so on CPU
their matmuls run the generic kernels. Metal has no repacking, so nothing changes
there. No throughput is reported — the model arrives over virtiofs from a macOS
host, so any tok/s would describe the VM — and CUDA is untested. Gates:
`bench/m2/run_{engine,converter,parity}_gate.sh` (12/12, 16/16 with matching
SHA-256 across platforms, PASS).

---

## What did *not* work (recorded honestly)

- **OS page-cache instead of our bounded arena** — ~75 % faster, but 1000+ swapouts: it breaks "the Mac stays usable." Rejected.
- **Naive coupled-layer prefetch** — 5–15 % *slower* (cache thrash). Rejected.
- **Lossless expert compression / expert-pinning** — low ROI *after* the storage split made fetch cheap; not implemented.
- **ANE / MLX compute paths** — real 1.2–1.4× gains, but only for models that fit in RAM; they don't help the >RAM streaming regime.

Method: `python -m bench.m0d.run_native_streaming_ab` (per-turn decode tok/s, hits/misses, memory). A/B = same model, same cache, change one variable. Parity gate (`ctest -R test-peregrine-model-e2e`) asserts streamed output is bit-identical to resident (NMSE = 0) — it passes.
