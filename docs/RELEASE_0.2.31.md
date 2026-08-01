# Slipstream 0.2.31

**This is the canonical product repo.** Do not ship Slipstream releases to `Schero94/LLM-BOOM` (dev monorepo). Earlier mistaken tags `v0.2.29` / `v0.2.30` on LLM-BOOM are not authoritative.

## Highlights
- **Stop guarantee:** owned-PID SIGTERM first (Metal `llama-server` + MLX `run_omlx_pgrn` / lock holder), lockfile cleanup, no broad `pkill -f omlx-server`.
- **Paths:** Start defaults to internal `~/Modelle` (MLX at `~/Modelle/mlx`). External volumes (e.g. Crucial) are Advanced overflow only — sticky `/Volumes` / Crucial roots are unstuck on launch.
- **i18n:** EN + DE full key parity across Chat, Models, Downloads, Benchmarks, Live, Logs, Settings, Cluster.
- **Qwen 3.6:** verified Metal+PGRN Start → stream → Stop on internal SSD (see monorepo artifact `QWEN36_INTERNAL_SMOKE_*` / install from this tag).

## Storage policy
1. Active = internal `~/Modelle`
2. When internal would fill → copy/move to external, delete internal copy, free room for the new model
3. Never sticky-default Crucial for Start

## Next (not this release)
- DeepSeek V4 Flash experiment (Metal+PGRN, real Q4_K experts — not MXFP4)
- Model-agnostic discovery under `~/Modelle` (Auto backend, less per-model hardcoding)

## Build
Stage `resources/llama-server` (+ optional `omlx-pgrn`) then `cargo tauri build` from `app/src-tauri`, or install the release binary into `/Applications/Slipstream.app`.
