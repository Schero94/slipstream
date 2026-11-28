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
| — | Prefill cache pollution measurement (E3) | measured 2026-11-26: neutral | | | | dropped |
| 4.0 | Agent-profile hygiene in the one-click Kilo/OpenCode patch | route title/commit requests away from the main slot, ctx = server ctx, timeouts ≥ 600 s | 3 | 4 | 3 | open |
| 8.0 | Prefill feasibility: compute vs I/O split on the 35k prompt (llama-completion fetch/stage ms) | decide if <30 s is reachable by I/O work alone or needs prefix reuse (Colibrì review: compute ceiling risk) | 5 | 4 | 2.5 | open |
| — | Cold insertion of prefill reads (Colibrì #2912) | E3 2026-11-26: neutral, code removed | | | | dropped |
| — | Whole-next-layer prefetch during prefill (Colibrì #2850) | moot: with layer-major, fetch is ~6 s of ~140 s | | | | dropped |
| — | Cross-conversation prefix reuse (in RAM) | E4 2026-11-27: already works via llama-server user-start checkpoints + --cache-ram: new conversation 190 s → 2.1 s | | | | done |
| — | Lookahead top-6 instead of top-8 (Colibrì #2300) | 2026-11-28: −6.3 % @4 GiB, −7.8 % @10 GiB (code prompt, app regime); ranks 7–8 are routed every token, so skipping them only moves their reads onto the critical path; code removed | | | | dropped |
| 3.0 | MTP single draft, confidence ≥ 0.6 (Colibrì #2302) | narrow verify keeps MTP profitable while streaming; code +7–11 % | 3 | 3 | 3 | open |
| 2.7 | Re-check OS page cache vs F_NOCACHE with bounded 10 GiB cache (Colibrì relies on page cache) | maybe faster without swap now; must stay 0 MB swap | 3 | 2 | 2.2 | open |
| — | New MoE models scout | done 2026-11-25; see items above (LFM2.5, Qwen3-Coder, Mellum, type whitelist, generalization) | | | | done |
| 2.0 | PGRN support for merged gate_up expert tensors (only inkling2/gemma4/zaya/metis need it; fusing is opt-in upstream) | byte-exact split, plan in scout report | 3 | 4 | 6 | open |
| — | Beyond-decode research | done 2026-11-26; items added above (E1–E3, progress, profile hygiene); rejected: --cache-reuse (disabled for hybrid), token-dropping prefill, sparse attention, KIVI, LLMLingua on code | | | | done |
| — | Prefill baseline + split | done 2026-11-26 (staging 45–47 % before layer-major) | | | | done |
| — | Cheap prefill levers (ubatch 4096) | done v0.4.1 | | | | done |
| — | Layer-major prefill | done v0.4.1 opt-in; default-on pending cancellation + 16 GB check | | | | done |
| — | Colibrì latest optimizations review | done 2026-11-25: main e479104, dev 8ea9e9d; items added above | | | | done |
| — | Dependencies semver refresh | done: cargo update both workspaces + Tauri 2.12.7 merged (183/96/44/7 tests, dmg build ok) | | | | done |
| — | llama.cpp re-port 33aeb17f8586 → 8e23278c7c36 (106 commits) | done 2026-11-28: docs/PORT_UPSTREAM_8e23278.md; parity hashes unchanged, decode 4 GiB +0.8 %, 10 GiB +4.7 % | | | | done |
| 3.0 | Upstream chunked Metal commit (#32947) for resident / non-PGRN runs | opt-in GGML_METAL_CHUNK_COMMIT=1 since the 8e23278 port (with PGRN it cost 3–5 % decode); measure resident decode/prefill with it on/off, consider enabling it only without --pgrn | 2 | 3 | 2 | open |
| — | Low-risk dependency majors | done: libp2p 0.57.1 (codec → native async fn; pulls aws-lc-rs), ureq 3, mdns-sd 0.22 (IPv6 answers now parsed), base64 0.23; 183/96 tests + mdns 15/15, dmg build ok; Dockerfile.node rebuild not verified | | | | done |
| 4.0 | MLX runtime lock refresh (chore/deps-latest 992c7e7) + live oMLX/PGRN serve smoke | newer starlette/pydantic/hf-hub; must serve an MLX model before release | 2 | 4 | 2 | open |
| 2.0 | RustCrypto 3.x + rand 0.10 together (16 call sites, wire crypto) | needs interop tests between old/new nodes | 2 | 3 | 3 | open |
| 1.3 | oMLX fork 0.5.3 → 0.8.3 (1,881 commits; mlx 0.32.4, transformers ≥5.14) | multi-day rebase + vendor A/B gate | 3 | 2 | 4.5 | open |
| 1.5 | rusqlite 0.43 (MSRV 1.89 → bump Dockerfile.node rust image) | | 1 | 4 | 2.7 | open |


| 2.7 | Improve lookahead recall beyond 83 % | each +5 pp recall ≈ fewer sync misses (38 % of decode time at 10 GiB) | 4 | 2 | 3 | open |
| 2.0 | Laguna 118B validation on the new engine | the "bigger than RAM" pitch works with v0.4+ | 5 | 4 | 5 | blocked (disk: ~134 GB needed, ~50–63 GB free) |
| 1.6 | Notarized app + auto-update | no right-click → Open, painless upgrades | 4 | 2 | 5 | blocked (Apple Developer ID) |

## From Colibrì review 2026-11-27 (docs/COLIBRI_REVIEW_2026-11-27.md)

| Score | Item | Hypothesis / target | I | C | E | Status |
|---:|---|---|---:|---:|---:|---|
| 10.0 | Op-level profile of layer-major prefill compute (~130 s) | know the DeltaNet / attention / expert split before any compute work | 5 | 4 | 2 | open |
| 8.0 | Per-layer progress + cancel for layer-major prefill (Colibrì #2056) | progress moves during a 150 s prompt, cancel works; prerequisite for layer-major default-on | 4 | 4 | 2 | doing |
| 6.7 | Prompt states survive app/server restart (Colibrì #2781/#3083; --slot-save-path) | first request after restart 190 s → < 3 s; app saves slot before stop, restores after start | 5 | 4 | 3 | open |
| — | Stage experts in ascending use-count order in layer-major (Colibrì #1769 idea) | 2026-11-28: neutral (−0.4 % @4 GiB, +0.9 % @10 GiB on the first 256 tokens after the 39.5k prompt); the slowdown after a long prompt is not cache content (hit counters unchanged); code removed | | | | dropped |
| 5.3 | Last layer computes attention/output only for the last token (qwen35moe.cpp) | first turn −2..4 s, lossless | 2 | 4 | 1.5 | open |
| — | Decayed-frequency eviction (arXiv 2611.15842), offline trace replay first | 2026-11-28: plain decayed counts are worse with lookahead (+23..+56 % demand misses); with lookahead-protect they win: replay −8..−13 % demand misses, A/B decode +3.5 % @4 GiB, +2.3 % @10 GiB, +11 % after a 39.5k prompt @4 GiB, identical output. Kept opt-in as `PGRN_EVICT=decay` (bench/RESULTS.md) | | | | done (opt-in) |
| — | Make `PGRN_EVICT=decay` the default (+ app) | 2026-11-28: app sets it for streamed runs with cache < 0.45 × PGRN (≤ 8 GiB of the 35B), no-MTP pooled +1.9 % @4, +3.5 % @6, +1.0 % @8 GiB, −1.0 % @10 GiB (so off there); engine default stays off; critic fixes done (exact parsing, H clamp, HOT-tier WARN, wasted-read accounting). Needs the bundled engine rebuilt at the next release | | | | done (app) |
| — | Kick-free lookahead (`batch_begin(L+1)` inside the kick, critic idea) | 2026-11-28: parity OK, but 0 failed publishes without MTP → nothing to unblock; −1.3 % vs decay @4 GiB, noise @8–10. Reverted; only relevant if streamed runs ever use MTP again | | | | negative |
| 3.0 | Multi-token expert-use predictor for eviction (Belady headroom) | replay: Belady −30..−60 % reads vs ~−10 % for decay; the paper needs ~16 tokens of foresight for most of it | 4 | 1 | 4 | open |
| 4.5 | Turbo: treat lookahead-predicted experts as resident (Colibrì; lossy opt-in) | decode +6–11 % for +0.2 % NLL | 3 | 3 | 2 | open |
