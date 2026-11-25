# Improvement backlog

Score = impact × confidence ÷ effort (each 1–5). Status: open / doing / done / dropped /
blocked (needs the user). The loop in `docs/IMPROVEMENT_LOOP.md` always picks the highest
open score. Add every new idea here with a one-line hypothesis.

| Score | Item | Hypothesis / target | I | C | E | Status |
|---:|---|---|---:|---:|---:|---|
| 8.0 | Prefill feasibility: compute vs I/O split on the 35k prompt (llama-completion fetch/stage ms) | decide if <30 s is reachable by I/O work alone or needs prefix reuse (Colibrì review: compute ceiling risk) | 5 | 4 | 2.5 | open |
| 7.5 | Cold insertion of prefill reads (Colibrì #2912) | prompt sweep stops evicting decode-hot experts; decode misses after a big prompt −50 %+, prefill misses down | 3 | 5 | 2 | open |
| 5.3 | Whole-next-layer prefetch during prefill into a second scratch (Colibrì prefill_stream, #2850) | overlap SSD with compute in prefill (~all experts used, no predictor needed); TTFT −20..30 % | 4 | 4 | 3 | open |
| 4.0 | Cross-conversation prefix reuse for hybrid Qwen3.6 (Colibrì prefix_store; llama.cpp checkpoints) | same system prompt + tools → second agent conversation TTFT seconds instead of minutes | 5 | 4 | 5 | open |
| 4.0 | Lookahead top-6 instead of top-8 (Colibrì #2300) | ranks 7–8 often wasted reads; decode +3–5 % on code | 2 | 4 | 2 | open |
| 3.0 | MTP single draft, confidence ≥ 0.6 (Colibrì #2302) | narrow verify keeps MTP profitable while streaming; code +7–11 % | 3 | 3 | 3 | open |
| 2.7 | Re-check OS page cache vs F_NOCACHE with bounded 10 GiB cache (Colibrì relies on page cache) | maybe faster without swap now; must stay 0 MB swap | 3 | 2 | 2.2 | open |
| 6.0 | New MoE models: scout + test 2–3 that fit (agent running) | v0.4 wins hold beyond Qwen3.6; catalog stays current | 5 | 4 | 3 | doing |
| 5.0 | PGRN support for merged gate_up expert tensors | new upstream-converted GGUFs merge gate+up; PGRN v1 rejects them → needed for new models | 5 | 4 | 4 | open |
| 4.0 | Beyond-decode research: prompt-cache reuse across agent turns, KV compression, agent UX (agent running) | ranked list of user-felt wins | 4 | 3 | 3 | doing |
| 6.7 | Prefill baseline + profile (35k prompt) | know whether prefill is SSD-bound and where time goes | 5 | 4 | 3 | doing |
| 5.0 | Cheap prefill levers: ubatch 4096/8192, io-threads 16 | fewer passes → fewer expert re-reads; TTFT −20..40 % | 4 | 5 | 4 | doing |
| 5.0 | Layer-(group)-major prefill (Colibrì qwen36_graph.h qg_prompt_layers, PR #2298 — same model, Metal) | each expert read once per prompt; needs full-layer scratch (partitions hold ~130 < 256 experts) or 4-layer groups; output identical | 5 | 4 | 4 | open |
| — | Colibrì latest optimizations review | done 2026-11-25: main e479104, dev 8ea9e9d; items added above | | | | done |
| 4.0 | Dependencies + forks to latest (worktree chore/deps-latest) | current, secure, no regressions | 3 | 4 | 3 | doing |
| 3.0 | Prompt/KV cache persistence across agent turns | repeated agent context answered without re-prefill | 5 | 3 | 5 | open |
| 2.7 | Improve lookahead recall beyond 83 % | each +5 pp recall ≈ fewer sync misses (38 % of decode time at 10 GiB) | 4 | 2 | 3 | open |
| 2.0 | Laguna 118B validation on the new engine | the "bigger than RAM" pitch works with v0.4+ | 5 | 4 | 5 | blocked (disk: ~134 GB needed, ~50–63 GB free) |
| 1.6 | Notarized app + auto-update | no right-click → Open, painless upgrades | 4 | 2 | 5 | blocked (Apple Developer ID) |
