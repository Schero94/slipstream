# Slipstream engine port: upstream llama.cpp 79bba02a6741 → 33aeb17f8586

- Old pin: `79bba02a6741` (2026-07-16, "CUDA: Support CUDA Virtual Devices")
- New pin: `33aeb17f8586` (full: `33aeb17f85869279e3821f746bc645357139bc2c`, 2026-11-24, "feat: Inkling 2 DSpark support (#32218)")
- That's **2677 upstream commits** past the old pin.
- Source of the port: slipstream `fa779fb` (`engine/` + `patches/slipstream-seams.patch`, byte-identical to that commit). It already includes the PGRN perf print in sampling.cpp, the `pgrn_lookahead_*` perf fields and `pgrn_steer_mark`.
- **Revision 2** (after acceptance) additionally carries the coordinator's 7 newer engine changes (`since_fa.p1.diff`, applied with `patch -p1`):
  - `ggml_cont` on the lookahead node;
  - decode-only steer (`n_tokens <= 8`);
  - PSR mark on by default;
  - `pgrn_settle_us` stats in llama-context, `include/llama.h` and sampling.cpp;
  - repo-root detection in tests/CMakeLists.

  It also fixes the llama-server blocker described below. The `.orig` backups that `patch` left in the tree were removed.
- Port tree: `upstream-port/llama.cpp`, branch `port`. The cherry-pick of "slipstream @ pin" is resolved and staged but not committed.
- How it was built: a `slip-pin` branch (old pin + engine + patch, committed) was cherry-picked onto upstream master. That's a 3-way merge, equivalent to `git apply --3way`.

## How to apply (for the reviewer)
1. `apply.sh`: `PIN=33aeb17f8586`.
2. Replace `engine/` with `deliverable/engine/` (48 files). Then **delete** these files from the repo's `engine/`, because upstream now ships them and a stale copy would overwrite upstream's version during `apply.sh`:
   - `engine/src/models/laguna.cpp`
   - `engine/conversion/laguna.py`
   - `engine/models/templates/poolside-Laguna-{S-2.1,XS-2.1,XS.2}.jinja`

   The Slipstream-only parts of these files (DFlash capture points in `laguna.cpp`, the `DFlashLagunaForCausalLM` converter in `laguna.py`) now live in the seams patch.
3. Copy `deliverable/slipstream-seams.patch` to `patches/slipstream-seams.patch` (4440 lines, 53 upstream files, +2790/−61).
4. Round-trip check, done: a fresh clone at the new pin + `engine/` + `git apply patch` gives a tree identical to the port tree (`diff -rq` empty, the same check as `engine_sync.sh --verify`). `patched-upstream-files.txt` lists the files in the patch.
5. CLAUDE.md lists `src/models/laguna.cpp` as Slipstream-owned. That's no longer true; it's upstream plus a small patch.

## Conflicts and how each was resolved
The cherry-pick produced 43 conflicted files, about 130 hunks. Notation: "ours" = upstream master, "theirs" = fork.

### MoE graph (`src/llama-graph.{h,cpp}`, `src/models/qwen35moe.cpp`)
- **What changed upstream:** `build_moe_ffn` was restructured. The per-expert `*_exps_s` scale arguments are gone (scales now come from `model.get_w_scale(w)`). New arguments `post_exp_norm` and `post_exp_norm_eps` were added. Expert GEMMs moved into a `build_experts(cur, ids, mm_ids, cached)` lambda, used for upstream's new host MoE cache (`llama_moe_cache`).
- **`compact_slots`:** added as the last argument (default false). `expert_slots = ggml_dup(selected_experts)` is still named `pgrn_moe_slots-<il>`, and top-k is still named `ffn_moe_topk_raw` in compact mode, so the eval-callback contract is unchanged. Compact mode calls `build_experts(cur, ids=selected_experts, mm_ids=expert_slots, cached=false)`, so GEMMs read the compact arena tensors indexed by slot.
- **Behaviour change (intentional):** biases and scales are now indexed by **expert id**. The old fork indexed `*_b` biases by slot, which was wrong for compact mode on biased models. qwen35moe and Laguna have no expert biases, so their output is unchanged.
- **Compact mode bypasses two upstream paths:** the upstream MoE cache (`moe_cache` lookup is skipped) and `moe_random_routing`.
- **f16-safe down-projection scaling:** upstream added its own `build_amax_src1_f16`. It's a power-of-two amax rescale, but only for `llm_arch_mmid_down_bf16` arches (Mistral4, Mellum, Flint, **Laguna**).
  - **Resolution:** those arches use upstream's amax (an exact scale). Every other arch, including qwen35moe, keeps the fork's L2 column scaling unchanged.
  - This means **Laguna's numerics change** relative to the old fork (amax instead of L2). It should be equal or better, but it was not measured: no Laguna model was run.
- **qwen35moe:** the lookahead block (`pgrn_lookahead`, `PGRN_LOOKAHEAD`), the `steer` view passed as `exp_probs_b`, and compact selection were kept as they were. Call sites were adapted to the new signature, for both the trunk and the MTP graph.

### llama-context (`process_ubatch`, `graph_compute`, eval callback)
- `pgrn_eval_callback` / `pgrn_eval` were kept unchanged, and placed after upstream's new `sched_copy_experts`. In `process_ubatch` the hooks merged cleanly: callback swap, lookahead/steer reset, `pgrn_error` check.
- Fixed for the new API: `hparams.n_expert_used` is now a function, so it's called as `n_expert_used()`.
- Perf fields and prints were merged next to upstream's new `n_reallocs_*` counters.

### Server blocker found in acceptance (fixed in rev 2)
- **Symptom:** llama-server failed on the first prompt ubatch with `PGRN staging failed: top-k tensor geometry ...`. My first round only ran llama-completion on the real model, so this was missed.
- **Cause:** the tripping tensor was `ffn_moe_topk-39`, I32 with shape `[8, 0, 1, 1]`. The last trunk layer gathers only the output rows (`inp_out_ids`) before its FFN, and upstream's refactored server submits prompt ubatches that request **no** outputs. That layer's top-k therefore has 0 rows, and `pgrn_eval` rejected `ne[1] <= 0`.
- **Fix in `llama_context::pgrn_eval`:** `ne[1] == 0` is now valid and means there is nothing to stage. The callback drops that layer's lookahead tensor, still joins any prefetch aimed at the layer, and forwards to the user callback. The same applies to compact `pgrn_moe_slots`.
- The geometry error message now names the tensor and its shape.

### Loader / model (`llama-model-loader.*`, `llama-model.*`, `llama.cpp`, `llama-quant.cpp`, `include/llama.h`)
- **What changed upstream:** the loader ctor now takes `llama_load_mode` instead of `use_mmap`/`use_direct_io`, adds `load_mtp` and lazy reads, and keys `ctx_map` by `{buft, lazy}`. `external_experts` is now passed after `load_mtp`.
- **Streamed-expert branch:** moved to the top of the refactored `create_tensor`. `check_tensor_dims` gained an `allow_reshape` argument. Upstream deleted `create_tensor_as_view`, so it was dropped.
- **New in the port: PGRN forces `load_mtp`.** Upstream skips the NextN/MTP block unless MTP speculation is requested. The PGRN sidecar includes that block's experts, so skipping it would fail geometry validation. The MTP block's dense weights are therefore always loaded when `--pgrn` is set, which is what the old pin always did.
- `llama_model_params` gained `load_mtp` (upstream) and `pgrn_compact_slots` (fork). Default-param order was kept in sync.
- Duplicates from the fork's Laguna backport were removed: `LLM_TYPE_118B_A8B`, the second `llama_model_laguna` struct, the arch/vocab/pre-tokenizer entries, the chat-parser Laguna patches, and the saver exclusion.
- **`llama-model-saver`:** the fork's `recurrent_layers` fix was re-applied, writing n_layer_all entries. Upstream's per-layer `add_kv` writes only `n_layer()` entries, but the qwen35/qwen35moe loaders read n_layer_all, so a saved model with an MTP block could not be reloaded. This is an upstream bug that the e2e fixture hits.

### Metal (`ggml-metal-context.m`, `-device.{h,m}`, `-ops.{h,cpp}`, `ggml.{h,cpp}`)
- Upstream removed the `debug_fusion` argument, and `graph_id` was re-threaded after `debug_graph`. Kept as before: the `PGR_METAL_PROFILE` / `PGR_METAL_CPU_PROFILE` profiling, the encoder wrapper and the speculation-scope API.
- `SPEC_PIN`: upstream replaced the inline mat-mat test with `ggml_metal_op_mul_mat_use_mm()` and added an M5 tensor-API `mul_mat_mma` path. `spec_pinned` now gates the small-batch mat-vec path, `use_mm`, **and** the new mma path.
- CUDA `fattn.cu`: took upstream's version. It reworked the same gqa-ratio logic, and CUDA isn't used on this host.

### Speculative / MTP / DFlash (`common/speculative.*`, `src/models/dflash.cpp`, `models.h`, `conversion/*`)
- **Upstream now has its own MTP and DFlash family** (DFlash/DSpark/DFlash2/LiliCorr refactor #32016, device-buffer features, MTP device rows, probabilistic drafts). I **took upstream's implementations** and re-applied only what upstream lacks:
  - **ANE draft type (`draft-ane`):** ported to the new impl interface (`process(common_batch)`, the `n_max` ctor argument, `need_embd` removed). Its enum value sits after LILICORR, and `COMMON_SPECULATIVE_TYPE_COUNT` is now 14.
  - **Laguna DFlash drafter:**
    - `decoder_arch` hparam.
    - `enc.aux_norm` per-aux RMSNorm, applied in both the encoder graph and upstream's fused `build_inject_inp`.
    - Optional softplus attention gate with deferred `wo`.
    - Context K/V projected through `attn_norm`.
    - Causal attention when `dflash.decoder_arch == laguna`, in addition to upstream's `dflash.attention.causal` key.
    - Target extract id == n_layer reads unmasked nextn. This **forces host feature copies** (upstream's device-buffer feature path can't carry h_nextn).
    - Non-finite feature sanitizing on the host path.
    - The Laguna target graph got its DFlash capture points back (`t_layer_inp`, `h_nextn`, deferred out-row gather).
  - **SPEC_PIN kernel scope:** in the MTP draft decode, MTP catch-up and server target verify, now around `llama_process`.
  - Adaptive-draft EWMA controller.
- `common_base_params_to_speculative` was not changed. The server still clears `pgrn_path` for the draft model at load. In the fit path, upstream now fits the draft together with the target, but fit is disabled whenever `--pgrn` is set.

### Server (`server-context.cpp`, `server.cpp`, `server-common.*`, `server-task.*`)
- **What changed upstream:** a big refactor. It added `server_slot_stats` (replacing `n_draft_*` and `result_timings`) and `server_batch`. Upstream's MTP/probabilistic verify uses `spec_draft_q`, and `llama_process` replaced `llama_decode`.
- **Adaptive draft:** observed from `stats.n_draft_*`. Its metrics went into `server_slot_stats` and its `to_json` as `draft_adaptive_n`, `draft_adaptive_n_next`, `draft_acceptance`, `draft_acceptance_ewma`.
- **Grammar-forced drafts:** kept. They read `slot.stats`. They now also **clear `spec_draft_q`** when they replace the draft, so the probabilistic verifier falls back to sample-and-match instead of asserting on mismatched sizes.
- **Routing logger:** ported to `server_batch::token` (`id_slot`, `token`, `is_prompt`), wrapped around `llama_process` inside `yield_to_queue`. Logger setup moved into the new `llama_server(argc, argv)`.
- ANE identity check and fail-closed startup were kept. They're combined with upstream's `has_synth()` abort.
- The OpenAI `tool_choice` function-object support in server-common merged cleanly.

### common / args
- `--pgrn` now sets `load_mode` to non-mmap (`AUTO`/`MMAP` → `NONE`, `MMAP_MLOCK` → `MLOCK`), because upstream removed `use_mmap`/`use_direct_io`/`use_mlock`. `common_model_params_to_llama` enforces the same.
- The PGRN options block now sits after `--lazy-mode`. `--spec-draft-adaptive` sits after upstream's new `--spec-synth-*`.

### Build / tests
- Peregrine sources go into upstream's `LLAMA_CORE_SOURCES` so they stay out of the unity build, and `peregrine_ane.mm` is marked `SKIP_UNITY_BUILD_INCLUSION`. tests/tools CMake merged cleanly.
- **Engine tests ported to the new APIs:**
  - `test-peregrine-loader`: `load_mode` / `load_mtp` arguments.
  - `test-peregrine-model`: `common_batch` + `llama_process`, because `common_batch_add` is gone. The resident reference now also loads MTP so its tensor set matches the streamed load.
- **`test-llama-archs`:**
  - The fixture sets `load_mtp=true`. On upstream's virtual load path, a skipped MTP tensor has no buffer type and asserts (upstream bug).
  - `save_qwen35moe_separate` also splits the merged `ffn_gate_up_exps.scale` that upstream fixtures now carry.
- `test-arg-parser`: PGRN asserts now check `load_mode`. `test-chat-auto-parser`: took upstream's version, since its Laguna tests are already upstream.
- Python: `conversion/{__init__,base,qwen}.py`, `gguf-py` constants and `convert_hf_to_gguf_update.py` took upstream plus the fork's `DECODER_ARCH`, `ENC_AUX_NORM`, the `ATTN_GATE` DFlash tensor and the `DFlashLagunaForCausalLM` registration. `docs/speculative.md` kept both sections.

## Upstream features we get
- **Host MoE cache (`llama_moe_cache`):** LRFU/SLRU, missed experts run on CPU. This is a related idea to PGRN, but for RAM-resident experts. It's disabled when PGRN compact is active.
- Upstream MTP improvements: device-buffer h rows, saved/restored MTP state, probabilistic drafts, spec checkpoints.
- The DFlash family (DFlash2/DSpark/LiliCorr, device features, backend sampling).
- `--load-mode` and `--lazy-mode`.
- `build_amax_src1_f16`.
- Deterministic radix top-k ties on Metal.
- Many Metal kernel speedups: MXFP4/NVFP4, fused routing, BF16/F32 src1 for MUL_MAT(_ID), mul_mm for small K.
- New architectures, and the server refactor (stats, router role).

## Results
- **Build:** static Metal (`-DBUILD_SHARED_LIBS=OFF -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF -DGGML_METAL_EMBED_LIBRARY=ON`), `-j 8`, every build under `heavy.sh`. All targets built: llama-server, llama-completion, llama-perplexity, llama-pgrn-convert and all peregrine tests. `otool -L` shows only system frameworks.
- **ctest -R peregrine** (plus arg-parser and chat-auto-parser): 15/15 unit tests pass (`ctest.log`).
  - `test-peregrine-model-e2e` fails under ctest **only** because its `--repo-root` is hardwired to `${PROJECT_SOURCE_DIR}/../..`, which needs the slipstream repo layout for `bench.m1.convert_gguf_to_pgrn`.
  - Run by hand with `--repo-root slipstream`, it **passes everything** (`e2e-test.log`): qwen3moe and qwen35moe, CPU and Metal, coupling, compact, MTP. Every case gives nmse=0 / max_abs=0, and the identity rejection check passes.
- **Real model:** Qwen3.6-35B-A3B UD-Q4_K_XL, GGUF + PGRN from `~/Modelle` (internal SSD). `run_arm.sh`, cache 4 GiB, 160 tokens, greedy, interleaved old/new, 2 reps, 25 s cooldown (`parity-summary.tsv`).
  - Old binary: a snapshot of `llama.cpp-slipstream/build-static/bin/llama-completion` as it was at 04:07 on Nov 25.

| arm | old decode tok/s | new decode tok/s | output hash |
|---|---|---|---|
| streamed, default | 6.16, 5.98 (6.07) | 6.43, 6.25 (6.34, +4%) | e6c6a36552db both |
| streamed, `PGRN_LOOKAHEAD=0` | 10.11, 10.42 (10.27) | 10.56, 10.38 (10.47, +2%) | e6c6a36552db both |
| resident (no `--pgrn`), 1 rep | 28.88 | 32.49 (+12.5%) | e6c6a36552db both |

- **Parity:** the greedy output is **byte-identical** across old and new binaries, streamed and resident, lookahead on and off. Swap delta was 0 MB in every arm. The PGRN hit rate moved by about 0.1 percentage points (68.76 → 68.89).
- The tok/s numbers are informal port checks, not `bench/RESULTS.md` entries. The resident run had a single rep.

- **llama-server, rev 2** (`bench/fetchwall/run_arm_server.sh`, cache 4, `PGRN_LOOKAHEAD=0`, interleaved old/new, 2 reps; `server-parity-summary.tsv`, labels `port-*`):
  - Old binary: a snapshot of `llama.cpp-slipstream/build-static/bin/llama-server` as it was at 04:39.

| server arm | old tok/s | new tok/s | draft acc | output hash |
|---|---|---|---|---|
| no speculation (`SPEC_ARGS=" "`) | 10.07, 9.98 (10.03) | 10.31, 10.20 (10.26, +2%) | n/a | 38b5b06864f1 both |
| MTP (`--spec-type draft-mtp --spec-draft-n-max 4`) | 9.07, 9.03 (9.05) | 9.12, 8.93 (9.03) | 110/191 both | 38b5b06864f1 both |

  The server output is byte-identical to the old binary (the expected `38b5b06864f1`), MTP acceptance is identical, and swap delta was 0 MB.
- **Rev 2 tests:** all 15 unit tests pass. `test-peregrine-model-e2e` under ctest still fails in *this* layout: the new repo-root detection looks for `../bench` or `../../bench`, and neither exists under `upstream-port/`. In the real `slipstream/llama.cpp-slipstream` layout it resolves correctly. Run by hand with `--repo-root slipstream` it passes all 12 parity cases plus identity rejection (`e2e-test.log`).

## Risks / not verified
- **Laguna numerics:** Laguna now uses upstream's amax down-projection scaling, and upstream's Laguna graph plus re-added capture points. Not run, because no Laguna model is on the internal SSD. The Laguna DFlash drafter path was not run either. It compiles, but `aux_norm`, the gate and causal attention are untested on real weights.
- **Server paths:** plain decode and MTP speculation are now verified on the real model (above). Not exercised: adaptive draft, grammar-forced drafts (no grammar request was sent), the routing logger, ANE, SPEC_PIN, and multi-slot `--parallel` > 1.
- `--pgrn` now always loads the MTP block's dense weights. That's a few hundred MB more than an upstream non-MTP load, but the same as the old pin.
- DFlash drafters whose extract ids include n_layer lose upstream's device-buffer feature path, which is slower than host copies.
- Newly converted GGUFs with merged `ffn_gate_up_exps` (upstream now merges at convert, #32102) are rejected by PGRN v1 ("separate gate/up/down only"). Existing GGUFs are fine. `pgrn-convert` needs merged-tensor support eventually.
- `llama-graph` extra nodes: the L2 scaling adds about 5 ops per MoE layer. No graph-size problems were seen at ub 2048.
- The `test-peregrine-model-e2e` ctest `--repo-root` path is environment-dependent. It is unchanged from the fork.
