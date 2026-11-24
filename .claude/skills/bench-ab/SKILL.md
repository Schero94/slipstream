---
name: bench-ab
description: Run a disciplined interleaved A/B benchmark of an engine variant on a streamed MoE (decode tok/s, PGRN hit-rate, lookahead recall, swap delta, output-hash parity) and record the verdict. Use for any performance claim about the engine.
---

# A/B benchmark (fetch-wall experiments)

Tool: `bench/fetchwall/run_arm.sh LABEL CACHE_GB [ENV=VAL ...]`
- Defaults: `llama-completion` from `llama.cpp-slipstream/build-static/bin`, Qwen3.6-35B-A3B
  Q4_K_XL GGUF **and** PGRN from `~/Modelle/qwen3.6-35b-a3b-q4/` (internal SSD only —
  never load from `/Volumes/Crucial X10`), greedy, 160 tokens, compact slots on, io 8.
- Appends one line per arm to `bench/fetchwall/results/summary.tsv`
  (`decode_tps`, `hit`, `la_recall`, `swap_delta_mb`, `out` = output hash).

Protocol:
1. One model process at a time; nothing heavy running. RAM admission may refuse a
   cache size when free RAM is low — lower the cache, never bypass admission.
2. Interleave A,B,A,B (≥2 reps each), `sleep 25` between arms.
3. Cache band {4,6,8,10,14} GiB — test where the feature is meant to help.
4. Verdict:
   - lossless variant: `out` hash identical to baseline, else it is a bug;
   - win = consistent across reps and `swap_delta_mb` = 0;
   - lossy variant (e.g. `PGRN_STEER`): also run quality — see CLAUDE.md
     (`llama-perplexity --kl-divergence-base` on exact routing, then `--kl-divergence`).
5. Append method + raw numbers + verdict (incl. negatives) to `bench/RESULTS.md`.
