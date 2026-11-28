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

VALIDATION_PLACEHOLDER
