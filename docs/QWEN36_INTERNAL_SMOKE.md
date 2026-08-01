# Qwen 3.6 internal Metal smoke — 2026-08-01

**Canonical product repo:** https://github.com/Schero94/slipstream (not LLM-BOOM).

## Path / storage policy
- Active Start path: real internal tree `~/Modelle/qwen3.6-35b-a3b-q4` (not a symlink).
- Removed primary-looking Crucial symlinks under `~/Modelle` / `~/Modelle/mlx`.
- Crucial data **kept** as overflow archive (`/Volumes/Crucial X10/Modelle/…` still present).
- Copied stream essentials to internal: UD-Q4_K_XL `.gguf` (21 GiB) + `.pgrn` (19 GiB) + manifest + partition-weights.
- App unsticks sticky `/Volumes` / Crucial `modelsRoot` / `mlxDir` so Start defaults to `~/Modelle`.

## Memory policy (this run)
| Phase | free+inactive | swap used | notes |
|------|----------------|-----------|-------|
| before | **17.19 GiB** | 1383 MiB | floor ≥2 GiB OK |
| ready | **12.12 GiB** | 1383 MiB | cache=6 · headroom=3 · ctx=4096 |
| after chat | **12.05 GiB** | 1383 MiB | no swap climb |
| after Stop | **22.40 GiB** | 1383 MiB | RAM returned |

Hard floor 2 GiB never approached. No dual-serve. Residency: Metal PGRN (product path).

## Live Metal Start → stream → Stop
- Binary: `/Applications/Slipstream.app/Contents/Resources/resources/llama-server`
- Owned PID: `47225`
- Health 200 after load (~57 s)
- Chat: `max_tokens=64`, temp=0, reasoning off
- Engine log (authoritative):
  - Prefill: **9.64 tok/s** (23 tokens)
  - Decode: **6.24 tok/s** (64 tokens)
  - Wall: ~12.6 s total gen
  - PGRN: cache 6.0 GiB, high-water 6.1 GiB, **hit-rate 72.10%**, width-weighted partition active
- RSS while running: **~10.1 GiB**
- **Stop:** SIGTERM owned PID → process gone, port free, free+inactive 12→22 GiB — **PASS**
- oMLX lock: not created (Metal-only) — N/A

Note: smoke script initially failed to parse the HTTP JSON due to a heredoc/stdin bug; the server log confirms the completion ran. Script fixed for next runs.

## MLX
- Not run this pass: no real `~/Modelle/mlx/Qwen…` tree on internal (symlink removed; 36 GiB copy would clog remaining disk).
- Overflow on Crucial remains available under Advanced — not sticky Start.
- Next: copy MLX twin when internal has room, or overflow policy move.

## DeepSeek V4 Flash
- **Later** (see `docs/pgrn-mlx/artifacts/DEEPSEEK_V4_FLASH.md`). Not downloaded/converted this pass.

## Model-agnostic (sketch)
- One Models folder `~/Modelle`, discover GGUF+PGRN / MLX+experts.pgrn under it, Auto backend.
- Overflow-to-external only when internal would fill. No per-model Crucial hardcoding.

## Contracts run
All `apps/peregrine-control/scripts/test_*.mjs` OK including `test_i18n_coverage.mjs` (EN=DE=464 keys) and `test_path_policy.mjs`.
Rust: `cargo test teardown::` — 5/5 OK.

## App
- Version **0.2.31** (Stop teardown + path unstick + i18n).
- Installed release binary to `/Applications/Slipstream.app`.
