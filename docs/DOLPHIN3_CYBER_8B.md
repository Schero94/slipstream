# Dolphin3-Cyber-8B on Slipstream (dense Metal)

**Date:** 2026-08-17  
**Model:** [RavichandranJ/Dolphin3-Cyber-8B-GGUF](https://huggingface.co/RavichandranJ/Dolphin3-Cyber-8B-GGUF)  
**Quant used:** `Q4_K_M` (4.92 GB / 4.58 GiB on disk)  
**License:** Llama 3.1  
**Architecture:** dense LlamaForCausalLM 8.03B (32 layers, GQA 32/8) — **not MoE**

## PGRN streaming?

**No.** There are no MoE experts to peel into a `.pgrn` sidecar. Do **not** run PGRN convert. Path is **full Metal GGUF load** (`--gpu-layers 99`, no `--pgrn`). Compact slots / grammar-draft / expert-cache knobs are MoE-only and N/A here.

## Local path

```
~/Modelle/dolphin3-cyber-8b-q4/Dolphin3.0-Llama3.1-8B-abliterated.Q4_K_M.gguf
```

Created `~/Modelle` (was missing). Qwen 35B trees were not on internal disk this session (Crucial not mounted) — Dolphin lives on internal only.

## Chat template

Llama 3.1 / Dolphin3 headers (`<|begin_of_text|>`, `<|start_header_id|>…`, stop `<|eot_id|>`). Slipstream uses `--jinja` so the GGUF-embedded template applies. Recommended sampler (card + smoke): temp `0.7`, top_p `0.9`, top_k `40`, repeat_penalty `1.1`.

## Start in Slipstream (3 steps)

1. **Models** → pick **Dolphin3-Cyber-8B (dense)** (catalog id `dolphin3-cyber-8b`). Backend **Metal** (or Auto; dense has no MLX twin).
2. Confirm Models folder is `~/Modelle` and the GGUF shows ready (no convert step).
3. **Start** → Chat → **Stop** when done.

Requires a build that includes the dense Metal path (`dense: true` catalog + `start_server` allows GGUF-only when `dense`). Until that ships into `/Applications/Slipstream.app`, use the CLI below (same `llama-server` binary the app embeds).

### CLI (same engine as the app)

```bash
SRV=/Applications/Slipstream.app/Contents/Resources/resources/llama-server
GGUF=~/Modelle/dolphin3-cyber-8b-q4/Dolphin3.0-Llama3.1-8B-abliterated.Q4_K_M.gguf

"$SRV" \
  --model "$GGUF" \
  --gpu-layers 99 --ctx-size 8192 --parallel 1 \
  --batch-size 512 --ubatch-size 512 -fa on --jinja \
  --alias slipstream --host 127.0.0.1 --port 8080 \
  --metrics --no-warmup \
  --reasoning off --temp 0.7 --top-p 0.9 --top-k 40 --repeat-penalty 1.1 \
  -ctk q8_0 -ctv q8_0
```

## Recommended knobs (36 GiB unified)

| Knob | Value | Why |
|------|-------|-----|
| ngl | 99 (all) | Dense 8B fits easily on Apple Silicon |
| ctx | 8k default (4k tight / 16k if free≥14) | Fine-tune was 2k; base allows more — keep KV modest |
| KV | q8_0 | Full-attention; q8 buys headroom vs f16 |
| cache / io / compact / grammar-draft | ignore | PGRN MoE levers |
| temp | 0.7 | Conversational cyber assistant (not coding-temp-0) |

RAM ballpark: model ~5 GiB resident + KV for ctx. Keep ≥2 GiB free+inactive emergency floor (CRASH_AVOIDANCE). One serve only — never dual with Qwen/oMLX.

## Smoke (2026-08-17)

Artifact: `DOLPHIN3_CYBER_8B_SMOKE_20260817-212852.md`

| Metric | Value |
|--------|-------|
| free+inactive before | 10.88 GiB |
| Load → /health | ~5 s |
| RSS (ready / after chat) | 5.12 / 5.13 GiB |
| free+inactive after load | 7.11 GiB |
| Prefill | 37 tok / 0.255 s → **~145 tok/s** |
| Decode | 29 tok / 1.049 s → **~27.6 tok/s** |
| Wall chat | 1.34 s (incl. RTT) |
| free+inactive after stop | 9.13 GiB |
| Swap | unchanged (used 1165 MiB) |
| Dual-serve | none (owned PID torn down) |

Prompt: “In one short sentence, what is SQL injection?” — coherent answer.

## vs Qwen 3.6 35B-A3B (unfair but useful)

| | Dolphin3-Cyber 8B Q4 | Qwen 3.6 35B MoE Q4 (streamed) |
|--|---------------------|--------------------------------|
| Path | Dense full Metal | PGRN expert stream |
| Disk | ~5 GiB | ~21 GiB GGUF + PGRN |
| RSS | ~5 GiB | much higher (resident + arena) |
| Decode (this machine) | ~28 tok/s short chat | ~18–19 tok/s peak warm (different recipe) |
| Role | Cybersecurity specialist, uncensored | Daily coding MoE driver |

Expect Dolphin to feel snappier for short turns; do not treat tok/s as quality. Keep Qwen as default coding driver.

## Quants

| Quant | Size | Note |
|-------|------|------|
| **Q4_K_M** (chosen) | 4.92 GB | Best balance on 36 GiB |
| Q5_K_M | 5.73 GB | Catalog alt; not downloaded |
| Q8 / F16 | 8.5 / 16 GB | Skip unless needed |

## Catalog / ship

- Catalog entry: `dolphin3-cyber-8b` (not `soon: true`)
- Dense Metal start wired in product + workspace (`dense` flag)
- i18n EN+DE: `note.dolphin3`, `st.denseReady`
- Product bump target: Slipstream **0.3.5** on `Schero94/slipstream` (not LLM-BOOM releases)
