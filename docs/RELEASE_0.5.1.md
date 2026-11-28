# Slipstream 0.5.1 — real agents start new conversations in under two seconds

0.5.0 showed that a new conversation can reuse the agent's setup. With the request shape real
coding agents send (OpenCode, Kilo Code: a system prompt with the date and working directory, a
list of tool definitions, files arriving as tool results), one short side request — like the
title an agent asks for — made the server throw that saved setup away. 0.5.1 keeps it, moves the
engine to the newest llama.cpp, and fixes forced tool calls.

All numbers: Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF and PGRN on the internal NVMe,
llama-server with the app's flags, 4 GiB expert cache, swap growth 0 MB. Raw data in
`bench/RESULTS.md`.

## What changed for you

| | 0.5.0 | 0.5.1 |
|---|---:|---:|
| New conversation in a real agent (same system prompt + 12 tools), after a side request | 18.3 s | **1.6 s** |
| Decode speed at 10 GiB cache | 18.2 tok/s | **19.1 tok/s** (+4.7 %) |
| Decode speed at 4 GiB cache | 10.5 tok/s | 10.5 tok/s |

Answers are identical to before (same output hashes in every check).

- **The agent's setup survives side requests.** The server keeps recent prompts in memory. Its
  rule was never to load a big saved prompt when only a small part of it fits, because loading
  used to remove it from memory. Slipstream now loads a copy and keeps the original, so a new
  conversation only reads its own words (26 tokens instead of 2,745). Known cost: on the day the
  date in the agent's system prompt changes, the first new conversation takes ~22 s instead of
  ~12 s. On Macs with less than 32 GB the prompt memory (1 GB) holds only one long context, so
  this helps less there.
- **Newest llama.cpp.** The engine now builds on upstream llama.cpp from 27 Nov 2026
  (`8e23278c7c36`, 106 commits newer). One new upstream Metal code path made decoding 3–5 %
  slower here; it is switched off (`GGML_METAL_CHUNK_COMMIT=1` turns it back on).
- **Forced tool calls work again.** When an app asks for one specific tool
  (`tool_choice: {"type":"function", ...}`), the model calls exactly that tool. The new upstream
  code had lost this for Qwen3.6.

## Validation

- Engine: 14/14 peregrine tests on the shipped build. The shipped llama-server was built at
  04:09:37, after the last source change (04:09:32), and pgrn-convert was relinked at 04:21:17;
  both only use macOS system libraries. A fresh `apply.sh` reproduces the engine source exactly
  (upstream `8e23278c7c36` + Slipstream files + patch). The release gate in `bench/RESULTS.md`
  ran on this same llama-server: a new agent conversation after a side request starts in 1.6 s,
  a restart restores the saved context in 0.19 s, and the answers match the earlier runs
  (hashes `86b8ff31ee95` / `c4062b92a557`). Forced tool calls pick the requested tool again.
- 183/183 Rust workspace tests, 107/107 Tauri tests, 44/44 Python tests, 7/7 UI contracts,
  headless asset check PASS.
- DMG checksum valid. The mounted app passes `codesign --verify --deep --strict` (ad-hoc
  signature, not notarized) and reports 0.5.1. It contains 476 resource files, all 16 runtime
  components, no Python bytecode cache and no Ollama payload. All five programs inside are
  arm64 (Apple Silicon), and llama-server only uses macOS system libraries.
- We started the bundled llama-server straight from the mounted DMG with the app's flags
  (4 GiB cache) and gave it the short reference prompt. With Fast prompt reading (the default)
  it writes 10.4 tokens/s, and 10.5 tokens/s with it switched off. Both give the reference
  output (hash `38b5b06864f1`, the same as 0.5.0 and 0.4.1), with 0 MB swap. The app opens
  from the DMG and quits cleanly: ports 8080/8081 stay free and no server process is left
  running.
- The zip unpacks to an app that is byte-identical to the one in the DMG, and its signature
  checks out.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.5.1_aarch64.dmg` | 70,103,645 | `af907c5feffc9d3b835bea2d459aafd95efbfea0d19a3ffd4f00490f999de1a8` |
| `Slipstream_0.5.1_aarch64.zip` | 68,563,585 | `cf9bc9bf00a2258a2c25580ce3a2d12900c377f54ca28b72374ec2259108c709` |
| `slipstream-node_0.5.1_macos_aarch64.tar.gz` | 5,448,723 | `7dbf50fcb1f184d6a8b30631431ffd2383050e8a1a06315fedb7bcbff69af3f8` |

The node archive reports 0.5.1. It contains the arm64 binary, launchd/systemd/Docker templates,
the deploy README and the community mesh guide, laid out exactly like 0.5.0. The Docker image
has not been rebuilt.

The app is ad-hoc signed and **not notarized**. The first time you open it, right-click the app
and choose Open, or run `xattr -dr com.apple.quarantine Slipstream.app`.
