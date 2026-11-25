# Improvement backlog

Score = impact × confidence ÷ effort (each 1–5). Status: open / doing / done / dropped /
blocked (needs the user). The loop in `docs/IMPROVEMENT_LOOP.md` always picks the highest
open score. Add every new idea here with a one-line hypothesis.

| Score | Item | Hypothesis / target | I | C | E | Status |
|---:|---|---|---:|---:|---:|---|
| 10.0 | Generalize lookahead + compact + Turbo beyond qwen35moe (qwen3moe, lfm2moe, mellum, …) | v0.4 gains apply only to Qwen3.6 today; move the lookahead/steer/compact hooks into shared build_moe_ffn paths → same +20–40 % on other MoE archs, lossless | 5 | 4 | 2 | open |
| 8.0 | New model test 1: LFM2.5-24B-A2B (unsloth UD-Q4_K_XL, separate K-quant experts, ~29 GB) | converts + runs today; baseline tok/s at 2/4/8 GiB; then gains after generalization | 4 | 5 | 2.5 | doing |
| 6.7 | Widen PGRN expert-type whitelist to Q8_0/Q5_0/Q5_1/Q4_0 (pgrn-convert.cpp:590) | unlocks Mellum2.1, Xing4.0, Clover, Qwen3.8-Flash-Next (down_exps not K-quantizable at widths % 256 ≠ 0); test on Mellum (8 GB) | 5 | 4 | 3 | open |
| 5.3 | New model test 2: Qwen3-Coder-30B-A3B (ggml-org Oct-22 reconvert, ~34 GB) | fresh upstream GGUF loads; compare with old Qwen3-30B rows | 4 | 4 | 3 | open |
| 3.0 | README/catalog: minimax-m3, deepseek4 already in pin (blocker is NVFP4 experts), add LFM2.5/Mellum when verified | docs truthful | 2 | 5 | 3.3 | open |
| 12.5 | Agent context survives other requests + restarts (E2): raise `--cache-ram` and/or SSD slot spill (`--slot-save-path`, save/restore) | app's `--cache-ram 512` drops every prompt state > ~22k tokens; turn after an interleaved request pays ~150 s again; restore measured 0.10 s vs 83.7 s (W1) → TTFT on hits < 3 s, 0 MB swap | 5 | 5 | 2 | open |
| 10.0 | Turn-to-turn checkpoint reuse in the real chat path (E1) | multi-turn replay with tools: from turn 2 `prompt_n ≤ new + 64`; tune `--checkpoint-min-step`, `preserve_thinking` | 5 | 4 | 2 | open |
| 10.0 | Prefill progress in the cockpit (`return_progress` / `prompt_progress`) | long waits become visible ("reading prompt 18k/35k, ~70 s"); keeps SSE alive | 3 | 5 | 1.5 | open |
| 6.0 | Prefill cache pollution measurement (E3) → cap prefill admission / cold insert | decode right after a 30k prefill ≥ 10 % slower; fix with cold inserts (pairs with Colibrì #2912 item) | 3 | 4 | 2 | open |
| 4.0 | Agent-profile hygiene in the one-click Kilo/OpenCode patch | route title/commit requests away from the main slot, ctx = server ctx, timeouts ≥ 600 s | 3 | 4 | 3 | open |
| 8.0 | Prefill feasibility: compute vs I/O split on the 35k prompt (llama-completion fetch/stage ms) | decide if <30 s is reachable by I/O work alone or needs prefix reuse (Colibrì review: compute ceiling risk) | 5 | 4 | 2.5 | open |
| 7.5 | Cold insertion of prefill reads (Colibrì #2912) | prompt sweep stops evicting decode-hot experts; decode misses after a big prompt −50 %+, prefill misses down | 3 | 5 | 2 | open |
| 5.3 | Whole-next-layer prefetch during prefill into a second scratch (Colibrì prefill_stream, #2850) | overlap SSD with compute in prefill (~all experts used, no predictor needed); TTFT −20..30 % | 4 | 4 | 3 | open |
| 4.0 | Cross-conversation prefix reuse for hybrid Qwen3.6 (Colibrì prefix_store; llama.cpp checkpoints) | same system prompt + tools → second agent conversation TTFT seconds instead of minutes | 5 | 4 | 5 | open |
| 4.0 | Lookahead top-6 instead of top-8 (Colibrì #2300) | ranks 7–8 often wasted reads; decode +3–5 % on code | 2 | 4 | 2 | open |
| 3.0 | MTP single draft, confidence ≥ 0.6 (Colibrì #2302) | narrow verify keeps MTP profitable while streaming; code +7–11 % | 3 | 3 | 3 | open |
| 2.7 | Re-check OS page cache vs F_NOCACHE with bounded 10 GiB cache (Colibrì relies on page cache) | maybe faster without swap now; must stay 0 MB swap | 3 | 2 | 2.2 | open |
| — | New MoE models scout | done 2026-11-25; see items above (LFM2.5, Qwen3-Coder, Mellum, type whitelist, generalization) | | | | done |
| 2.0 | PGRN support for merged gate_up expert tensors (only inkling2/gemma4/zaya/metis need it; fusing is opt-in upstream) | byte-exact split, plan in scout report | 3 | 4 | 6 | open |
| — | Beyond-decode research | done 2026-11-26; items added above (E1–E3, progress, profile hygiene); rejected: --cache-reuse (disabled for hybrid), token-dropping prefill, sparse attention, KIVI, LLMLingua on code | | | | done |
| 6.7 | Prefill baseline + profile (35k prompt) | know whether prefill is SSD-bound and where time goes | 5 | 4 | 3 | doing |
| 5.0 | Cheap prefill levers: ubatch 4096/8192, io-threads 16 | fewer passes → fewer expert re-reads; TTFT −20..40 % | 4 | 5 | 4 | doing |
| 5.0 | Layer-(group)-major prefill (Colibrì qwen36_graph.h qg_prompt_layers, PR #2298 — same model, Metal) | each expert read once per prompt; needs full-layer scratch (partitions hold ~130 < 256 experts) or 4-layer groups; output identical | 5 | 4 | 4 | open |
| — | Colibrì latest optimizations review | done 2026-11-25: main e479104, dev 8ea9e9d; items added above | | | | done |
| 4.0 | Dependencies + forks to latest (worktree chore/deps-latest) | current, secure, no regressions | 3 | 4 | 3 | doing |

| 2.7 | Improve lookahead recall beyond 83 % | each +5 pp recall ≈ fewer sync misses (38 % of decode time at 10 GiB) | 4 | 2 | 3 | open |
| 2.0 | Laguna 118B validation on the new engine | the "bigger than RAM" pitch works with v0.4+ | 5 | 4 | 5 | blocked (disk: ~134 GB needed, ~50–63 GB free) |
| 1.6 | Notarized app + auto-update | no right-click → Open, painless upgrades | 4 | 2 | 5 | blocked (Apple Developer ID) |
