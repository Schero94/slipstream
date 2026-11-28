# Slipstream engine port: upstream llama.cpp 33aeb17f8586 → 8e23278c7c36

- Old pin: `33aeb17f8586` (2026-11-24, "feat: Inkling 2 DSpark support (#32218)")
- New pin: `8e23278c7c36` (full `8e23278c7c36f4429fb237467d50e2935a91e655`, 2026-11-27, "ui: show only the mention
  label in generated conversation titles (#33205)") = upstream master on 2026-11-28.
- Gap closed: **106 upstream commits**.
- Port source: slipstream `0d788ef` (engine/ + patch), plus the two later server commits from the main session
  (`e54201d` prompt-cache copy restore, `ea5c9fa` usable-prefix compare + LRU refresh), carried as a delta
  (`ea5c9fa` tree − `0d788ef` tree, 3 files, applied cleanly with `git apply -3`).
- Port tree: `../upstream-port2/llama.cpp`, branch `port2` (commits: cherry-pick of "slipstream @ 33aeb17", Metal
  graph_id fix, server delta, chunked-commit gate). Old binaries snapshotted in `../upstream-port2/bin-old/`
  (built 2026-11-28 01:21 from the 33aeb17 tree). The previous working tree is kept as
  `../llama.cpp-slipstream.pin-33aeb17`.
- Method as last time: `slip-pin` = old pin + engine/ + patch (committed), cherry-picked onto upstream master
  (3-way merge). `scripts/engine_sync.sh` gained `TREE=<dir>` so the patch can be regenerated from a tree other than
  `llama.cpp-slipstream/`; `TREE=<port tree> scripts/engine_sync.sh --verify` → VERIFY PASS (exit 0).

## Conflicts (7 files, 10 hunks) and resolutions

| file | upstream change | resolution |
|---|---|---|
| `src/llama-context.cpp`, `src/llama-graph.cpp` | graph params gained `w_scales` (#28898 quant scales) | keep both: `w_scales` + the layer-major `lm_il0/lm_il1/lm_resid` |
| `src/llama-model.cpp` | output `output_s/output_in_s` scale tensors and their "separate output weight" check removed (#28898: scales now live in a map, `get_w_scale`) | keep the PGRN admission/loader block, drop the removed check |
| `src/models/dflash.cpp` (4) | `build_lora_mm` / `build_attn` lost the per-weight scale argument (#28898) | keep the Laguna-drafter additions (aux_norm, normed context K/V, softplus gate with deferred `wo`), drop the `*_s` arguments |
| `tools/server/server-common.cpp` | named `tool_choice` implemented upstream (#31564, incl. `allowed_tools`) | take upstream; the fork's function-object handling is dropped (upstream covers it) |
| `ggml/src/ggml-cuda/fattn.cu`, `tests/test-backend-ops.cpp` | upstream keeps `gqa_ratio > 2 / > 1` (non-power-of-2 GQA, #19092/#33045) and now tests GQA 2/3 for MLA | take upstream; the fork's CUDA `% 4 / % 2` change and its GQA 6/9 tests leave the patch (CUDA is not used on this host) |

Not a textual conflict, found at build time:

- **Metal chunked commit (#32947).** Upstream added a second graph-submit path (with one command-buffer thread,
  encode in 256-node chunks and commit each chunk). Its `ggml_metal_op_init` call lacked the fork's `graph_id`
  argument → compile error. Fixed by passing `profile_graph_id`, plus the `PGR_METAL_CPU_PROFILE` commit timing and
  graph line, so profiling works on both paths.

Cleanup: the old patch carried two stray backup files as new files (`src/llama-graph.cpp.orig`,
`src/models/qwen35moe.cpp.orig`, ~6k lines). They are gone; the patch shrank from 11,534 to 5,849 lines
(60 upstream files).

## Speed regression found and fixed: chunked Metal commit is opt-in

The first acceptance run (port as merged) had identical outputs but slower decode in the app regime
(llama-server, `SPEC_ARGS=" "`, lookahead on, prompt_code.txt, 160 tokens, interleaved, 25 s cooldown):

| cache | old | new (as merged) |
|---|---|---|
| 4 GiB | 10.10, 10.53 | 10.02, 10.00 (−3.0 %) |
| 10 GiB | 18.61, 18.80 | 18.08, 17.49 (−4.9 %) |

Bisect with env toggles in one binary (`bench/fetchwall/port2_perfbisect.sh`, 10 GiB, 2 reps; temporary branch
`perf-bisect`): skipping the chunked commit path (`PORT_NOCHUNK`) 19.00 / 18.95 vs new 17.36 / 18.25 and old
18.13 / 18.38; reverting #33175 (fused-node concurrency check, a routing-determinism fix) 17.90 / 17.98 — no gain;
recurrent states by gather instead of in place (#30139 reverted) 17.11 / 17.12 — slower, so in-place reading helps.
Round 2 at both caches (old / new / no-chunk / chunked with a small first chunk):
10 GiB 18.72, 18.69 / 18.09, 17.74 / 18.94, 18.95 / 19.03, 18.59; 4 GiB 10.28, 10.29 / 9.95, 9.91 / 10.43, 10.35 /
10.28, 10.18. All outputs `38b5b06864f1`.

Cause (likely): with PGRN the scheduler gives Metal many small graphs (split at every top-k read-back). The old
path commits the first `max(64, 10 %)` nodes before encoding the rest; the chunked path encodes a whole small graph
before the first commit, so the GPU starts later on every split.

Fix: the chunked path runs only with `GGML_METAL_CHUNK_COMMIT=1`; default = the old path. Lossless (only command
buffer submission order changes). Resident (non-PGRN) runs also keep the old path; whether the chunked path helps
them is a backlog item.

## Acceptance (final port binary, 2026-11-28 03:31; all GGUF/PGRN from ~/Modelle, Qwen3.6-35B-A3B UD-Q4_K_XL)

- Build: static Metal flags, every target, `otool -L` system frameworks only.
- `ctest -R "peregrine|arg-parser|chat-auto-parser"`: 15/16. The one failure, `test-peregrine-model-e2e`, is the
  known `--repo-root` layout issue in a tree outside the repo. Run by hand with `--repo-root slipstream`: all parity
  cases pass with nmse=0 / max_abs=0 (decoder, compact, coupling, MTP, CPU + Metal), layer-major bitwise=yes
  (rewind, abort 60 checks, prefixes 0/5), identity rejection OK. In the repo layout (rebuilt
  `llama.cpp-slipstream/`) see bench/RESULTS.md.
- Parity, llama-completion `run_arm.sh` 4 GiB (default lookahead): old 10.63, 10.50 · new 10.69, 10.46; output
  **e6c6a36552db** in all four, hit 69.01 %, recall 82.87 %, swap 0.
- E4 (`e4_newconv.sh`, LM=1, new binary): S1 39,517 tokens 135.9 s, S2 33 tokens read, 2.1 s; hashes
  **a9f8c4863784 / c4062b92a557** (same as every earlier run); peak RSS 10.5 GB, swap 0.
- Decode, llama-server app regime, interleaved old/new, 2 reps: 4 GiB old 10.49, 10.43 → new 10.58, 10.50
  (+0.8 %); 10 GiB old 18.01, 18.47 → new 19.10, 19.08 (+4.7 %). Output 38b5b06864f1 everywhere, swap 0.
- Layer-major progress (`lm_progress.sh`): 332 events, monotonic, median gap 0.31 s, first token 133.9 s, output
  54edf738c5a8 (= before). Cancel (`lm_abort.sh`): aborted 0.072 s after the cancel at layer 7/40, follow-up short
  reply identical (5dd272b4f316).

No upstream change in this range changed Qwen3.6 numerics (identical hashes in completion, server, E4 and
layer-major runs), so no KLD run was needed.

## Upstream features we get

- Recurrent states read in place (#30139): measurably faster decode for Qwen3.6's DeltaNet layers (the gather
  revert was 6 % slower in the bisect).
- Deterministic expert routing on Metal: the fused top-k no longer runs concurrently with the router matmul (#33175).
- Quant scales for fp8 / nvfp4 (#28898), named `tool_choice` + `allowed_tools` (#31564), BF16 src1 for MUL_MAT(_ID),
  moe-cache across backends, seq-state restore fix with ON_DEVICE (#30318), YaRN hparams in the base loader, new
  models (RAON-1-mini, hy_v4_flash, Inkling 2 updates), many CUDA/Vulkan/SYCL changes not used here.

## Risks / not verified

- Laguna and the Laguna DFlash drafter: compile only (no Laguna model on the internal SSD), as last port. The
  `*_s` scale arguments were removed from the drafter graph; upstream applies scales through the weight-scale map.
- `tool_choice` with a function object now goes through upstream's implementation; the fork's error messages
  ("tool_choice function '…' is not present in tools") are gone. Not exercised by a request.
- MTP speculation in the server was not re-measured in this port (the app runs streamed models without MTP when
  lookahead is on); the e2e MTP parity cases pass.
- Resident runs and the chunked Metal path: not measured (see backlog).
