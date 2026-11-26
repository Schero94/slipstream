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

- Engine: 14/14 peregrine tests on a fresh build (binary newer than every source), incl. the
  layer-major fixture with four trunk layers, non-empty-memory and serialized-state parity on
  CPU and Metal. Qwen3.6 greedy output unchanged (hash `e6c6a36552db`).
- 183/183 Rust workspace tests, 96/96 Tauri tests, 44/44 Python tests, 7/7 UI contracts,
  headless asset check PASS.
- DMG checksum valid; the mounted app passes `codesign --verify --deep --strict` (ad-hoc, not
  notarized), reports 0.4.1, contains 476 resource files, all 16 runtime components, no Python
  bytecode cache, no Ollama payload; all binaries arm64; llama-server links system frameworks only.
- The bundled llama-server, started from the mounted DMG with the app's flags, decodes the
  reference prompt at 9.9 tok/s with the reference output hash, with and without Fast prompt
  reading, 0 MB swap. The app launches from the DMG and quits cleanly; ports 8080/8081 stay free.
- Prompt reuse: a follow-up served from the prompt cache is byte-identical to a fresh prefill.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.4.1_aarch64.dmg` | 69,996,389 | `dbfc5a6be8b55071b383893ebf88bf829b15565ac9bc8958aba0d6de7a6811ba` |
| `Slipstream_0.4.1_aarch64.zip` | 68,379,544 | `5ed8a886dc2721ed2f282366a22d9e259ee1f8930e80d3e43eb79c808bb2cc71` |
| `slipstream-node_0.4.1_macos_aarch64.tar.gz` | 5,449,311 | `29f31fb3135d708773ee59251157ebfdedc35b96b139ba1025b06795549dd373` |

The node archive reports 0.4.1 and contains the arm64 binary, launchd/systemd/Docker templates,
the deploy README and the community mesh guide. It is ~1.1 MB larger than 0.4.0 because
libp2p 0.57 links aws-lc-rs; the Docker image has not been rebuilt with it yet.

The app is ad-hoc signed, **not notarized**: the first launch needs right-click → Open, or
`xattr -dr com.apple.quarantine Slipstream.app`.
