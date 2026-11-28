# Slipstream 0.5.2 — a faster first answer, and a smarter cache on smaller Macs

Two engine improvements that change nothing about the answers: the first long prompt is read
about 5 seconds faster, and Macs that can only give the model a small memory cache keep the
right parts of the model in it more often. Plus screenshots of the app in the README.

All numbers: Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF and PGRN on the internal NVMe,
llama-server with the app's flags (no MTP for streamed runs), swap growth 0 MB. Raw data in
`bench/RESULTS.md`.

## What changed for you

| | 0.5.1 | 0.5.2 |
|---|---:|---:|
| First long agent prompt (39k tokens, 4 GiB cache, app chat path) | 136 s | **131 s** |
| Writing speed, 4 GiB cache | 9.7 tok/s | **9.9 tok/s** (+2 %) |
| Writing speed, 6 GiB cache | 12.1 tok/s | **12.6 tok/s** (+3.5 %) |
| Writing speed, 10–14 GiB cache | unchanged | unchanged |

Answers are bit-for-bit the same as before.

- **Faster first answer.** After reading a prompt, only the last word's result is needed to
  start answering. The model's last layer now skips the attention work for all the other words
  (it still stores what later turns need). That saves about 5 seconds on a 39k-token prompt. A
  reviewer found that the first version changed the result in the last decimal places on real
  prompt sizes; the shipped version keeps the same computation kernel and was checked
  bit-for-bit on 248,320 output values.
- **A smarter cache when memory is tight.** When the model's expert cache is small (up to about
  8 GiB for the 35B model), Slipstream now keeps the experts that are used most often and stops
  the look-ahead loader from throwing out experts it has just fetched. That means fewer SSD reads
  and 2–3.5 % faster writing on 16–24 GB Macs. With bigger caches it makes no difference, so it
  only switches on below that size.
- **Screenshots** of the app in the README and on the website.

## Under the hood

- Our benchmark script measured with MTP speculation on, which the app does not use for
  streamed models; it now matches the app. Earlier rows are marked in `bench/RESULTS.md`.
- Tried and dropped: freeing the look-ahead loader from old pins ("kick-free", no gain without
  MTP).

## Validation

- Engine: 14/14 peregrine tests on the shipped build. The shipped llama-server was built at
  10:57:24, after the last source change (10:00:44). pgrn-convert (04:21:17, unchanged since 0.5.1)
  is newer than all of its own sources (04:01:35), so it was not rebuilt. Both only use macOS system
  libraries. A fresh `apply.sh` reproduces the engine source exactly (upstream `8e23278c7c36` +
  Slipstream files + patch). The release gate in `bench/RESULTS.md` ran on this same llama-server
  file: the first 39k-token agent prompt took 131.5 s (0.5.1: 135.9 s), a new agent conversation
  after a side request started in 1.4 s, a restart restored the saved context in 0.19 s, and the
  answers match the earlier runs (hashes `a9f8c4863784` / `86b8ff31ee95` / `c4062b92a557`). Forced
  tool calls still pick the requested tool.
- 183/183 Rust workspace tests, 108/108 Tauri tests, 44/44 Python tests, 7/7 UI contracts,
  headless asset check PASS.
- DMG checksum valid. The mounted app passes `codesign --verify --deep --strict` (ad-hoc
  signature, not notarized) and reports 0.5.2. It contains 476 resource files, all 16 runtime
  components, no Python bytecode cache and no Ollama payload. All five programs inside are
  arm64 (Apple Silicon), and llama-server only uses macOS system libraries.
- We started the bundled llama-server straight from the mounted DMG with the app's flags
  (4 GiB cache, so the app also switches on the smarter cache, `PGRN_EVICT=decay`) and gave it
  the short reference prompt. With Fast prompt reading (the default) it writes 10.9 tokens/s, and
  11.0 tokens/s with it switched off. Both give the reference output (hash `38b5b06864f1`, the
  same as 0.5.1, 0.5.0 and 0.4.1), with 0 MB swap. This is a quick check that the shipped program
  works, one run each, not a speed benchmark (the speed table above comes from the benchmark runs).
  The app opens from the DMG and quits cleanly: ports 8080/8081 stay free and no server process
  is left running.
- The zip unpacks to an app that is byte-identical to the one in the DMG, and its signature
  checks out.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.5.2_aarch64.dmg` | 70,106,602 | `b57d869832186a8268f2f8511a5eafa5c9b82a249e381a9f7b95e6a4c87a710d` |
| `Slipstream_0.5.2_aarch64.zip` | 68,567,547 | `6446025a3ae5260b2d7bc1b62546506ed445f8876dcbf0e87a3eb59a4ab000ef` |
| `slipstream-node_0.5.2_macos_aarch64.tar.gz` | 5,448,988 | `e253b6ca8d9db2027ef398e3c82e1d107a818b4fe3b24924f736606254a5e50b` |

The node archive reports 0.5.2. It contains the arm64 binary, launchd/systemd/Docker templates,
the deploy README and the community mesh guide, laid out exactly like 0.5.1. The Docker image
has not been rebuilt.

The app is ad-hoc signed and **not notarized**. The first time you open it, right-click the app
and choose Open, or run `xattr -dr com.apple.quarantine Slipstream.app`.
