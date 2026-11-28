# Slipstream 0.5.3 — a faster coding model for smaller caches

The app's model list now offers **Groot-19B-A3B** (October 2026), a slimmed-down Qwen3.6 made for
coding agents. On a Mac that can only give the model a small memory cache it writes about 60 %
faster than Qwen3.6-35B, and it needs about half the disk space. The engine is unchanged from 0.5.2.

Measured on an M3 Pro / 36 GB, GGUF and PGRN on the internal NVMe, llama-server with the app's
flags, swap growth 0 MB (raw data in `bench/RESULTS.md`):

| Expert cache | Groot-19B-A3B | Qwen3.6-35B-A3B (for comparison) |
|---|---:|---:|
| 4 GiB | **~17.6 tok/s** | ~10.9 tok/s |
| 8 GiB | **~29 tok/s** | ~15 tok/s |
| Disk incl. sidecar | ~22 GB | ~40 GB |

- Same answers with every engine setting (lookahead on/off, cache size, cache policy).
- Tool calls and forced tool calls work like Qwen3.6.
- Its coding quality claims (Terminal-Bench 2.1: 52.1 vs 43.8 for Qwen3.6-35B) come from the
  model's authors; we checked a few coding answers by hand, not a benchmark.
- It runs without MTP (MTP made it 24 % slower), which the app sets automatically.
- Qwen3.6-35B stays the default.

Also: the app's compatibility note now lists the newer expert formats (Q4_0/Q5_0/Q8_0) and model
families (LFM2.5, Mellum, Groot), and says which models cannot be streamed yet (merged gate+up
expert tensors, e.g. Inkling-2).

## Validation

- Engine: unchanged from 0.5.2, and the same files ship. llama-server (built 10:57:24) and
  pgrn-convert (04:21:17) are both newer than every source file they are built from (newest
  change 10:00:44), a dry-run build finds nothing to rebuild, and a fresh `apply.sh` reproduces the
  engine source exactly. The checksums are the same as in 0.5.2. 14/14 peregrine tests pass on this
  build, and both programs only use macOS system libraries.
- 183/183 Rust workspace tests, 108/108 Tauri tests, 44/44 Python tests, 7/7 UI contracts,
  headless asset check PASS.
- DMG checksum valid. The mounted app passes `codesign --verify --deep --strict` (ad-hoc
  signature, not notarized) and reports 0.5.3. It contains 476 resource files, all 16 runtime
  components, no Python bytecode cache and no Ollama payload. All five programs inside are
  arm64 (Apple Silicon), and llama-server only uses macOS system libraries.
- The app's built-in page really contains the new model: we unpacked the app script embedded in
  the shipped app and it is identical to `app/dist/app.js`, including the Groot-19B-A3B entry and
  its English and German notes.
- We started the bundled llama-server straight from the mounted DMG with the app's flags at a
  4 GiB cache (Fast prompt reading on, the smarter cache `PGRN_EVICT=decay` on, no MTP) and gave
  it the short reference prompt:
  - Qwen3.6-35B-A3B writes 10.5 tokens/s and gives the reference output (hash `38b5b06864f1`,
    the same as 0.5.2, 0.5.1, 0.5.0 and 0.4.1).
  - Groot-19B-A3B writes 17.1 tokens/s and gives the same output as in our model test (hash
    `3a83d0323170`).
  - 0 MB swap in both. This is a quick check that the shipped program works, one run each, not a
    speed benchmark (the speed table above comes from the benchmark runs).
- The app opens from the DMG and quits cleanly: ports 8080/8081 stay free and no server process
  is left running.
- The zip unpacks to an app that is byte-identical to the one in the DMG, and its signature
  checks out.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.5.3_aarch64.dmg` | 70,100,416 | `0b9d3ddb9855e081ddb31ffcf42a38381e134f3ce20ed7fc402a155906b88fc4` |
| `Slipstream_0.5.3_aarch64.zip` | 68,562,551 | `eef0d982b275751607cd76202a2565fc5fb067e11dd79310450bd3b2627299d9` |
| `slipstream-node_0.5.3_macos_aarch64.tar.gz` | 5,452,070 | `17b9e4cfcf40efa827a07d8d5a19a17f70a99999a8e528d5dc56287c57fc4130` |

The node archive reports 0.5.3. It contains the arm64 binary, launchd/systemd/Docker templates,
the deploy README and the community mesh guide, laid out exactly like 0.5.2. The Docker image
has not been rebuilt.

The app is ad-hoc signed and **not notarized**. The first time you open it, right-click the app
and choose Open, or run `xattr -dr com.apple.quarantine Slipstream.app`.
