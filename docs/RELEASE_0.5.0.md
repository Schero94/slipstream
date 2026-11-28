# Slipstream 0.5.0 — agent-ready: new conversations and restarts in about two seconds

A coding agent sends the same big context (instructions, tool definitions, source files) over and
over. 0.5.0 makes sure the Mac reads it only once: a new conversation with the same agent setup,
and the first request after an app or server restart, now start answering in about two seconds
instead of three minutes. The first long prompt reads about a third faster by default, you can
see its progress, and you can stop it.

All numbers: Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF and PGRN on the internal NVMe,
llama-server with the app's flags, 4 GiB expert cache, ~39.5k-token agent prompt, swap growth
0 MB. Raw data and the experiments that lost are in `bench/RESULTS.md`.

## What changed for you

| Time until the first word | 0.4.1 | 0.5.0 |
|---|---:|---:|
| First long agent prompt (cold) | 183–190 s | **137 s** |
| New conversation, same agent setup (system prompt + tools) | 187 s | **2.1 s** |
| First request after restarting the app or server | 187 s | **2.2 s** |
| Follow-up turn in the same conversation | 0.8 s | 0.8 s |

Every reused answer was checked against a fresh computation of the same prompt: identical
output (same hash).

- **Fast prompt reading is on by default.** The model reads a long prompt layer by layer, so each
  expert is loaded from the SSD once per prompt instead of once per batch. 190 s → 137 s
  (−28 %), same answers. It can now be stopped mid-prompt (stops within ~0.3 s) and reports
  progress while it reads. You can still switch it off in Settings.
- **New conversations reuse the agent setup.** The server already keeps the state at the start of
  each user message; we measured it end to end: a second conversation with the same system
  prompt and tools only reads its new words (33 tokens instead of 39,509).
- **The agent context survives restarts.** Before the server stops (settings change, Quit), the
  app writes the current context to the internal SSD (~1 GB, 0.6 s) and loads it back after the
  next start (0.3 s). Rules: only an idle server is saved, a small request never replaces a big
  agent context, at most two saved contexts are kept, at least 24 GB of disk must stay free, and
  a saved context only loads into exactly the same model, settings and engine build.
- **You see what the engine is doing.** Chat and editor requests (Kilo Code, OpenCode, …) show
  "Reading your prompt 17,400 of 39,517 (44 %) · about 1 min left", then "Writing the answer".
- **Simpler first run: 3 clicks** from a fresh install to a model running and connected to your
  editor (was 5): download, start, set up the editor. Disk space is checked up front (the
  default model needs ~40 GiB), a failed start says why, and a memory check runs before Start.

## Where the time goes now

The first cold read of a 39k-token prompt is close to what this Mac can compute: about 40 % is
attention (it grows with the square of the prompt length and cannot be skipped without
changing answers), and SSD loading is only ~6 s of 137 s. That is why 0.5.0 focuses on never
reading the same prompt twice.

## Fixes

- With several parallel requests, cancelling one prompt no longer aborts the others.
- Backend messages in the English UI are English.

## Tried and rejected

- Loading the prompt's most-used experts last, so they stay in the cache for the answer: no
  effect (−0.4 % / +0.9 % at 4 / 10 GiB). The slower words right after a long prompt come from
  attention over the long context, not from what is cached.
- Prefetching only the top 6 of the 8 predicted experts (Colibrì #2300): 6–8 % slower here — all
  8 are needed, and at 4–10 GiB the extra reads do not push out useful experts.
- Details and raw numbers in `bench/RESULTS.md`.

## Validation

- Engine: 14/14 peregrine tests, including the stop-mid-prompt checks for Fast prompt reading,
  run on the shipped binary. That binary was built at 01:21:38, after the last source change
  (01:21:24). The release gate in `bench/RESULTS.md` ran on this same binary: new conversation
  and restart answers match a fresh computation (hashes `a9f8c4863784` / `c4062b92a557`).
- 183/183 Rust workspace tests, 107/107 Tauri tests, 44/44 Python tests, 7/7 UI contracts,
  headless asset check PASS.
- DMG checksum valid. The mounted app passes `codesign --verify --deep --strict` (ad-hoc
  signature, not notarized) and reports 0.5.0. It contains 476 resource files, all 16 runtime
  components, no Python bytecode cache and no Ollama payload. All five programs inside are
  arm64 (Apple Silicon), and llama-server only uses macOS system libraries.
- We started the bundled llama-server straight from the mounted DMG with the app's flags
  (4 GiB cache) and gave it the short reference prompt. With Fast prompt reading (the new
  default) it writes 10.5 tokens/s, and 10.6 tokens/s with it switched off. Both give the
  reference output (hash `38b5b06864f1`, the same as 0.4.1), with 0 MB swap. The app opens
  from the DMG and quits cleanly: ports 8080/8081 stay free and no server process is left
  running.
- The zip unpacks to an app that is byte-identical to the one in the DMG, and its signature
  checks out.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.5.0_aarch64.dmg` | 69,994,913 | `54107f52a1aa0e515cac5a54b16f7c3dcc425b1aa63e4b21b957bb705be45040` |
| `Slipstream_0.5.0_aarch64.zip` | 68,420,312 | `afbcf10b9bf1148f9ccd95079298f785d1471b922e80fa55448ceae789456781` |
| `slipstream-node_0.5.0_macos_aarch64.tar.gz` | 5,448,405 | `635a5b0dab1fcd35aa6ec2fbbc5808456c4d917370db7d19c78861a494eb2756` |

The node archive reports 0.5.0. It contains the arm64 binary, launchd/systemd/Docker templates,
the deploy README and the community mesh guide, laid out exactly like 0.4.1. The Docker image
has not been rebuilt.

The app is ad-hoc signed and **not notarized**. The first time you open it, right-click the app
and choose Open, or run `xattr -dr com.apple.quarantine Slipstream.app`.
