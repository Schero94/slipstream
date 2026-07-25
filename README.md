<p align="center">
  <img src="docs/icon.png" alt="" width="128" height="128" />
</p>

# Slipstream

![Slipstream: the expert cache streaming live from SSD](docs/hero.gif)

**Run coding models that are bigger than your Mac's RAM, locally and at usable speed.**

Slipstream is a fork of [llama.cpp](https://github.com/ggml-org/llama.cpp) that streams Mixture-of-Experts (MoE) expert weights off your SSD instead of loading the whole model into memory. A 16-36 GB Apple-Silicon Mac can run 35B-480B MoE coding models and stay usable while it does it. Nothing goes to a cloud, there are no API keys, and no data leaves the laptop.

It ships as a Mac app with the engine bundled inside. Download the `.dmg`, drag it to Applications, open it. Nothing to compile, no dependencies to install. Point your AI coding assistant (Kilo Code, Cline, Cursor, OpenCode, anything OpenAI-compatible) at `http://127.0.0.1:8080/v1` and go.

> Not affiliated with Ollama. Slipstream is a llama.cpp/Metal fork with its own SSD expert-streaming layer (PGRN) and a self-contained control app.

---

## Why this exists

A MoE model is enormous on disk but only fires a few experts per token. In a 118B model with 8B active parameters, the overwhelming majority of the weights are idle at any given moment. So Slipstream keeps the always-needed weights resident and streams the routed experts off the SSD as they come up, into a bounded cache sized against your RAM (a per-layer CLOCK-LRU-K arena).

If a requested cache size would starve the system, admission refuses it outright. That check exists because I mmapped a 73 GB file on 36 GB of RAM early on and kernel-panicked the machine. The OS does not save you here.

Models that won't fit in your RAM become runnable. That's the entire pitch.

---

## Get started

1. Download the latest `Slipstream_x.y.z_aarch64.dmg` from [Releases](../../releases).
2. Drag `Slipstream.app` into Applications. The first launch needs right-click → Open, once, because the app isn't notarized yet.
3. Open it. Slipstream detects your Mac and proposes settings for it: cache size, context and I/O threads derived from your RAM and core count. Click "Apply best".
4. Pick a model from the dropdown and download it (with progress), then generate its PGRN sidecar. See *Compatible models* below.
5. Click Start. Once the pill turns green you're serving an OpenAI-compatible API on `127.0.0.1:8080`.
6. In your coding assistant, add an OpenAI Compatible provider. For Kilo and OpenCode there's a one-click patch instead; restart VS Code afterwards.

Everything runs on-device. While you work, the app shows live SSD throughput, cache hit-rate, tokens/sec, token usage and RAM headroom. Two engine features have switches in the settings panel: Compact, which is on by default and is the fastest setting I've measured, and Predictive Prefetch, which is off by default and experimental. The UI is available in English, German, Chinese and Spanish.

### Codebase indexing (optional)

Streamed models are slow at prefill, so the less your assistant sends per request, the better it feels. The app can run the retrieval half of that for you, on-device: it starts a local embedding server, and it can download and start Qdrant as the vector store. Neither ships inside the `.dmg`. The app fetches the Qdrant release binary when you ask it to, and you pick an embedding model the same way you pick a chat model. Once both are up, point your assistant's codebase indexing at them and it will send small, relevant prompts instead of half the repository.

---

## Measured results

Numbers from a 36 GB Apple-Silicon Mac, raw logs in [`bench/RESULTS.md`](bench/RESULTS.md). The experiments that failed are recorded in the same place as the ones that worked. Full write-up: [`BENCHMARKS.md`](BENCHMARKS.md).

### Qwen3.6-35B-A3B (Q4), streamed from the internal NVMe

| Cache | Decode | Cache hit-rate |
|------:|-------:|---------------:|
| 2 GiB | ~5.5 tok/s | ~21% |
| 10 GiB | ~13 tok/s | ~78% |
| 14 GiB | ~19 tok/s | ~86% |

Prefill of a large (~30k-token) coding-agent prompt goes from 75 to 208 tok/s, a 2.7x gain, with `ubatch 2048` and parallel I/O threads.

### Laguna S 2.1 (118B-A8B, Q4), a model that does not fit in 36 GB

| Configuration | Decode | vs. baseline |
|---|---:|---:|
| PGRN on external USB SSD, no draft | 0.72 tok/s | 1.0x |
| PGRN on internal NVMe (storage split) | 1.95 tok/s | 2.7x |
| + DFlash speculative draft | 2.36 tok/s | 3.3x |
| + larger cache | 2.83 tok/s | 3.9x |

The streamed file (PGRN) belongs on your fastest disk. The GGUF gets read once at load and can live anywhere. That storage split was the biggest single lever in the whole project, worth 2.7x on its own, and it involved no code at all.

---

## Compatible models

Slipstream streams experts, so it needs a MoE architecture with Q4_K, Q5_K or Q6_K expert tensors that mainline llama.cpp can read. Dense models have no experts to stream. IQ, Q2, Q3, Q8_0 and MXFP4 expert quants don't work.

Interactive tier, small active parameter counts, the daily drivers:

| Model | Total / Active | Note |
|---|---|---|
| Qwen3.6-35B-A3B | 35B / 3B | recommended, MTP speed |
| Qwen3-30B-A3B | 30B / 3B | no MTP, lighter |
| DeepSeek-V2-Lite | 16B / 2.4B | smallest, lowest RAM |
| GLM-4.5-Air | 106B / 12B | strong quality |
| Laguna S 2.1 | 118B / 8B | DFlash speculative decoding |

XL streaming tier, verified against mainline llama.cpp. These are 240-466 GB at Q4 and need a big fast SSD:

| Model | Total / Active | Note |
|---|---|---|
| Qwen3-Coder-480B | 480B / 35B | coding-focused |
| Llama 4 Maverick | 400B / 17B | fastest decode of the giants |
| DeepSeek V3 | 671B / 37B | general |
| DeepSeek R1 | 671B / 37B | reasoning; thinking tokens slow agent use |
| GLM-5.2 | 744B / 40B | top-tier, and large enough to need a big SSD or a lower quant |

Waiting on mainline llama.cpp support: MiniMax M3 (23B active) and DeepSeek V4-Flash (13B active).

The app's dropdown is seeded with these, but you can point it at any compatible GGUF. Model weights aren't included; you download those from Hugging Face. Slipstream is the engine and the tooling around it.

---

## How it works

A converter pulls the stacked expert tensors out of the GGUF into a streamable binary that sits next to it, the PGRN sidecar. Reads go through `pread` with `F_NOCACHE`, which keeps the OS page cache from ballooning and fighting the RAM budget.

The cache is partitioned by layer, one bounded CLOCK-LRU-K tier each. Cross-layer eviction is impossible by construction, so streaming behaviour stays predictable. Above that sits the memory-health gate, which rejects any cache size that would push the Mac into swap. "The Mac stays usable" is a hard invariant here, not an aspiration.

Compact slots let single-token MoE layers execute straight out of the pinned arena, with no copy between cache and compute buffer. I measured this at a 2 GiB cache, saw nothing and shelved it. Re-measured at the cache sizes the app actually recommends it's worth +13-24%, peaking around +24% at 6 GiB, and the output stays bit-exact. The app enables it by default; on the command line it's `--pgrn-compact-slots`.

Speculative decoding uses MTP on Qwen models and DFlash on Laguna, drafting several tokens per target pass.

Async prefetch is opt-in and still experimental. A background thread warms the next layer's experts while the current layer computes, driven either by a predictor table (`PGCT1`) or an expert-coupling table (`PGCC1`). The machinery works end to end. Getting the *prediction* accurate enough to beat the SSD-fetch wall is unsolved, and `bench/` documents how badly it currently loses.

A parity test asserts that streamed output is bit-identical to fully-resident output, NMSE = 0. Optimizations that only reorder eviction are parity-neutral by construction, which is what makes them safe to ship.

Architecture notes live in [`docs/ARCH.md`](docs/ARCH.md).

---

## Limitations

Speed scales with your SSD and your RAM. More resident cache means a higher hit-rate means faster decode. External USB SSDs are a genuine bottleneck, so keep the streamed PGRN on internal NVMe. Decode is roughly 78-92% bound on SSD fetch.

Big agentic prompts are prefill-heavy. A 30k-token first request against a streamed model takes minutes. Raise `--pgrn-io-threads` and turn on codebase indexing (see above) so your assistant sends small, relevant prompts; later turns reuse the KV cache and come back quickly.

118B on 36 GB runs at about 2.8 tok/s. That's batch work. It is not chat, and I'd rather say so here than have you find out after the download. The 35B at ~13-19 tok/s, depending on cache size, is the interactive one.

The GGUF to PGRN converter still needs the Python tooling in `bench/`. Bundling it into the app is on the list; the engine itself is already fully bundled.

The app isn't notarized, so the first launch needs right-click → Open, or `xattr -dr com.apple.quarantine Slipstream.app`.

---

## Repository layout

```
engine/    new source files (PGRN streaming, prefetch, admission, arena, …), path-preserving and browsable
patches/   slipstream-seams.patch, the changes to upstream llama.cpp files
apply.sh   clones ggml-org/llama.cpp @ pinned commit, drops in engine/, applies the patch
app/       the Tauri 2 control app (dist/ frontend + src-tauri/ Rust backend)
bench/     benchmark methodology and recorded results (RESULTS.md)
docs/      architecture notes
```

## Building from source

Requires Xcode command-line tools, CMake, and Rust with the Tauri CLI.

```bash
# 1. Reconstruct the engine (upstream llama.cpp @ pin + the Slipstream changes)
./apply.sh                    # -> ./llama.cpp-slipstream

# 2. Build a self-contained, Metal-embedded server binary (no external deps)
cd llama.cpp-slipstream
cmake -B build-static -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
      -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF -DGGML_METAL_EMBED_LIBRARY=ON \
      -DCMAKE_OSX_ARCHITECTURES=arm64
cmake --build build-static --target llama-server -j
cd ..

# 3. Bundle it into the app and build the .dmg
cp llama.cpp-slipstream/build-static/bin/llama-server app/src-tauri/resources/llama-server
cd app/src-tauri && cargo tauri build   # -> the self-contained .dmg
```

`otool -L build-static/bin/llama-server` should list only system frameworks. No `@rpath`, no Homebrew, no OpenSSL. That's what makes the app copy-and-run on any Apple-Silicon Mac.

## Running the engine directly

The app sets all of this for you. If you're running `llama-server` yourself, this is the part that isn't upstream llama.cpp:

```bash
./llama-server -m model.gguf \
  --pgrn model.pgrn \
  --pgrn-cache-gb 10 \
  --pgrn-headroom-gb 6 \
  --pgrn-io-threads 8 \
  --pgrn-compact-slots
```

| Flag | What it does |
|---|---|
| `--pgrn FILE` | Stream experts from this PGRN sidecar. Disables mmap of the model. |
| `--pgrn-cache-gb N` | Hard upper bound in GiB on the resident expert cache. Required with `--pgrn`. |
| `--pgrn-headroom-gb N` | RAM in GiB kept free for macOS and whatever else you're running. Required with `--pgrn`. |
| `--pgrn-io-threads N` | Parallel cold-read threads per layer stream, 1 to 64. This is the flag that moves prefill: 8 to 16 took a 30k-token prompt from 75 to 208 tok/s. |
| `--pgrn-compact-slots` | Run single-token MoE layers straight from pinned arena slots. Worth +13-24%. |
| `--pgrn-predict FILE` | PGCT1 hot-set table for speculative next-layer prefetch. Warms the cache only, never changes output. |
| `--pgrn-coupling FILE` | PGCC1 table, conditioned on the experts the current layer fired. Takes precedence over `--pgrn-predict`. |
| `--pgrn-hot-percent N` | Share of the cache reserved for HOT experts. 0 keeps pure CLOCK-LRU-K, which measured best. |
| `--pgrn-ane-draft MANIFEST` | Fail-closed Core ML one-shot draft. Every candidate is verified against the target model. |
| `--pgrn-ane-budget-mib N` | Memory ceiling in MiB for that Core ML draft. |

There are further HOT/WARM tuning flags (`--pgrn-promote-hits`, `--pgrn-demote-idle`, `--pgrn-hot-cooldown`). The tier reservation measured between -1% and -6%, so the default of 0 is also the best setting I found. `llama-server --help` lists them all.

Two environment variables cover the multi-disk case:

| Variable | What it does |
|---|---|
| `PGRN_MIRROR=/path/to/copy.pgrn` | A byte-identical PGRN copy on a second SSD. Reads get striped across both, split in proportion to each disk's probed cold-read bandwidth. |
| `PGRN_MIRROR_WEIGHT=N` | Override that split, as a percentage sent to the primary disk. |
| `PGRN_BUFFERED=1` | Skip `F_NOCACHE` and let the OS page cache back the reads. Faster per read, but the cache is then unbounded. See BENCHMARKS.md before using it. |

Striping is a capacity feature first. On an internal NVMe paired with a slow USB drive it comes out slower, because they share a bus.

---

## Acknowledgements

Slipstream was inspired by [Colibrì](https://github.com/JustVugg/colibri) by JustVugg, the project that showed a 700B-scale MoE model streaming from disk on consumer hardware. Colibrì is pure C on CPU and CUDA; Slipstream takes the idea to Apple Silicon (Metal, unified memory) with a native app and its own PGRN engine. The disk-benchmarking methodology, the route-trace and expert-coupling analysis, and the RAM admission idea all came from reading it. Thank you. 🐦

---

## License & attribution

The Slipstream additions, meaning the PGRN expert-streaming layer (`engine/`), the seams patch and the control app (`app/`), are released under the MIT License. See [`LICENSE`](LICENSE).

Slipstream is built on llama.cpp (MIT, © the ggml authors). The upstream sources are not vendored here: `apply.sh` fetches them from the pinned commit and retains their original license. Model weights belong to their respective creators and are not distributed here.

---

*Performance figures are hypotheses until they're recorded in `bench/RESULTS.md`.*
