# Slipstream Native Runtime Capsule Design

**Date:** 2026-08-02
**Status:** Approved by the user's autonomous goal-loop instruction

## Objective

Ship Slipstream as a self-contained local LLM platform built on the project's
own llama.cpp/PGRN engine, with oMLX/PGRN as an Apple-Silicon backend that obeys
the same product contract. Models should start from a deliberate internal-NVMe
layout, stream tokens immediately, remain inside bounded memory, and stop
without leaving processes, ports, wired memory, or swap pressure behind.

Slipstream does not bundle Ollama, emulate Ollama's API, use its model store, or
present itself as affiliated with Ollama.

## Product architecture

### 1. llama.cpp/PGRN is the canonical engine

The bundled `llama-server` and `pgrn-convert` binaries define the canonical
model, API, lifecycle, safety, and measurement behavior on macOS and Linux.
The public interface remains OpenAI-compatible `/v1/*` streaming. The headless
server and Mac app must use the same launch planner and safety rules.

### 2. oMLX/PGRN has contract parity, not product ownership

On Apple Silicon, oMLX/PGRN may serve an MLX model when it passes the same
preflight, streaming, quality, RAM, swap, stop, and metrics gates as
llama.cpp/PGRN. Backend-specific options remain internal. Users select a model
and operating profile; Slipstream explains which qualified engine is active.

Linux advertises only llama.cpp/PGRN. It never claims oMLX support.

### 3. Reproducible runtime capsule

A versioned manifest records the expected llama server, converter, oMLX source
overlay, native PGRN library, Python/MLX package lock, architecture, and hashes.
A preflight command reports each component as `bundled`, `installed`,
`repairable`, or `unavailable`.

The release build fails closed when the canonical llama components are absent.
For oMLX, the public app carries the audited fork, PGRN overlay, launcher, and
native dylib. The Python/MLX layer is installed atomically from a reproducible,
hash-locked runtime capsule; a pinned network bootstrap remains a repair path,
not an unverified developer-environment dependency. A partial installation
never receives the `READY` marker.

### 4. Unified model registry and storage planner

One logical model can have:

- a GGUF plus `.pgrn` variant for llama.cpp/PGRN;
- an MLX safetensors plus `experts.pgrn` variant for oMLX/PGRN.

The registry records real paths, resolved paths, file identity, size, device,
filesystem, internal/external classification, symlink state, and hashes of
small manifests. Continuously read PGRN expert data must prefer the internal
NVMe. GGUF or resident weight data that is read once at load may live on a
configured external overflow disk.

The planner rejects downloads, copies, or conversions that would cross a disk
reserve. On the current 36-GiB Mac the internal container has only about
13.9 GB free and already contains the 36-GB oMLX Qwen model, including a 17-GB
`experts.pgrn`. The first loop validates that existing internal path rather than
copying another large model.

### 5. Safe lifecycle and streaming

Before Start, Slipstream verifies:

- exactly one owned heavy server;
- model and PGRN files are complete and on the claimed devices;
- the chosen cache plus at least 3 GiB reserve fits current memory;
- free plus inactive memory stays above the critical start floor;
- bounded PGRN mode is active and OS-page-cache mode is off;
- the port is free or owned by Slipstream;
- the selected backend exposes the required `/health`, `/v1/models`, and
  `/v1/chat/completions` behavior.

Streaming must be incremental from engine to client. The validation records
time to first byte, time to first token, inter-chunk gaps, prefill speed, decode
speed, PGRN hit rate, high-water bytes, RSS, free memory, pageouts, swap-ins,
swapouts, output hash, Stop latency, port release, and memory reclaim.

Failed gates stop only the owned process and record the failure. `mlock`, dual
heavy servers, page-cache-backed PGRN, memory-guard bypass, predictive prefetch,
and experimental vendor changes remain explicit A/B-only options.

## Autonomous optimization loop

Every cycle follows the same protocol:

1. Capture baseline commit, binary/model hashes, storage placement, RAM, swap,
   and operating profile.
2. Run one warm-up and at least three deterministic measured requests.
3. Require normalized output parity for parity-preserving changes.
4. Reject quality, TTFT, decode, RSS, pageout, swap, or reclaim regressions
   outside the predeclared gate.
5. Append machine-readable evidence and an honest summary to
   `bench/RESULTS.md`.
6. Promote one passing change, commit it, and select the next measured
   bottleneck.

Mocks prove orchestration and failure behavior only. Speed and stability claims
require a real model.

## First release-increment acceptance criteria

- A clean macOS installation reports and verifies every bundled llama/PGRN and
  oMLX/PGRN component without relying on `/Applications/oMLX.app`.
- The existing internal Qwen3.6 oMLX model completes Start, health, incremental
  chat streaming, Stop, port release, and memory reclaim with no swapout growth.
- The qualified llama.cpp/PGRN model completes the equivalent real-model gate.
- Both backends expose the same required OpenAI streaming contract and model
  alias behavior.
- The UI clearly distinguishes internal streamed PGRN data from external
  load-once overflow data and refuses unsafe disk operations.
- macOS app, headless node, runtime contracts, API parity, packaging, and real
  browser tests pass.

## Explicit non-goals

- Bundling or integrating Ollama.
- Copying another large model to the nearly full internal SSD.
- Claiming Linux GPU performance before a real CUDA/Vulkan qualification.
- Enabling experimental memory or prefetch controls by default.
