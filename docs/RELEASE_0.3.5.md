# Slipstream 0.3.5 — dense Metal catalog (Dolphin3-Cyber-8B)

Adds a **dense** (non-MoE) Metal path so small full-load GGUFs can Start without a PGRN sidecar, and catalogs **Dolphin3-Cyber-8B**.

## Why

Slipstream’s Metal start previously **required** a `.pgrn` convert. That is correct for MoE expert streaming (Qwen 35B daily driver) and wrong for dense Llama-class models: there are no experts to stream.

## Changes

- Catalog: `dolphin3-cyber-8b` → `RavichandranJ/Dolphin3-Cyber-8B-GGUF` Q4_K_M (optional Q5_K_M row), not `soon`
- `dense: true` → Start when GGUF present; skip convert messaging
- `start_server`: if `dense` and no PGRN → full Metal (`--gpu-layers 99`, `--jinja`, no `--pgrn*`); MoE path unchanged
- Dense sampler defaults: temp 0.7 / top_p 0.9 (conversational); coding MoE stays temp 0
- i18n EN+DE: `note.dolphin3`, `st.denseReady`
- Docs: `docs/DOLPHIN3_CYBER_8B.md` + smoke numbers

## Smoke (internal SSD, owned PID)

| | |
|--|--|
| GGUF | `~/Modelle/dolphin3-cyber-8b-q4/…Q4_K_M.gguf` (~4.6 GiB) |
| Load | ~5 s to `/health` |
| RSS | ~5.1 GiB |
| Prefill / decode | ~145 / ~27.6 tok/s (short chat) |
| PGRN | **no** |

Qwen 35B MoE path untouched. One serve only (CRASH_AVOIDANCE).

## Verify

```bash
node app/scripts/test_a11y_click_contract.mjs
node app/scripts/test_api_parity_ai.mjs
# optional live smoke (no dual-serve):
app/scripts/live_dolphin_dense_smoke.sh
```
