# Slipstream 0.4.1 — agents answer follow-ups in under a second

Coding agents send the same large context again and again. 0.4.1 keeps that context in
memory across interleaved requests, so a follow-up turn on a ~40k-token context starts
answering in 0.8 s instead of almost four minutes. The first turn gets faster too, every
Mixture-of-Experts model now gets lookahead prefetch, and two new model families run.

All numbers: Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF and PGRN on the internal NVMe,
llama-server with the app's flags, 39,515-token agent prompt, swap growth 0 MB. Raw data and
the experiments that lost are in `bench/RESULTS.md`.

## What changed for you

| | 0.4.0 | 0.4.1 |
|---|---:|---:|
| Agent follow-up turn after an interleaved request | 229 s | **0.8 s** |
| First answer, 39.5k-token prompt, 4 GiB cache | 235 s | **183 s** (default) |
| First answer with "Fast prompt reading" (opt-in), 4 GiB | 235 s | **~154 s** |

Measured in the app's own server setup (`-fa on`, prompt cache, checkpoints), interleaved,
two runs each. Without those (bare server) Fast prompt reading reaches 134 s.

- **Follow-up turns.** The prompt cache now holds 2 GiB on Macs with 32 GB or more and 1 GiB
  below (was 512 MiB, which could not hold any context above ~22k tokens, so a title or
  commit-message request threw the whole context away). The reused answer is byte-identical
  to a fresh prefill of the same prompt.
- **First answer.** Streamed models read prompts in 4096-token batches, halving the expert
  re-reads (−21 %, +340 MB peak RAM).
- **Fast prompt reading (layer-major prefill), opt-in.** Runs every layer for the whole prompt
  before the next layer, so each expert is read from the SSD once per prompt instead of once
  per batch (SSD reads in the prompt: 102 s → 6 s; first answer −16 % on top of the new
  default). Output is bit-identical, and follow-up turns still come from the prompt cache. It is off by
  default while the remaining safety work lands: a long prompt cannot be stopped mid-read yet.
- **Lookahead for every MoE model.** Lookahead prefetch moved into the shared MoE code, so it
  now works for all architectures, not only Qwen3.6, with two guards that switch it off where
  it cannot help (very small or fully resident caches).
- **New models.** The PGRN converter accepts Q4_0/Q4_1/Q5_0/Q5_1/Q8_0 experts, which models use
  when their expert width is not a multiple of 256. Verified on this Mac:
  - LFM2.5-24B-A2B (Liquid AI): streams losslessly; lookahead +20 % at 8 GiB.
  - Mellum2.1-12B-A2.5B (JetBrains, coding): streams losslessly.

## Fixes

- The server aborted on JSON-schema requests when grammar-forced drafts were combined with a
  speculative mode that had not drafted itself.
- Lookahead was silently inactive for multi-token batches (MTP verify) and for architectures
  without compact slots.
- The app's cache recommendation now reserves room for the prompt cache and the larger prefill
  buffer, which RAM admission does not account for.

## Dependencies

All Rust dependencies refreshed (Tauri 2.12.7, tokio, quinn and ~70 crates), plus libp2p 0.57,
ureq 3, mdns-sd 0.22 and base64 0.23. llama.cpp stays at upstream `33aeb17f8586`.

## Tried and rejected

Cold insertion of prompt reads (neutral), grammar-forced drafts without a draft model
(−7 to −11 %), parallel reads and zero-copy slots for small batches (neutral), n-gram
speculation (−9 % on a rewrite). Details in `bench/RESULTS.md`.

## Validation

_Filled in after the release build._

## Release artifacts

_Filled in after the release build._
