# Layer-major prefill for streamed MoE (PGRN) — implementation plan

Date: 2026-11-26 · Status: design (no code written yet) · Backlog row: "Layer-(group)-major prefill" (score 5.0)
Tree: `llama.cpp-slipstream` @ 33aeb17f8586 + Slipstream seams. Line numbers below refer to that tree on 2026-11-26.

## 0. Problem, measured

A 39,515-token prompt on Qwen3.6-35B-A3B UD-Q4_K_XL, streamed (PGRN), M3 Pro 36 GB, cache 4 GiB, llama-server
(`bench/fetchwall/results/prefill1.txt`, `split1.txt`, bench/RESULTS.md):

| arm | TTFT | prefill tok/s |
|---|---:|---:|
| ub 2048 | 259.1 s | 152.5 |
| ub 4096 | 207.0 s (−20 %) | 190.9 |
| ub 2048, 16 I/O threads | 249.0 s (−4 %) | 158.7 |

The PGRN counters for the measured request (task 37 in `split1.txt`; the cumulative counters minus the warm-up task 0):

| quantity | value |
|---|---:|
| prompt eval | 259.1 s |
| PGRN staging (callback time) | 118.2 s |
|   fetch from SSD (cache misses) | 108.1 s |
|   upload (memcpy into the scratch) | 9.6 s |
| experts staged | 189,724 (≈ 19.3 ubatches × 40 layers × 246) |
| bytes staged | 344,053 MiB = 336 GiB (1.81 MiB per expert) |
| cache hit rate | 3.3 % |
| **everything else (compute floor + sync + graph)** | **≈ 140.9 s** |

The PGRN file is 20.13 GB for 40 × 256 experts. A prompt that reads every expert once moves about 18.1 GiB.
Today the 39.5k prompt moves 336 GiB: every 2048-row ubatch routes to ~246 of 256 experts per layer, the cache
partition per layer holds ~56 records at 4 GiB (~140 at 10 GiB), so almost every ubatch re-reads almost every expert.
The SSD runs at an effective 3.1 GiB/s (336 GiB / 108 s).

## 1. Options and the recommendation

How the engine stages experts today (needed to judge the options):

- `pgrn_eval` (src/llama-context.cpp:3293) runs at each layer's top-k sync (`ffn_moe_topk-L`), reads the
  ids and calls `llama_model::pgrn_stage` (src/llama-model.cpp:3289) → `pgr_scratch_stage` → `pgr_stage_selected`
  (src/peregrine_stage.cpp). Prefill never uses compact slots (qwen35moe.cpp `build_layer_ffn`: compact only when
  `n_tokens == 1`).
- `pgr_stage_selected` fetches each distinct id through the layer's cache partition (`pgr_runtime_get_many` in
  windows of `min(io_width, layer_cap)`) and memcpys the record into the **scratch**.
- The scratch (src/peregrine_scratch.cpp `pgr_scratch_new`) is ONE region of full `n_expert` gate/up/down
  tensors (≈ 0.47–0.5 GiB, logged as "PGRN scratch"), and every streamed layer's tensors are bound to the same
  bytes. MUL_MAT_ID reads only the routed slots, so stale slots from other layers are harmless.

So the scratch already *is* a 256-slot lease for one layer. It just gets overwritten by the next layer before the
next ubatch comes back to this layer.

### (a) True layer-major, one layer per graph (k = 1) — RECOMMENDED

Run layer L for every ubatch of the decode batch, then layer L+1. The scratch keeps layer L's experts for all
ubatches; a per-layer "held" bitmap skips experts already uploaded. Each expert is read once per decode call.

- I/O: 336 GiB → ≤ 18.1 GiB for a single decode call (−95 %); see §4 for the server regime.
- Memory: +0.31 GiB (residual buffer for 40,960 rows × 2048 × f32). No new expert memory: the existing scratch
  is the lease, the cache partitions do not need ≥ 256 slots.
- Cost: 40 × 20 = 800 small graphs instead of 20 big ones. Today's graphs already sync at every layer's top-k
  (the eval callback splits there) and are rebuilt every ubatch (`n_kv` grows by 2048 per ubatch, so
  `can_reuse` fails anyway), so the extra cost is graph build/alloc per layer and a 16 MiB get_rows/set_rows
  per (layer, ubatch). Estimated 2–6 s (to be measured in M2).
- Same structure as Colibrì's `qg_prompt_layers` (c/qwen36_graph.h:1545): per layer, all batches; residual
  `S × H × 4` bytes in a GPU tensor viewed per batch.

### (b) Layer-group-major (k layers per graph)

Groups of k layers per graph, all ubatches per group. With the shared scratch this only works if the scratch has
k slabs (layer L bound to slab L mod k): +k × 0.47 GiB (k = 4 → +1.9 GiB taken from the cache or the headroom).
The cache partitions cannot be the lease (56 slots at 4 GiB, 140 at 10 GiB, both < 256), unless they are
repartitioned dynamically, which is much more engine work. The only gain over (a) is fewer graph launches
(200 instead of 800 → maybe 2–4 s). Keep as optional milestone M6, decided by M2's measured overhead. The
code in (a) takes a layer range `[il0, il1)`, so (b) is a scratch change plus a group-size knob, not a
redesign.

### (c) Keep a layer resident across ubatches with a scheduling hook, graph unchanged

The graph still runs a ubatch through all 40 layers before the next ubatch. Keeping every layer's experts across
ubatches means holding 40 × 256 × 1.81 MiB = 18.1 GiB, which is impossible at 4–10 GiB. What can be done
without restructuring is "fewer passes": larger ubatches (measured: ub 4096 −20 %). ub 8192+ grows the compute
buffer (MoE intermediates ~ n_tokens × 8 × 512 × 4 B × 3, plus DeltaNet chunks and FA) by GiBs that come out of
the cache budget, and still makes 5 passes. Rejected as the main fix; worth one measurement (ub 8192) as a
baseline arm in M4.

**Decision: (a), k = 1.** It reuses the scratch that exists, needs no extra expert memory, and the parity argument
is local (per layer, per ubatch). It is the most work in llama.cpp core files (memory rewind + range graph), but
none of it is speculative: every piece has a pattern in the tree (`prepare()`'s cell save/restore,
`h_nextn_dev`'s device row buffer, `llm_graph_input_batch_idxs`).

## 2. Design

### 2.1 Control flow inside `llama_context::decode`

decode() (src/llama-context.cpp:2177) already prepares all ubatches up front: `memory->init_batch` (line 2293)
returns a memory context that holds every ubatch and, for the KV cache, a slot (`sinfo`) per ubatch found by
`llama_kv_cache::prepare` (src/llama-kv-cache.cpp:785) — so **KV slots for all ubatches are reserved before the
first one runs**; nothing new is needed for that. The ubatch loop is `do { … } while (mctx->next())`
(lines 2383–2568).

New, between `output_reserve` (line 2336) and the `do {` at line 2383:

```cpp
// Slipstream layer-major prefill: run every ubatch through layer il, then il+1.
// Pre-passes cover layers [0, n_layer-1); the existing ubatch loop below then runs the last layer
// plus the output head with all its output/logits/sampler logic unchanged.
const bool lm = lm_eligible(*balloc, mctx.get(), has_embd_row, has_state_row, write_h_nextn, has_samplers);
if (lm) {
    if (int rc = decode_layer_major_prepasses(mctx.get()); rc != 0) return rc;  // ends with mctx->lm_rewind()
    lm_il0 = model.hparams.n_layer() - 1;  // the loop below builds graphs for [lm_il0, n_layer) + head
    lm_il1 = model.hparams.n_layer();
    model.pgrn_scratch_hold(lm_il0);       // the last layer's experts are also staged once
}
// ... existing do { ... } while (mctx->next()); unchanged ...
if (lm) { lm_il0 = 0; lm_il1 = -1; model.pgrn_scratch_hold(-1); }
```

`decode_layer_major_prepasses` (new, llama-context.cpp next to `process_ubatch`):

```cpp
int llama_context::decode_layer_major_prepasses(llama_memory_context_i * mctx) {
    const int n_layer = model.hparams.n_layer();
    for (int il = 0; il < n_layer - 1; ++il) {
        if (il > 0) mctx->lm_rewind();          // memory metadata back to "before ubatch 0", cursor = 0
        lm_il0 = il; lm_il1 = il + 1;
        model.pgrn_scratch_hold(il);            // new layer epoch: clear the held bitmap
        do {
            const auto & ubatch = mctx->get_ubatch();
            n_outputs = 0;                       // pre-pass graphs have no output head
            ggml_status status;
            const auto * res = process_ubatch(ubatch, LLM_GRAPH_TYPE_DECODER, mctx, status);  // applies ubatch b
            if (!res) return lm_fail(mctx, status);   // see 2.7
        } while (mctx->next());
        ++lm_passes;
    }
    mctx->lm_rewind();                           // the final pass re-applies ubatches 0..N-1 itself
    return 0;
}
```

`process_ubatch` (line 1830) is reused unchanged except that `graph_params` (line 3088) passes the new fields
(`lm_il0`, `lm_il1`, `lm_resid`). It calls `mctx->apply()` for ubatch b on top of the rewound state, so the cell
metadata each (layer, ubatch) graph sees equals what the normal path sees for ubatch b (see 2.2).

`lm_eligible(...)` — all must hold, else the normal path runs:
- `PGRN_LAYER_MAJOR=1` (read once in the ctor into `lm_enabled`; a test-only setter `set_layer_major(bool)`),
- `model.pgrn_enabled()` (the flag also works without PGRN — useful for the resident parity test — so allow it
  when `lm_enabled` is forced by the test setter),
- the arch supports layer ranges (`llama_model::supports_layer_range()`: true for `LLM_ARCH_QWEN35MOE` only in v1),
- number of ubatches in this batch ≥ `PGRN_LAYER_MAJOR_MIN_UBATCHES` (default 2),
- `cparams.causal_attn && !cparams.embeddings`, graph type DECODER (not the MTP context),
- `!has_embd_row && !has_state_row && !write_h_nextn && !layer_inp_dev && sampling.samplers.empty()`,
- `mctx->lm_supported()` (KV / recurrent / hybrid contexts, `n_rs_seq == 0`, no `rs_replay`),
- one sequence in the batch (`ubatch.n_seqs_unq == 1` for all ubatches; server `--parallel 1`).

### 2.2 Memory metadata: rewind, don't re-derive

Re-applying ubatch b after b+1…N are already applied is not safe: `llama_kv_cache::apply_ubatch`
(src/llama-kv-cache.cpp:1134) purges overwritten positions, and `get_n_kv` / `get_n_kv_rows` (lines 1297, 1376)
depend on how many cells are used at that moment. Rather than recording derived values, restore the metadata to
the pre-batch snapshot before every layer pass and re-apply ubatches in order. This is the same trick
`llama_kv_cache::prepare` (lines 785–848) and `llama_memory_recurrent::prepare` (src/llama-memory-recurrent.cpp:533)
already use.

New in `src/llama-memory.h` (`struct llama_memory_context_i`, line 82):

```cpp
// Slipstream layer-major prefill: restore the memory metadata to the state before the first apply()
// of this batch and move the cursor back to ubatch 0. Tensor data (K/V rows, recurrent states) is untouched.
virtual bool lm_supported() const { return false; }
virtual void lm_rewind() { GGML_ABORT("layer-major rewind not supported"); }
```

- `llama_kv_cache_context` (src/llama-kv-cache.cpp:2995ff): in `apply()` (line 3024), when `i_cur == 0` and no
  snapshot exists yet, take `snap = kv->lm_meta_save()`; `lm_rewind()` = `kv->lm_meta_restore(snap); i_cur = 0;`.
  `lm_supported()` = `!ubatches.empty() && kv->other == nullptr`.
  New in `llama_kv_cache`: `struct lm_meta { std::vector<llama_kv_cells> cells; std::vector<uint32_t> heads; };`
  `lm_meta lm_meta_save() const { return { v_cells, v_heads }; }` and the matching restore. A full copy of the
  cells for 40,960 cells is a few MiB on the host and happens 40 times per prompt.
- `llama_memory_recurrent_context` (src/llama-memory-recurrent.cpp:1404ff): same lazy snapshot in `apply()`
  (line 1426) of `{cells, used, head, n, rs_z, rs_idx}`; `lm_rewind()` restores it and sets `i_next = 0`.
  `rs_idx` must be in the snapshot because `s_copy()` (line 1500) resets it as a side effect of `set_input`.
  `lm_supported()` = `!is_full && !ubatches.empty() && mem->n_rs_seq == 0 && !mem->rs_replay`.
- `llama_memory_hybrid_context` (src/llama-memory-hybrid.cpp:247–268): `lm_rewind()` calls both children and
  sets `i_next = 0`; `lm_supported()` = both children.

Why this is exact: between rewinds nothing else touches the metadata (decode is single-threaded per context),
and after the last pass the metadata equals the normal path's end state, because the final pass applies
ubatches 0…N−1 in order on the snapshot exactly like the normal loop.

### 2.3 K/V and DeltaNet state across ubatches

- K/V rows are per layer (`get_k(ctx, il, …)`). Within a layer pass the ubatches run in order, so ubatch b of
  layer L reads K/V of ubatches 0…b−1 of layer L, which that pass has already written into their reserved
  cells (`sinfos[b]`), plus its own rows. The causal mask comes from the cell metadata after rewind + apply(0…b),
  which matches the normal path.
- Recurrent conv/SSM states are per layer (`mctx_cur->get_r_l(il)`, `get_s_l(il)`; qwen35moe.cpp
  `build_layer_attn_linear`). Layer L's state tensor is only written by layer L's graphs. Within the pass, ubatch
  b reads the state ubatch b−1 left for that layer. The per-ubatch inputs that pick the source cell and the zeroed
  cell (`s_copy`, `rs_z`, `head`, `n`) are reproduced by the rewind, so the very first ubatch of every layer pass
  clears/copies exactly as the normal path did for that layer.
- Positions: `inp_pos` is filled from `ubatch.pos`, which belongs to the ubatch objects kept by the memory context,
  so nothing changes.

### 2.4 The layer-range graph (qwen35moe)

`llm_graph_params` (src/llama-graph.h:857) gets three fields, copied into `llm_graph_context` by its ctor
(src/llama-graph.cpp:1715):

```cpp
int32_t       lm_il0   = 0;        // first layer of this graph
int32_t       lm_il1   = -1;       // one past the last layer; -1 = n_layer (normal full graph)
ggml_tensor * lm_resid = nullptr;  // persistent [n_embd, n_batch] F32 residual rows (device buffer)
```

`allow_reuse` (src/llama-graph.h:906) must also compare `lm_il0`/`lm_il1`. The row offset is NOT part of the
topology (rows come in through an index input, below), so a graph is reusable whenever the rest matches.

`llama_model_qwen35moe::graph::graph` (src/models/qwen35moe.cpp:155–241):

```cpp
const int il0 = params.lm_il0, il1 = params.lm_il1 < 0 ? n_layer : params.lm_il1;
const bool lm_in  = il0 > 0;          // read the residual rows of this ubatch from lm_resid
const bool lm_out = il1 < n_layer;    // write them back, skip the head
auto * lm_rows = (lm_in || lm_out) ? build_inp_lm_rows() : nullptr;   // I32 [n_tokens] = batch_idxs

inpL = lm_in ? ggml_get_rows(ctx0, lm_resid, lm_rows->idxs) : build_inp_embd(model.tok_embd);
auto * inp = build_inp_mem_hybrid();
ggml_tensor * inp_pos     = range_has_attn(il0, il1) ? build_inp_pos() : nullptr;  // unused inputs stay unallocated
ggml_tensor * inp_out_ids = lm_out ? nullptr : build_inp_out_ids();
for (int il = il0; il < il1; ++il) { /* body unchanged */ }
if (lm_out) {
    ggml_build_forward_expand(gf, ggml_set_rows(ctx0, lm_resid, inpL, lm_rows->idxs));
    return;                            // no output norm / LM head / t_h_nextn / t_logits
}
// tail unchanged (output norm, crop, LM head)
```

- `build_inp_lm_rows()` (new, llama-graph.cpp) adds `llm_graph_input_lm_rows` (new, llama-graph.h next to
  `llm_graph_input_batch_idxs`, line 174): an I32 `[n_tokens]` tensor filled from `ubatch.data->batch_idxs`
  (the token's index in the decode batch; same source as `llm_graph_input_batch_idxs::set_input`, llama-graph.cpp:254).
  `ggml_get_rows` needs I32; `ggml_set_rows` accepts I32 or I64. If the Metal `SET_ROWS` kernel rejects I32 at
  build time, add a second I64 tensor filled from the same indices.
- This is the `h_nextn_dev` pattern (`build_h_nextn_dev`, llama-graph.cpp:4722: `ggml_set_rows` into a
  persistent device buffer with a batch-index input). get_rows/set_rows of F32 rows are exact copies.
- In-graph hazard: the graph reads rows `R` of `lm_resid` at its start and writes rows `R` at its end. The write
  depends on `inpL`, which depends on every op that read the input (both the attention residual and the RMS norm
  are ancestors), and `get_rows` copies into a compute tensor first. The Metal backend's concurrency check
  (mem ranges) serializes overlapping src/dst anyway.
- `lm_resid` allocation: `llama_context::lm_resid_alloc()` modeled on `h_nextn_dev_alloc`
  (llama-context.cpp:1562–1621): one `[n_embd, n_batch]` F32 tensor in its own ggml context, on
  `ggml_backend_dev_buffer_type(model.dev_output())`, allocated in the ctor when `lm_enabled`, so the PGRN live
  headroom check after context creation (llama-context.cpp:4815–4829) counts it.
- Unused inputs: a one-layer graph of a DeltaNet layer does not use the attention inputs, and an attention layer
  does not use `s_copy`. `llm_graph_input_mem_hybrid::set_input` (llama-graph.cpp:1272–1292) calls
  `set_input_k_idxs` / `set_input_v_idxs` / `set_input_attn_mask` / the `s_copy` loop without checking that the
  tensor got a buffer (`llm_graph_input_attn_kv::set_input`, line 706, already does). Add the same
  `t && t->buffer` guards there, and in `set_input_rep`. `inp_pos` and `inp_out_ids` are only created when used
  (above). The comment at llama-context.cpp:1888 ("crash if any model inputs were not used") is the failure if a
  guard is missing — M2's fixture catches it immediately.
- Lookahead: build_moe_ffn only adds the lookahead router for `n_tokens <= 8` (llama-graph.cpp:2398), so prefill
  graphs have none. Nothing to change.
- Graph reserve: a one-layer graph is a subset of the full graph plus one `[n_embd, n_ubatch]` F32 get_rows
  result, the same size as the full graph's `inp_embd`. It fits the reserved compute buffer. Watch for
  "graph reallocation" lines in M2; if one appears, add a `graph_reserve` call with a layer-range shape in
  `sched_reserve`.

### 2.5 Output: logits for the last token only

The final pass (layer `n_layer−1` + head) runs inside the existing loop, so `n_outputs` per ubatch, the
`inp_out_ids` crop at the last layer (qwen35moe.cpp:192–195, `crop_before_nextn`), logits/embeddings extraction,
`out_ids` mapping and swaps (lines 2440–2640) are untouched. With the server's prompt batch (only the last token
has `output = true`) every ubatch except the last gets `n_outputs = 0`, and the last layer's top-k tensor has 0 rows
for those ubatches, which `pgrn_eval` already handles (line 3330). That matches what Colibrì does by hand ("last
layer computes K/V for all rows, the full layer for the last row"): llama.cpp's crop after attention does the same.

### 2.6 PGRN staging: hold a layer's experts across ubatches

Scratch (Slipstream-owned, `src/peregrine_scratch.{h,cpp}` → `engine/`):

```cpp
// Layer-major hold: while held_layer == L, the scratch remembers which experts of L it holds and
// pgr_scratch_stage uploads only the missing ones. Staging any other layer, or hold(-1), clears it.
void pgr_scratch_hold(pgr_scratch * scratch, int32_t layer);
size_t pgr_scratch_held_skips(const pgr_scratch * scratch);  // telemetry
```

`struct pgr_scratch` gains `int32_t held_layer = -1; std::vector<uint8_t> held; size_t skips = 0;`.
`pgr_scratch_stage`: if `layer == held_layer`, keep only the ids whose `held[id] == 0` (first-seen order kept,
dedupe as today), call `pgr_stage_selected` on that list, set `held[id] = 1` for each on success, and add the
filtered count to `skips`. If `layer != held_layer`, clear `held`, set `held_layer = -1` (this keeps the normal
path byte-for-byte as it is). `llama_model::pgrn_scratch_hold(int32_t)` (src/llama-model.cpp, next to
`pgrn_stage` at line 3289) forwards to it.

`pgrn_eval` (src/llama-context.cpp:3293–3428), only when `lm_active`:
- staging (line 3404) is unchanged, because `pgr_scratch_stage` filters by itself;
- skip the speculative kicks (lines 3412–3420: `pgrn_steer_mark`, `pgrn_prefetch_list`, `pgrn_prefetch_kick`):
  layer L+1 is ~20 graphs away, so a predicted-set warm of L+1 at every ubatch is wasted I/O. `prefetch_settle`
  stays (cheap, keeps the stream contract).
- the per-ubatch steer refresh (line 1905) is irrelevant (steer is lossy and off by default) but harmless.

Scratch vs partition: experts go SSD → cache partition slot (via `get_many` windows) → memcpy into the scratch,
exactly as today. The partition does not need to hold 256: the scratch is the lease, and after a record is
uploaded the partition may evict it. Each expert is fetched and uploaded once per layer pass.

Cache pollution: layer-major still writes ~256 prefill records per layer through the partition and evicts
decode-hot experts, like today. Reading straight into the scratch would avoid both the eviction and the memcpy
(the Metal scratch is host-visible). That is the "cold insertion of prefill reads" backlog item; do it as M6 once
parity holds, because it touches the runtime's stream code.

Telemetry: add `pgrn_lm_passes`, `pgrn_lm_held_skips` to `llama_perf_context_data` and one line in the PGRN
print block (llama-context.cpp:5363–5387): `PGRN layer-major: decodes = %d, layer passes = %d, held skips = %llu`.

### 2.7 Failure and abort

If a pre-pass fails or is aborted, layers < il already hold this batch's state data while the metadata says it
was not processed. Partial removal is not possible for recurrent state, so `lm_fail` does `mctx->lm_rewind()`,
`memory->seq_rm(seq, -1, -1)` for the batch's sequence (the whole sequence, not the suffix), releases the hold, and
returns 2 (aborted) or −3 (failed). The server then has no cached prefix for that slot and reprocesses on the
next request. The normal path's suffix removal (lines 2412–2432) stays for failures in the final pass, where the
state is consistent up to the failed ubatch only for the last layer. Simplest: use the full-sequence removal for
any failure while `lm` is active.

### 2.8 Batch size: layer-major needs the prompt in one decode call

The server feeds at most `n_batch` tokens per `llama_decode` (server-context.cpp:4532 loop, `batch.size() < n_batch`),
and the app passes `--batch-size 2048 --ubatch-size 2048` (app/src-tauri/src/main.rs:413–414, 1273–1274), so
today decode() never sees more than one ubatch. Layer-major needs `-b ≥ prompt` (e.g. `-b 40960 -ub 2048`;
llama.cpp caps `n_batch` at `n_ctx` for causal models, llama-context.cpp:351). `n_batch` does not size the compute
buffer (that is `n_ubatch`); it sizes `lm_resid` (320 MiB at 40,960) and host batch arrays (small).

The server splits a hybrid model's prompt at checkpoint offsets anyway (server-context.cpp:4590–4605: at
`n − (4 + n_ubatch)` and `n − 4`, plus user-message starts when chat spans exist). For the 39,515-token prompt
the calls become 37,463 (19 ubatches, layer-major) + 2,048 (1 ubatch, normal) + 4 (normal). The ubatch boundaries
are the same as with `-b 2048` (2048-aligned), so parity against today's server holds. The 2,048-token chunk
costs one full expert pass (~17.5 GiB, ~6 s); dropping the `4 + n_ubatch` checkpoint when layer-major is on is a
possible server tweak (M4b, trade-off: that checkpoint lets a request that edits the prompt's last ubatch reuse
the state).

Progress: one decode call means no server progress lines for ~2.5 min. Cancellation still works (the abort
callback is checked per graph). Prefill progress in the cockpit (backlog) needs a progress callback from
the layer-major loop (`layer passes done / n_layer`) — note it there.

### 2.9 Memory budget (39.5k prompt, ctx 40,960, cache 4 GiB)

| item | today | layer-major v1 |
|---|---:|---:|
| `lm_resid` [2048 × 40,960] F32 (device buffer, counted by admission) | — | 320 MiB |
| compute buffer (n_ubatch = 2048) | unchanged | unchanged (one-layer graphs are a subset) |
| memory metadata snapshot (host) | — | a few MiB |
| scratch (one layer, all 256 experts) | ≈ 0.47–0.5 GiB | same |
| optional M5 second scratch (prefetch next layer) | — | +≈ 0.47–0.5 GiB |
| KV cache, recurrent state, cache partitions | unchanged | unchanged |

0 MB swap growth is an acceptance gate (CLAUDE.md). The app's admission already reserves headroom; v1 adds 0.31
GiB after model load, which the post-context headroom check sees.

## 3. Parity

Claim: with the same `n_ubatch`, the layer-major path produces bit-identical logits and an identical memory state
to the normal path.

1. Same ubatches: both paths use the ubatch list from `init_batch`; the server's chunk boundaries are 2048-aligned
   in both `-b 2048` and `-b 40960` (2.8), so the DeltaNet chunked recurrence sees the same row blocks.
2. Same inputs to each (layer, ubatch) graph: layer 0 reads the token embeddings as today. Layer L > 0 reads the
   F32 rows that layer L−1 wrote with `set_rows`. The normal path keeps the same F32 tensor in the compute buffer.
   Both are exact copies.
3. Same memory inputs: after rewind + apply(0…b) the KV cells, `n_kv`, `n_kv_rows`, `k_idxs`, mask, `s_copy`,
   `rs_z`, `head`, `n` equal the normal path's state when it processes ubatch b (2.2). Same shapes → same kernels
   (FA variant choice depends on shapes).
4. Same K/V and state data: per-layer tensors, written in ubatch order within the layer pass (2.3).
5. Same expert weights: the routed ids come from identical router inputs. At GEMM time every routed slot holds the
   immutable PGRN record, uploaded in this layer's epoch (held bitmap). MUL_MAT_ID never reads unrouted slots.
6. Same output path: the final pass is the existing loop.

Residual risk: Metal op fusion across the cut. In the full graph the layer L−1 output feeds layer L's RMS norm and
residual add directly. In layer-major it goes through `set_rows`/`get_rows`. Today's fusions (consecutive ADDs,
RMS_NORM+MUL(+ADD)) work in F32 registers and store F32, so they round the same way, and the cut does not split a
fusible pair (post_moe ADD → RMS_NORM is not fused). If a mismatch shows up anyway, bisect with the eval callback:
dump `l_out-L` for the last ubatch in both modes and find the first layer that differs.

### Tests

T1 — e2e fixture (CI, `ctest -R test-peregrine-model-e2e`):
- `tests/test-peregrine-model.cpp`: new `--layer-major` mode. `n_ctx 64, n_batch 32, n_ubatch 4`, prompt of 23
  tokens (6 ubatches, ragged last one), every token `output = true`. Four decodes: resident normal, resident
  layer-major, streamed normal, streamed layer-major (toggled through the test-only setter on the context, which
  the test reaches through the src include it already uses). Then one more single-token decode in each to check
  that KV and DeltaNet state carry over. Require `memcmp == 0` on all prompt logits and on the follow-up logits for
  normal vs layer-major (resident and streamed), and nmse = 0 resident vs streamed as today. Also require
  `pgrn_experts_staged(streamed LM) ≤ pgrn_experts_staged(streamed normal)` and `held skips > 0`. Run CPU and
  `--gpu`. Print `PGR_MODEL_PARITY_OK mode=layer_major … max_abs=0`.
- `tests/test-peregrine-model-e2e.py`: add the two invocations (CPU, `--gpu`) for `qwen35moe`.
- Fixture: `tests/test-llama-archs.cpp:324–327` builds qwen35moe with 3 layers (2 trunk + MTP). With the default
  `full_attention_interval = 4` both trunk layers are DeltaNet, so the fixture has no attention layer. Set the
  interval to 2 for this fixture (or `n_layer = 5`) so it covers both layer types (seam patch change; check that the
  existing parity tests still pass).
- `tests/test-peregrine-scratch.cpp`: hold semantics (second stage of the same ids uploads 0 bytes, another layer
  clears the hold, `hold(-1)` restores the old behavior).

T2 — real model, greedy hash (llama-completion, `bench/fetchwall/run_arm.sh`, internal SSD models, `scripts/heavy.sh`):
- run_arm.sh: add `B=${B:-$UB}` and pass `-b "$B"` (today `-b` is tied to `UB`, line 22).
- `PROMPT_FILE=prompt_30k.txt CTX=40960 NTOK=64`, cache 4 GiB, `UB=2048`:
  A `B=2048` (today) · A′ `B=40960` (layer-major off) · B `B=40960 PGRN_LAYER_MAJOR=1`.
  Pass: identical `out=` hash for A, A′, B. Repeat at cache 10 GiB and `UB=4096`, and once with `--pgrn-io-threads 1`.
- The greedy hash alone is weak evidence: ub 2048 and ub 4096 gave the same server hash `54edf738c5a8` although
  their ubatch boundaries differ. So also:

T3 — real model, bitwise logits: `test-peregrine-model MODEL.gguf MODEL.pgrn --layer-major-file prompt_30k.txt
--ctx 40960 --ubatch 2048` tokenizes the file, decodes it with layer-major off, clears the memory, decodes it again
with layer-major on in the same context, and compares the last-token logits with memcmp plus 16 greedy tokens
after the prompt. Print bytes staged in both modes. Heavy job.

T4 — server regime (`run_arm_server.sh`, add the same `B` variable): output hash equal to the `-b 2048` baseline
`54edf738c5a8`; 0 MB swap delta.

## 4. Expected TTFT

Model: TTFT ≈ non-staging time (compute floor + syncs + graph) + fetch + upload + layer-major overhead.
Non-staging is 140.9 s (server, ub 2048, measured above). The SSD rate is 3.1 GiB/s, the upload rate ~36 GiB/s
(344 GB in 9.6 s).

| regime | experts staged | bytes | fetch | upload | LM overhead (est.) | TTFT |
|---|---:|---:|---:|---:|---:|---:|
| server today, ub 2048, 4 GiB (measured) | 189,724 | 336 GiB | 108.1 s | 9.6 s | — | 259.1 s |
| single decode call, layer-major (llama-completion) | ≤ 10,240 | ≤ 18.1 GiB | ~5.8 s | ~0.5 s | 2–6 s | **~149–153 s (−41 %)** |
| server, layer-major + 2048 tail + 4 tail | ~21,200 | ~37.5 GiB | ~12.1 s | ~1.0 s | 2–6 s | **~156–160 s (−38 %)** |
| server, + M4b (no 4+n_ubatch checkpoint) | ~11,300 | ~20 GiB | ~6.5 s | ~0.6 s | 2–6 s | ~150–154 s |
| + M5 next-layer prefetch (I/O hidden) | | | ~0 visible | | | ~143–147 s |

- After this change prefill is compute-bound: about 270–280 tok/s at most with today's kernels (39,515 / 141 s).
  The cache size barely matters for prefill any more (4 vs 10 GiB). ub 4096 gave 207 s; layer-major at ub 4096
  should land near the same compute floor (its non-staging time was ≈ 145 s), so ub 2048 vs 4096 becomes a compute
  question to measure, not an I/O one.
- The two estimates that are least certain: the per-graph overhead (800 builds and syncs; measured in M2) and
  whether the first ubatch of each layer stalls the GPU for the ~0.15 s fetch of ~246 experts (it does in v1:
  ~6 s total, which M5 hides).
- These are hypotheses until they are in bench/RESULTS.md.

## 5. Implementation order (small, verifiable milestones)

Every step: build with `engine-build` (static Metal), run the 14 peregrine tests, sync with `engine-sync`
(scratch files → `engine/`, all upstream files → `patches/slipstream-seams.patch`), fresh `./apply.sh /tmp/x` must
apply and build. Heavy runs through `scripts/heavy.sh`, models from `~/Modelle` only.

**M0 — baseline plumbing (no engine change).** `run_arm.sh` / `run_arm_server.sh`: `B` variable. Measure A vs A′
(`-b 40960`, layer-major off) on prompt_30k: same hash, TTFT within noise. Confirms 2.8's boundary claim and that
`-b 40960` alone costs no memory or swap. Record in RESULTS.md.

**M1 — memory rewind.** `lm_supported` / `lm_rewind` in llama-memory.h, kv-cache, recurrent, hybrid (2.2).
Test (in test-peregrine-model, resident fixture): init a 6-ubatch batch, apply all, rewind, apply all again; the
`state_write_meta` bytes of the memory (or the cells compared field by field) are identical after both rounds, and
`get_n_kv()` per ubatch is identical. No behavior change for normal decode.

**M2 — layer-range graph, no PGRN change.** Params fields, `allow_reuse`, `llm_graph_input_lm_rows`, `lm_resid`
alloc, qwen35moe range build, input guards, `decode_layer_major_prepasses`, eligibility, failure path. With
`PGRN_LAYER_MAJOR=1` the path runs but still re-stages per (layer, ubatch), so I/O equals today's.
Gate: T1 bitwise on resident and streamed fixtures (CPU + GPU). On the real model, T2 hash plus the measured
overhead (TTFT with LM vs without, both at `-b 40960`; should differ only by the graph overhead). If the overhead
is > 3 % of TTFT, plan M6 (k > 1).

**M3 — hold the layer in the scratch.** `pgr_scratch_hold`, filtering in `pgr_scratch_stage`, the forwarder in
llama-model, skipping kicks in `pgrn_eval`, telemetry. Gate: T1 (+ staged-experts invariant), test-peregrine-scratch,
T2 and T3 on prompt_30k: identical hash and logits, experts staged ≤ 10,240 per decode, bytes ≈ 18 GiB.

**M4 — server regime and A/B.** Interleaved A/B (`bench-ab`), ≥ 2 reps, ≥ 25 s cooldown, cache 4 and 10 GiB:
server `-b 2048` today vs `-b 40960 PGRN_LAYER_MAJOR=1`, plus a ub 8192 arm for option (c). Gates: consistent
TTFT win, 0 MB swap, identical hash `54edf738c5a8`, decode tok/s after the prompt not worse (pollution check, E3).
Then the app: when the toggle is on, pass `--batch-size <ctx>` and `PGRN_LAYER_MAJOR=1` (main.rs:413, 1273),
EN+DE label, test contracts. M4b (optional): skip the `4 + n_ubatch` checkpoint in layer-major mode; measure.

**M5 — optional: next-layer prefetch.** A second scratch slab. While layer L runs ubatches 1…N, a background
thread reads all of L+1's experts into it, and the next pass swaps slabs (bind layer L to slab L mod 2, which is
also the k-slab mechanism of (b)). Hides the ~6–12 s of fetch. +0.47 GiB.

**M6 — optional.** (i) Group-major k > 1 with k slabs if M2's overhead justifies it. (ii) Cold insertion: layer-major
reads straight into the scratch and bypasses the cache partition (fixes prefill pollution of the decode-hot set and
saves the memcpy). (iii) Generalize the range graph to the other MoE archs (qwen3moe first) together with the
backlog's generalization item.

## 6. Risks

- Parity at the cut (Metal fusion, 3). Mitigated by T1 bitwise on GPU and by per-layer bisection.
- Unallocated inputs in one-layer graphs → crash in `set_input`. Mitigated by guards plus the fixture covering
  both layer types.
- Graph overhead larger than estimated (800 graphs). Measured in M2; fallback is M6 (k > 1 with slabs).
- Failure or abort mid-prompt loses the sequence's whole cached state (2.7). This only matters for requests that
  are cancelled during prefill.
- Server progress goes silent for the main chunk (2.8); the cockpit needs a progress callback.
- `-b` must grow in the app. If a user's prompt exceeds `n_batch` the server splits it, and each extra chunk costs
  one more expert pass. That is still correct, only slower.
- Upstream drift: the changes touch llama-context, llama-graph, three memory classes and qwen35moe. That is a
  larger seam patch, so it needs care at the next upstream port.
