# Improvement backlog

Score = impact × confidence ÷ effort (each 1–5). Status: open / doing / done / dropped /
blocked (needs the user). The loop in `docs/IMPROVEMENT_LOOP.md` always picks the highest
open score. Add every new idea here with a one-line hypothesis.

| Score | Item | Hypothesis / target | I | C | E | Status |
|---:|---|---|---:|---:|---:|---|
| 6.0 | New MoE models: scout + test 2–3 that fit (agent running) | v0.4 wins hold beyond Qwen3.6; catalog stays current | 5 | 4 | 3 | doing |
| 5.0 | PGRN support for merged gate_up expert tensors | new upstream-converted GGUFs merge gate+up; PGRN v1 rejects them → needed for new models | 5 | 4 | 4 | open |
| 4.0 | Beyond-decode research: prompt-cache reuse across agent turns, KV compression, agent UX (agent running) | ranked list of user-felt wins | 4 | 3 | 3 | doing |
| 6.7 | Prefill baseline + profile (35k prompt) | know whether prefill is SSD-bound and where time goes | 5 | 4 | 3 | doing |
| 5.0 | Cheap prefill levers: ubatch 4096/8192, io-threads 16 | fewer passes → fewer expert re-reads; TTFT −20..40 % | 4 | 5 | 4 | doing |
| 5.0 | Layer-major prefill | each expert read once per prompt instead of per ubatch; TTFT −50..80 %, output identical | 5 | 3 | 3 | open |
| 4.0 | Colibrì latest optimizations (review running) | adopt measured upstream wins, esp. prefill | 4 | 3 | 3 | doing |
| 4.0 | Dependencies + forks to latest (worktree chore/deps-latest) | current, secure, no regressions | 3 | 4 | 3 | doing |
| 3.0 | Prompt/KV cache persistence across agent turns | repeated agent context answered without re-prefill | 5 | 3 | 5 | open |
| 2.7 | Improve lookahead recall beyond 83 % | each +5 pp recall ≈ fewer sync misses (38 % of decode time at 10 GiB) | 4 | 2 | 3 | open |
| 2.0 | Laguna 118B validation on the new engine | the "bigger than RAM" pitch works with v0.4+ | 5 | 4 | 5 | blocked (disk: ~134 GB needed, ~50–63 GB free) |
| 1.6 | Notarized app + auto-update | no right-click → Open, painless upgrades | 4 | 2 | 5 | blocked (Apple Developer ID) |
