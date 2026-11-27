#include "common.h"
#include "llama.h"
#include "gguf.h"
#include "llama-model.h"
#include "llama-context.h"
#include "llama-batch.h"
#include "llama-kv-cache.h"
#include "llama-memory-hybrid.h"
#include "llama-memory-recurrent.h"

#include <algorithm>
#include <chrono>
#include <cstring>
#include <fstream>
#include <sstream>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <stdexcept>
#include <string>
#include <vector>
#include <unistd.h>

struct decode_result {
    std::vector<float> logits;
    size_t cache_bytes = 0;
    size_t scratch_bytes = 0;
    uint64_t admitted_resident_bytes = 0;
    int n_layer = 0;
    int n_expert = 0;
    bool had_coupling = false;
};

// Write a PGCC1 coupled table spanning the model's geometry: every source layer/expert
// predicts the first min(n_expert,16) experts. Deliberately generic - it maximally
// exercises the coupled prefetch path so the parity check proves warming never changes
// logits, regardless of which experts actually fire. Returns "" on failure.
static std::string write_coupling_fixture(int n_layer, int n_expert) {
    if (n_layer <= 1 || n_expert <= 0 || n_expert > 0xFFFF) return {};
    std::vector<unsigned char> b;
    auto u16 = [&](unsigned v) { b.push_back(v & 0xff); b.push_back((v >> 8) & 0xff); };
    auto u32 = [&](unsigned v) { for (int i = 0; i < 4; ++i) b.push_back((v >> (8 * i)) & 0xff); };
    const char magic[8] = {'P','G','C','C','1',0,0,0};
    b.insert(b.end(), magic, magic + 8);
    const int src_layers = n_layer - 1;                 // sources 0..n_layer-2 predict L+1
    const int m = n_expert < 16 ? n_expert : 16;
    u32(1); u32((unsigned) src_layers);
    for (int L = 0; L < src_layers; ++L) {
        u16((unsigned) L); u16((unsigned) n_expert);
        for (int e = 0; e < n_expert; ++e) {
            u16((unsigned) e); u16((unsigned) m);
            for (int s = 0; s < m; ++s) { u16((unsigned) s); u16((unsigned) (m - s)); }
        }
    }
    char path[] = "/tmp/pgr_e2e_cpl_XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) return {};
    size_t done = 0;
    while (done < b.size()) { ssize_t n = write(fd, b.data() + done, b.size() - done); if (n <= 0) break; done += (size_t) n; }
    close(fd);
    return done == b.size() ? std::string(path) : std::string();
}

// The fixture's PGRN budget: one fixed slot per streamed layer (the layer-partitioned cache refuses less),
// read from the GGUF block count so the fixture can grow without breaking admission.
static uint64_t fixture_cache_bytes(const char * gguf) {
    uint64_t n_blocks = 3;
    gguf_init_params ip = { /*.no_alloc =*/ true, /*.ctx =*/ nullptr };
    if (gguf_context * g = gguf_init_from_file(gguf, ip)) {
        const int64_t arch_id = gguf_find_key(g, "general.architecture");
        if (arch_id >= 0) {
            const std::string key = std::string(gguf_get_val_str(g, arch_id)) + ".block_count";
            const int64_t id = gguf_find_key(g, key.c_str());
            if (id >= 0) n_blocks = std::max<uint64_t>(n_blocks, gguf_get_val_u32(g, id));
        }
        gguf_free(g);
    }
    return n_blocks * 1179648ULL;
}

static uint64_t gibibytes(const char * text) {
    char * end = nullptr;
    const double value = std::strtod(text, &end);
    if (!end || *end != '\0' || value <= 0.0) throw std::runtime_error("invalid GiB value");
    return (uint64_t) (value * 1024.0 * 1024.0 * 1024.0);
}

static void stream_benchmark(const char * gguf, const char * pgrn, int n_tokens,
        uint64_t cache_bytes, uint64_t headroom_bytes) {
    using clock = std::chrono::steady_clock;
    llama_model_params mp = llama_model_default_params();
    mp.load_mode = LLAMA_LOAD_MODE_NONE;
    mp.pgrn_path = pgrn;
    mp.pgrn_cache_bytes = cache_bytes;
    mp.pgrn_headroom_bytes = headroom_bytes;

    const auto load_start = clock::now();
    llama_model_ptr model(llama_model_load_from_file(gguf, mp));
    const auto load_end = clock::now();
    if (!model || !model->pgrn_enabled()) throw std::runtime_error("PGRN model load failed");
    const pgr_admission_plan * plan = model->pgrn_admission();
    if (!plan || plan->status != PGR_ADMISSION_OK) throw std::runtime_error("PGRN admission failed");

    llama_context_params cp = llama_context_default_params();
    cp.n_ctx = std::max(64, n_tokens + 8);
    cp.n_batch = 8;
    cp.n_ubatch = 8;
    cp.n_threads = 4;
    cp.n_threads_batch = 4;
    llama_context_ptr ctx(llama_init_from_model(model.get(), cp));
    if (!ctx) throw std::runtime_error("context creation failed");

    common_batch prompt(ctx.get());
    for (int i = 0; i < 4; ++i) prompt.add(i + 1, i, 0, i == 3);
    const auto prompt_start = clock::now();
    const int prompt_rc = llama_process(ctx.get(), LLAMA_PROCESS_TYPE_DECODE, prompt.get());
    const auto prompt_end = clock::now();
    if (prompt_rc != 0) throw std::runtime_error("prompt decode failed");
    const llama_perf_context_data prompt_perf = llama_perf_context(ctx.get());

    const auto generation_start = clock::now();
    for (int i = 0; i < n_tokens; ++i) {
        common_batch batch(ctx.get());
        batch.add(1, 4 + i, 0, true);
        const int rc = llama_process(ctx.get(), LLAMA_PROCESS_TYPE_DECODE, batch.get());
        if (rc != 0) throw std::runtime_error("generation decode failed");
    }
    const auto generation_end = clock::now();
    const llama_perf_context_data perf = llama_perf_context(ctx.get());
    if (perf.pgrn_cache_hits + perf.pgrn_cache_misses == 0 || perf.pgrn_experts_staged == 0) {
        throw std::runtime_error("PGRN telemetry remained empty");
    }

    const double load_s = std::chrono::duration<double>(load_end - load_start).count();
    const double prompt_s = std::chrono::duration<double>(prompt_end - prompt_start).count();
    const double generation_s = std::chrono::duration<double>(generation_end - generation_start).count();
    const uint64_t accesses = perf.pgrn_cache_hits + perf.pgrn_cache_misses;
    const double hit_rate = accesses ? 100.0 * perf.pgrn_cache_hits / accesses : 0.0;
    const uint64_t gen_hits = perf.pgrn_cache_hits - prompt_perf.pgrn_cache_hits;
    const uint64_t gen_misses = perf.pgrn_cache_misses - prompt_perf.pgrn_cache_misses;
    const uint64_t gen_accesses = gen_hits + gen_misses;
    const double gen_hit_rate = gen_accesses ? 100.0 * gen_hits / gen_accesses : 0.0;
    std::printf("PGR_REAL_STREAM_OK tokens=%d load_s=%.3f prompt_s=%.3f generation_s=%.3f tok_s=%.3f "
                "hits=%llu misses=%llu hit_rate=%.3f cache=%llu high_water=%llu scratch=%llu "
                "experts=%llu uploaded=%llu topk_ms=%.3f stage_ms=%.3f fetch_ms=%.3f upload_ms=%.3f "
                "gen_hits=%llu gen_misses=%llu gen_hit_rate=%.3f gen_stage_ms=%.3f gen_fetch_ms=%.3f "
                "gen_upload_ms=%.3f admitted=%llu\n",
        n_tokens, load_s, prompt_s, generation_s, n_tokens / generation_s,
        (unsigned long long) perf.pgrn_cache_hits, (unsigned long long) perf.pgrn_cache_misses, hit_rate,
        (unsigned long long) perf.pgrn_cache_bytes, (unsigned long long) perf.pgrn_cache_high_water_bytes,
        (unsigned long long) perf.pgrn_scratch_bytes, (unsigned long long) perf.pgrn_experts_staged,
        (unsigned long long) perf.pgrn_bytes_uploaded, perf.pgrn_topk_read_ms, perf.pgrn_stage_ms,
        perf.pgrn_fetch_ms, perf.pgrn_upload_ms,
        (unsigned long long) gen_hits, (unsigned long long) gen_misses, gen_hit_rate,
        perf.pgrn_stage_ms - prompt_perf.pgrn_stage_ms,
        perf.pgrn_fetch_ms - prompt_perf.pgrn_fetch_ms,
        perf.pgrn_upload_ms - prompt_perf.pgrn_upload_ms,
        (unsigned long long) plan->resident_bytes);
}

static decode_result decode(const char * gguf, const char * pgrn, bool mtp, bool gpu, bool compact,
        const char * coupling_path = nullptr) {
    llama_model_params mp = llama_model_default_params();
    mp.load_mode = pgrn == nullptr ? LLAMA_LOAD_MODE_MMAP : LLAMA_LOAD_MODE_NONE;
    // the streamed load always materializes the NextN/MTP block (the sidecar covers it), so the
    // resident reference does too: both runs then load the same tensor set
    mp.load_mtp = true;
    if (gpu) mp.n_gpu_layers = 99;
    if (pgrn) {
        mp.pgrn_path = pgrn;
        // Layer-partitioned caches require at least one fixed slot per routed
        // layer; keep the tiny fixture budget bounded but above that minimum.
        mp.pgrn_cache_bytes = fixture_cache_bytes(gguf);
        mp.pgrn_headroom_bytes = 1ULL * 1024ULL * 1024ULL * 1024ULL;
        mp.pgrn_compact_slots = compact;
        mp.pgrn_coupling_path = coupling_path;
    }
    llama_model_ptr model(llama_model_load_from_file(gguf, mp));
    if (!model) throw std::runtime_error("model load failed");
    decode_result result;
    result.n_layer = (int) model->hparams.n_layer();
    result.n_expert = (int) model->hparams.n_expert;
    if (pgrn) {
        if (!model->pgrn_enabled()) throw std::runtime_error("PGRN runtime was not enabled");
        result.cache_bytes = model->pgrn_cache_bytes();
        result.scratch_bytes = model->pgrn_scratch_bytes();
        const pgr_admission_plan * plan = model->pgrn_admission();
        if (!plan || plan->status != PGR_ADMISSION_OK || result.cache_bytes == 0 ||
                result.cache_bytes > mp.pgrn_cache_bytes || result.scratch_bytes == 0) {
            throw std::runtime_error("PGRN admitted memory invariants failed");
        }
        result.admitted_resident_bytes = plan->resident_bytes;
        result.had_coupling = model->pgrn_has_coupling();
    }
    llama_context_params cp = llama_context_default_params();
    cp.n_ctx = 32;
    cp.n_batch = 8;
    cp.n_ubatch = 8;
    cp.n_threads = 4;
    cp.n_threads_batch = 4;
    if (mtp) cp.ctx_type = LLAMA_CONTEXT_TYPE_MTP;
    llama_context_ptr ctx(llama_init_from_model(model.get(), cp));
    if (!ctx) throw std::runtime_error("context creation failed");

    const llama_token tokens[] = {1, 2, 3, 4};
    const int token_count = compact ? 1 : 4;
    const int32_t n_embd = llama_model_n_embd(model.get());
    common_batch batch(ctx.get());
    std::vector<float> state;
    if (mtp) state.assign((size_t) token_count * n_embd, 0.0f);
    for (int token = 0; token < token_count; ++token) {
        const int32_t idx = batch.add(tokens[token], token, 0, true);
        if (mtp) {
            for (int32_t dim = 0; dim < n_embd; ++dim) {
                state[(size_t) token * n_embd + dim] = (float) ((dim % 7) - 3) * 1.0e-4f;
            }
            batch.set_embd_state(idx, { state.data() + (size_t) token * n_embd, 1, (size_t) n_embd });
        }
    }
    if (llama_process(ctx.get(), LLAMA_PROCESS_TYPE_DECODE, batch.get()) != 0) {
        throw std::runtime_error("decode failed");
    }
    const llama_perf_context_data perf = llama_perf_context(ctx.get());
    if (pgrn && (perf.pgrn_cache_bytes == 0 || perf.pgrn_cache_high_water_bytes < perf.pgrn_cache_bytes ||
            perf.pgrn_cache_hits + perf.pgrn_cache_misses == 0 || perf.pgrn_experts_staged == 0 ||
            (!compact && perf.pgrn_bytes_uploaded == 0))) {
        throw std::runtime_error("PGRN runtime telemetry invariants failed");
    }
    const int32_t n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(model.get()));
    const float * logits = llama_get_logits_ith(ctx.get(), token_count - 1);
    result.logits.assign(logits, logits + n_vocab);
    return result;
}

// ---------------------------------------------------------------------------------------------------------
// Slipstream layer-major prefill tests
// ---------------------------------------------------------------------------------------------------------

static std::vector<llama_token> lm_fixture_tokens(int n, int n_vocab) {
    std::vector<llama_token> tokens((size_t) n);
    for (int i = 0; i < n; ++i) tokens[(size_t) i] = 1 + (i * 7 + 3) % (n_vocab - 1);
    return tokens;
}

static std::vector<uint8_t> lm_state(llama_context * ctx) {
    std::vector<uint8_t> state(llama_state_get_size(ctx));
    const size_t n = llama_state_get_data(ctx, state.data(), state.size());
    state.resize(n);
    return state;
}

// M1: init a multi-ubatch batch, apply all, rewind, apply all again. The memory metadata (serialized with the
// state; apply() never touches tensor data) and the per-ubatch memory inputs must be identical in both rounds,
// and a final rewind must restore the pre-batch state.
static void lm_rewind_test(llama_context * ctx, const llama_model * model, int n_prompt, uint32_t n_ubatch) {
    llama_memory_clear(llama_get_memory(ctx), true);
    const int n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(model));

    // a short prefix first, so the batch starts on a non-empty memory (recurrent src cells, KV head)
    {
        common_batch prefix(ctx);
        for (int i = 0; i < 3; ++i) prefix.add(1 + i, i, 0, i == 2);
        if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, prefix.get()) != 0) throw std::runtime_error("prefix decode failed");
    }
    const std::vector<uint8_t> state0 = lm_state(ctx);

    common_batch batch(ctx);
    const auto tokens = lm_fixture_tokens(n_prompt, n_vocab);
    for (int i = 0; i < n_prompt; ++i) batch.add(tokens[(size_t) i], 3 + i, 0, true);

    llama_batch_allocr balloc(model->hparams.n_pos_per_embd());
    if (!balloc.init(*batch.get(), model->vocab, false)) throw std::runtime_error("balloc init failed");
    llama_memory_i * mem = llama_get_memory(ctx);
    auto mctx = mem->init_batch(balloc, n_ubatch, false);
    if (!mctx || mctx->get_status() != LLAMA_MEMORY_STATUS_SUCCESS) throw std::runtime_error("init_batch failed");
    if (!mctx->lm_supported()) throw std::runtime_error("memory context does not support layer-major rewind");
    auto * hctx = dynamic_cast<llama_memory_hybrid_context *>(mctx.get());
    if (!hctx) throw std::runtime_error("expected a hybrid memory context");

    struct inputs { uint32_t n_kv, n_kv_rows, n_rs, head; int32_t rs_z; std::vector<int32_t> s_copy; };
    auto round = [&](std::vector<inputs> & out) {
        do {
            if (!mctx->apply()) throw std::runtime_error("apply failed");
            inputs in{};
            in.n_kv      = hctx->get_attn()->get_n_kv();
            in.n_kv_rows = hctx->get_attn()->get_n_kv_rows();
            in.n_rs      = hctx->get_recr()->get_n_rs();
            in.head      = hctx->get_recr()->get_head();
            in.rs_z      = hctx->get_recr()->get_rs_z();
            for (uint32_t i = 0; i < in.n_rs; ++i) in.s_copy.push_back(hctx->get_recr()->s_copy((int) i));
            out.push_back(std::move(in));
        } while (mctx->next());
    };

    mctx->lm_snapshot();
    std::vector<inputs> r1, r2, r3;
    round(r1);
    const std::vector<uint8_t> state1 = lm_state(ctx);
    mctx->lm_rewind();
    const std::vector<uint8_t> state_rw = lm_state(ctx);
    round(r2);
    const std::vector<uint8_t> state2 = lm_state(ctx);
    mctx->lm_rewind();
    round(r3);
    const std::vector<uint8_t> state3 = lm_state(ctx);

    if (r1.size() < 2) throw std::runtime_error("rewind test needs at least two ubatches");
    auto same = [](const std::vector<inputs> & a, const std::vector<inputs> & b) {
        if (a.size() != b.size()) return false;
        for (size_t i = 0; i < a.size(); ++i) {
            if (a[i].n_kv != b[i].n_kv || a[i].n_kv_rows != b[i].n_kv_rows || a[i].n_rs != b[i].n_rs ||
                    a[i].head != b[i].head || a[i].rs_z != b[i].rs_z || a[i].s_copy != b[i].s_copy) return false;
        }
        return true;
    };
    if (!same(r1, r2) || !same(r1, r3)) throw std::runtime_error("per-ubatch memory inputs differ after rewind");
    if (state1 != state2 || state1 != state3) throw std::runtime_error("memory metadata differs after rewind + re-apply");
    if (state_rw != state0) throw std::runtime_error("rewind did not restore the pre-batch memory metadata");
    if (state1 == state0) throw std::runtime_error("apply did not change the memory metadata (test is vacuous)");
    std::printf("PGR_LM_REWIND_OK ubatches=%zu n_kv_first=%u n_kv_last=%u state_bytes=%zu\n",
            r1.size(), r1.front().n_kv, r1.back().n_kv, state1.size());
    mctx.reset();
    llama_memory_clear(llama_get_memory(ctx), true);
}

struct lm_run {
    std::vector<float> prompt_logits;
    std::vector<float> follow_logits;
    std::vector<uint8_t> state;
    std::vector<uint8_t> state_seq;
    uint64_t staged = 0, uploaded = 0, lm_decodes = 0, lm_passes = 0, skips = 0;
};

static std::vector<uint8_t> lm_state_seq(llama_context * ctx, llama_seq_id seq) {
    std::vector<uint8_t> state(llama_state_seq_get_size(ctx, seq));
    const size_t n = llama_state_seq_get_data(ctx, state.data(), state.size(), seq);
    state.resize(n);
    return state;
}

// n_prefix > 0: decode a prefix normally first, so the (layer-major) batch starts on a non-empty memory
static lm_run lm_decode(llama_context * ctx, const llama_model * model, bool layer_major, int n_prompt, int n_prefix = 0) {
    llama_memory_clear(llama_get_memory(ctx), true);
    const int n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(model));
    const auto tokens = lm_fixture_tokens(n_prefix + n_prompt, n_vocab);
    if (n_prefix > 0) {
        if (!ctx->set_layer_major(false)) throw std::runtime_error("set_layer_major failed");
        common_batch prefix(ctx);
        for (int i = 0; i < n_prefix; ++i) prefix.add(tokens[(size_t) i], i, 0, i + 1 == n_prefix);
        if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, prefix.get()) != 0) throw std::runtime_error("prefix decode failed");
    }
    if (!ctx->set_layer_major(layer_major)) throw std::runtime_error("set_layer_major failed");
    lm_run run;
    const llama_perf_context_data p0 = llama_perf_context(ctx);
    {
        common_batch prompt(ctx);
        for (int i = 0; i < n_prompt; ++i) prompt.add(tokens[(size_t) (n_prefix + i)], n_prefix + i, 0, true);
        if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, prompt.get()) != 0) throw std::runtime_error("prompt decode failed");
    }
    const llama_perf_context_data p1 = llama_perf_context(ctx);
    for (int i = 0; i < n_prompt; ++i) {
        const float * logits = llama_get_logits_ith(ctx, i);
        run.prompt_logits.insert(run.prompt_logits.end(), logits, logits + n_vocab);
    }
    run.state     = lm_state(ctx);
    run.state_seq = lm_state_seq(ctx, 0);
    {
        common_batch next(ctx);
        next.add(5, n_prefix + n_prompt, 0, true);
        if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, next.get()) != 0) throw std::runtime_error("follow-up decode failed");
        const float * logits = llama_get_logits_ith(ctx, 0);
        run.follow_logits.assign(logits, logits + n_vocab);
    }
    run.staged     = p1.pgrn_experts_staged - p0.pgrn_experts_staged;
    run.uploaded   = p1.pgrn_bytes_uploaded - p0.pgrn_bytes_uploaded;
    run.lm_decodes = p1.pgrn_lm_decodes - p0.pgrn_lm_decodes;
    run.lm_passes  = p1.pgrn_lm_passes - p0.pgrn_lm_passes;
    run.skips      = p1.pgrn_lm_held_skips - p0.pgrn_lm_held_skips;
    return run;
}

static bool lm_bitwise(const std::vector<float> & a, const std::vector<float> & b) {
    return a.size() == b.size() && std::memcmp(a.data(), b.data(), a.size() * sizeof(float)) == 0;
}

static double lm_max_abs(const std::vector<float> & a, const std::vector<float> & b) {
    double m = a.size() == b.size() ? 0.0 : INFINITY;
    for (size_t i = 0; i < a.size() && i < b.size(); ++i) m = std::max(m, std::fabs((double) a[i] - b[i]));
    return m;
}

// T2: cancellation. The layer-major abort check fires before step `fire_after` (one step = one (layer, ubatch)
// graph); the decode must return 2, the memory must be either untouched (abort before the first graph) or the
// sequence dropped, and the next decode on the same context must match the uninterrupted normal run bitwise.
struct lm_abort_state { int calls = 0; int fire_after = 0; };
static bool lm_abort_cb(void * data) {
    auto * st = static_cast<lm_abort_state *>(data);
    return st->calls++ >= st->fire_after;
}

struct lm_abort_case { std::string what; bool ok; };

static void lm_abort_test(llama_context * ctx, const llama_model * model, int n_prompt, int n_prefix, int fire_after,
        const lm_run & normal, std::vector<lm_abort_case> & out) {
    const int n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(model));
    const auto tokens = lm_fixture_tokens(n_prefix + n_prompt, n_vocab);
    llama_memory_t mem = llama_get_memory(ctx);
    llama_memory_clear(mem, true);
    if (n_prefix > 0) {
        if (!ctx->set_layer_major(false)) throw std::runtime_error("set_layer_major failed");
        common_batch prefix(ctx);
        for (int i = 0; i < n_prefix; ++i) prefix.add(tokens[(size_t) i], i, 0, i + 1 == n_prefix);
        if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, prefix.get()) != 0) throw std::runtime_error("prefix decode failed");
    }
    const std::vector<uint8_t> before = lm_state_seq(ctx, 0);
    if (!ctx->set_layer_major(true)) throw std::runtime_error("set_layer_major failed");
    lm_abort_state st;
    st.fire_after = fire_after;
    llama_set_layer_major_abort_callback(ctx, lm_abort_cb, &st);
    common_batch prompt(ctx);
    for (int i = 0; i < n_prompt; ++i) prompt.add(tokens[(size_t) (n_prefix + i)], n_prefix + i, 0, true);
    const int ret = llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, prompt.get());
    llama_set_layer_major_abort_callback(ctx, nullptr, nullptr);
    const std::string tag = " (prefix " + std::to_string(n_prefix) + ", abort before step " + std::to_string(fire_after) + ")";
    out.push_back({ "abort returns 2" + tag, ret == 2 });
    out.push_back({ "abort fired exactly once" + tag, st.calls == fire_after + 1 });
    const llama_pos pmax = llama_memory_seq_pos_max(mem, 0);
    if (fire_after == 0) {
        // nothing ran: the memory must be exactly as before, so the prompt continues on the kept prefix
        out.push_back({ "untouched memory kept" + tag, pmax == n_prefix - 1 && lm_state_seq(ctx, 0) == before });
    } else {
        out.push_back({ "touched sequence dropped" + tag, pmax == -1 });
        llama_memory_clear(mem, true);
        if (n_prefix > 0) {
            if (!ctx->set_layer_major(false)) throw std::runtime_error("set_layer_major failed");
            common_batch prefix(ctx);
            for (int i = 0; i < n_prefix; ++i) prefix.add(tokens[(size_t) i], i, 0, i + 1 == n_prefix);
            if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, prefix.get()) != 0) throw std::runtime_error("prefix decode failed");
            if (!ctx->set_layer_major(true)) throw std::runtime_error("set_layer_major failed");
        }
    }
    // the retry (layer-major again, no abort) must equal the uninterrupted normal run
    if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, prompt.get()) != 0) throw std::runtime_error("retry decode failed");
    std::vector<float> logits;
    for (int i = 0; i < n_prompt; ++i) {
        const float * l = llama_get_logits_ith(ctx, i);
        logits.insert(logits.end(), l, l + n_vocab);
    }
    out.push_back({ "retry prompt logits == normal" + tag, lm_bitwise(logits, normal.prompt_logits) });
    out.push_back({ "retry seq state == normal" + tag, lm_state_seq(ctx, 0) == normal.state_seq });
    llama_memory_clear(mem, true);
}

// T1: normal vs layer-major prefill on the fixture, resident and streamed, every token an output; once on an
// empty memory and once after a normally decoded prefix (non-empty KV and recurrent state)
static int layer_major_fixture(const char * gguf, const char * pgrn, bool gpu) {
    const int n_prompt = 23;          // n_ubatch 4 -> 6 ubatches, ragged last one
    const uint32_t n_ubatch = 4;
    const int prefixes[] = { 0, 5 };
    lm_run res_n[2], res_l[2], str_n[2], str_l[2];
    std::vector<lm_abort_case> aborts;
    int n_attn = 0, n_recr = 0, n_layer = 0;
    for (int streamed = 0; streamed < 2; ++streamed) {
        llama_model_params mp = llama_model_default_params();
        mp.load_mode = streamed ? LLAMA_LOAD_MODE_NONE : LLAMA_LOAD_MODE_MMAP;
        mp.load_mtp = true;
        if (gpu) mp.n_gpu_layers = 99;
        if (streamed) {
            mp.pgrn_path = pgrn;
            mp.pgrn_cache_bytes = fixture_cache_bytes(gguf);
            mp.pgrn_headroom_bytes = 1ULL * 1024ULL * 1024ULL * 1024ULL;
        }
        llama_model_ptr model(llama_model_load_from_file(gguf, mp));
        if (!model) throw std::runtime_error("model load failed");
        if (streamed && !model->pgrn_enabled()) throw std::runtime_error("PGRN runtime was not enabled");
        if (!model->supports_layer_range()) throw std::runtime_error("arch has no layer-range graph");
        n_attn = n_recr = 0;
        n_layer = (int) model->hparams.n_layer();
        for (int il = 0; il < n_layer; ++il) (model->hparams.is_recr(il) ? n_recr : n_attn)++;
        if (n_attn == 0 || n_recr == 0) throw std::runtime_error("fixture must cover attention and recurrent layers");
        if (n_layer < 3) throw std::runtime_error("fixture needs >= 3 trunk layers to cover a middle layer-range graph");
        llama_context_params cp = llama_context_default_params();
        cp.n_ctx = 64;
        cp.n_batch = 32;
        cp.n_ubatch = n_ubatch;
        cp.n_threads = 4;
        cp.n_threads_batch = 4;
        llama_context_ptr ctx(llama_init_from_model(model.get(), cp));
        if (!ctx) throw std::runtime_error("context creation failed");
        if (!streamed) lm_rewind_test(ctx.get(), model.get(), n_prompt, n_ubatch);
        for (int p = 0; p < 2; ++p) {
            lm_run & n = streamed ? str_n[p] : res_n[p];
            lm_run & l = streamed ? str_l[p] : res_l[p];
            n = lm_decode(ctx.get(), model.get(), false, n_prompt, prefixes[p]);
            l = lm_decode(ctx.get(), model.get(), true, n_prompt, prefixes[p]);
            if (n.lm_decodes != 0 || l.lm_decodes != 1) throw std::runtime_error("layer-major path did not run exactly once");
            if ((int) l.lm_passes != n_layer - 1) throw std::runtime_error("unexpected number of layer passes");
        }
        // cancellation: before the first graph, mid pre-pass (layer 1, ubatch 2) and in the final pass
        const int n_ub = (n_prompt + (int) n_ubatch - 1) / (int) n_ubatch;
        for (int p = 0; p < 2; ++p) {
            const lm_run & n = streamed ? str_n[p] : res_n[p];
            for (int fire : { 0, n_ub + 2, n_ub * (n_layer - 1) + 1 }) {
                lm_abort_test(ctx.get(), model.get(), n_prompt, prefixes[p], fire, n, aborts);
            }
        }
    }
    struct check { std::string what; bool ok; };
    std::vector<check> checks;
    double max_abs = 0.0, max_abs_follow = 0.0, res_vs_str = 0.0;
    for (int p = 0; p < 2; ++p) {
        const std::string tag = " (prefix " + std::to_string(prefixes[p]) + ")";
        checks.push_back({ "resident prompt logits" + tag,   lm_bitwise(res_n[p].prompt_logits, res_l[p].prompt_logits) });
        checks.push_back({ "resident follow logits" + tag,   lm_bitwise(res_n[p].follow_logits, res_l[p].follow_logits) });
        checks.push_back({ "resident memory state" + tag,    res_n[p].state == res_l[p].state });
        checks.push_back({ "resident seq state" + tag,       !res_n[p].state_seq.empty() && res_n[p].state_seq == res_l[p].state_seq });
        checks.push_back({ "streamed prompt logits" + tag,   lm_bitwise(str_n[p].prompt_logits, str_l[p].prompt_logits) });
        checks.push_back({ "streamed follow logits" + tag,   lm_bitwise(str_n[p].follow_logits, str_l[p].follow_logits) });
        checks.push_back({ "streamed memory state" + tag,    str_n[p].state == str_l[p].state });
        checks.push_back({ "streamed seq state" + tag,       !str_n[p].state_seq.empty() && str_n[p].state_seq == str_l[p].state_seq });
        checks.push_back({ "staged(LM) <= staged(normal)" + tag, str_l[p].staged <= str_n[p].staged });
        checks.push_back({ "held skips > 0" + tag,           str_l[p].skips > 0 });
        max_abs = std::max({ max_abs, lm_max_abs(res_n[p].prompt_logits, res_l[p].prompt_logits),
                             lm_max_abs(str_n[p].prompt_logits, str_l[p].prompt_logits) });
        max_abs_follow = std::max({ max_abs_follow, lm_max_abs(res_n[p].follow_logits, res_l[p].follow_logits),
                                    lm_max_abs(str_n[p].follow_logits, str_l[p].follow_logits) });
        res_vs_str = std::max(res_vs_str, lm_max_abs(res_l[p].prompt_logits, str_l[p].prompt_logits));
    }
    bool ok = true;
    for (const auto & c : checks) {
        if (!c.ok) { std::fprintf(stderr, "PGR_LM_CHECK_FAIL: %s\n", c.what.c_str()); ok = false; }
    }
    bool abort_ok = !aborts.empty();
    for (const auto & c : aborts) {
        if (!c.ok) { std::fprintf(stderr, "PGR_LM_ABORT_FAIL: %s\n", c.what.c_str()); abort_ok = false; }
    }
    std::printf("PGR_LM_ABORT_%s checks=%zu\n", abort_ok ? "OK" : "FAIL", aborts.size());
    ok = ok && abort_ok;
    std::printf("PGR_MODEL_PARITY_OK mode=layer_major device=%s layers=%d attn=%d recr=%d prompt=%d ubatch=%u prefixes=0,5 "
                "checks=%zu max_abs=%.9g max_abs_follow=%.9g resident_vs_streamed=%.9g staged_normal=%llu staged_lm=%llu "
                "held_skips=%llu bitwise=%s\n",
        gpu ? "gpu" : "default", n_layer, n_attn, n_recr, n_prompt, n_ubatch, checks.size(), max_abs, max_abs_follow,
        res_vs_str, (unsigned long long) str_n[0].staged, (unsigned long long) str_l[0].staged,
        (unsigned long long) str_l[0].skips, ok ? "yes" : "no");
    return ok && res_vs_str == 0.0 ? 0 : 1;
}

// T3: the real model, one long prompt decoded in ONE decode call with layer-major off, then on, in the same
// context; last-token logits compared bitwise plus a greedy continuation
static int layer_major_file(const char * gguf, const char * pgrn, const char * prompt_file, int n_ctx, int n_ubatch,
        uint64_t cache_bytes, int n_gen) {
    using clock = std::chrono::steady_clock;
    llama_model_params mp = llama_model_default_params();
    mp.load_mode = LLAMA_LOAD_MODE_NONE;
    mp.n_gpu_layers = 99;
    mp.pgrn_path = pgrn;
    mp.pgrn_cache_bytes = cache_bytes;
    mp.pgrn_io_threads = 8;
    llama_model_ptr model(llama_model_load_from_file(gguf, mp));
    if (!model || !model->pgrn_enabled()) throw std::runtime_error("PGRN model load failed");
    std::ifstream in(prompt_file);
    if (!in) throw std::runtime_error("cannot read the prompt file");
    std::stringstream ss; ss << in.rdbuf();
    const auto tokens = common_tokenize(llama_model_get_vocab(model.get()), ss.str(), true, true);
    if ((int) tokens.size() + n_gen > n_ctx) throw std::runtime_error("prompt does not fit the context");
    llama_context_params cp = llama_context_default_params();
    cp.n_ctx = n_ctx;
    cp.n_batch = n_ctx;
    cp.n_ubatch = n_ubatch;
    llama_context_ptr ctx(llama_init_from_model(model.get(), cp));
    if (!ctx) throw std::runtime_error("context creation failed");
    const int n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(model.get()));
    struct out { std::vector<float> last; std::vector<llama_token> gen; std::vector<uint8_t> state_seq; double prompt_s; uint64_t staged, uploaded, skips; };
    auto run = [&](bool lm) {
        llama_memory_clear(llama_get_memory(ctx.get()), true);
        if (!ctx->set_layer_major(lm)) throw std::runtime_error("set_layer_major failed");
        out o{};
        const auto p0 = llama_perf_context(ctx.get());
        common_batch batch(ctx.get());
        for (size_t i = 0; i < tokens.size(); ++i) batch.add(tokens[i], (llama_pos) i, 0, i + 1 == tokens.size());
        const auto t0 = clock::now();
        if (llama_process(ctx.get(), LLAMA_PROCESS_TYPE_DECODE, batch.get()) != 0) throw std::runtime_error("prompt decode failed");
        const float * logits = llama_get_logits_ith(ctx.get(), -1);
        o.last.assign(logits, logits + n_vocab);
        o.prompt_s = std::chrono::duration<double>(clock::now() - t0).count();
        o.state_seq = lm_state_seq(ctx.get(), 0);   // serialized KV + recurrent state right after the prompt
        const auto p1 = llama_perf_context(ctx.get());
        if (lm && p1.pgrn_lm_decodes != p0.pgrn_lm_decodes + 1) throw std::runtime_error("layer-major path did not run");
        o.staged = p1.pgrn_experts_staged - p0.pgrn_experts_staged;
        o.uploaded = p1.pgrn_bytes_uploaded - p0.pgrn_bytes_uploaded;
        o.skips = p1.pgrn_lm_held_skips - p0.pgrn_lm_held_skips;
        llama_token tok = (llama_token) (std::max_element(o.last.begin(), o.last.end()) - o.last.begin());
        for (int g = 0; g < n_gen; ++g) {
            o.gen.push_back(tok);
            common_batch next(ctx.get());
            next.add(tok, (llama_pos) (tokens.size() + g), 0, true);
            if (llama_process(ctx.get(), LLAMA_PROCESS_TYPE_DECODE, next.get()) != 0) throw std::runtime_error("generation failed");
            const float * l = llama_get_logits_ith(ctx.get(), -1);
            tok = (llama_token) (std::max_element(l, l + n_vocab) - l);
        }
        return o;
    };
    const out a = run(false);
    const out b = run(true);
    const bool bitwise = lm_bitwise(a.last, b.last);
    const bool same_gen = a.gen == b.gen;
    const bool same_state = !a.state_seq.empty() && a.state_seq == b.state_seq;
    std::printf("PGR_LM_FILE tokens=%zu ubatch=%d cache_gib=%.2f bitwise_last_logits=%s max_abs=%.9g greedy_%d_equal=%s "
                "seq_state_equal=%s seq_state_mib=%.1f "
                "normal_prompt_s=%.2f lm_prompt_s=%.2f normal_staged=%llu lm_staged=%llu normal_mib=%.0f lm_mib=%.0f held_skips=%llu\n",
        tokens.size(), n_ubatch, cache_bytes / 1073741824.0, bitwise ? "yes" : "no", lm_max_abs(a.last, b.last), n_gen,
        same_gen ? "yes" : "no", same_state ? "yes" : "no", a.state_seq.size() / 1048576.0,
        a.prompt_s, b.prompt_s, (unsigned long long) a.staged, (unsigned long long) b.staged,
        a.uploaded / 1048576.0, b.uploaded / 1048576.0, (unsigned long long) b.skips);
    return bitwise && same_gen && same_state ? 0 : 1;
}

int main(int argc, char ** argv) {
    if (argc == 7 && std::string(argv[3]) == "--stream-only") {
        common_init();
        try {
            const int n_tokens = std::atoi(argv[4]);
            if (n_tokens <= 0) throw std::runtime_error("token count must be positive");
            stream_benchmark(argv[1], argv[2], n_tokens, gibibytes(argv[5]), gibibytes(argv[6]));
            return 0;
        } catch (const std::exception & error) {
            std::fprintf(stderr, "PGR_REAL_STREAM_FAIL: %s\n", error.what());
            return 1;
        }
    }
    if (argc >= 4 && std::string(argv[3]) == "--layer-major") {
        common_init();
        try {
            const bool gpu = argc >= 5 && std::string(argv[4]) == "--gpu";
            return layer_major_fixture(argv[1], argv[2], gpu);
        } catch (const std::exception & error) {
            std::fprintf(stderr, "PGR_MODEL_PARITY_FAIL: %s\n", error.what());
            return 1;
        }
    }
    if (argc >= 9 && std::string(argv[3]) == "--layer-major-file") {
        // MODEL.gguf MODEL.pgrn --layer-major-file PROMPT CTX UBATCH CACHE_GIB NGEN
        common_init();
        try {
            return layer_major_file(argv[1], argv[2], argv[4], std::atoi(argv[5]), std::atoi(argv[6]),
                    gibibytes(argv[7]), std::atoi(argv[8]));
        } catch (const std::exception & error) {
            std::fprintf(stderr, "PGR_LM_FILE_FAIL: %s\n", error.what());
            return 1;
        }
    }
    if (argc < 3 || argc > 7) {
        std::fprintf(stderr, "usage: %s MODEL.gguf MODEL.pgrn [--mtp] [--gpu] [--compact] [--coupling]\n"
                             "       %s MODEL.gguf MODEL.pgrn --stream-only TOKENS CACHE_GIB HEADROOM_GIB\n",
            argv[0], argv[0]);
        return 2;
    }
    bool mtp = false;
    bool gpu = false;
    bool compact = false;
    bool coupling = false;
    for (int index = 3; index < argc; ++index) {
        const std::string option(argv[index]);
        if (option == "--mtp" && !mtp) mtp = true;
        else if (option == "--gpu" && !gpu) gpu = true;
        else if (option == "--compact" && !compact) compact = true;
        else if (option == "--coupling" && !coupling) coupling = true;
        else return 2;
    }
    common_init();
    std::string coupling_file;
    try {
        const decode_result resident = decode(argv[1], nullptr, mtp, gpu, compact);
        if (coupling) {
            coupling_file = write_coupling_fixture(resident.n_layer, resident.n_expert);
            if (coupling_file.empty()) throw std::runtime_error("failed to write coupling fixture");
        }
        const decode_result streamed = decode(argv[1], argv[2], mtp, gpu, compact,
                coupling_file.empty() ? nullptr : coupling_file.c_str());
        // Fail closed: --coupling must actually load the table, else nmse=0 proves nothing
        // about the coupled path (the kick would silently no-op).
        if (coupling && !streamed.had_coupling) throw std::runtime_error("coupling table failed to load");
        if (resident.logits.size() != streamed.logits.size()) throw std::runtime_error("logit size mismatch");
        double squared_error = 0.0;
        double squared_ref = 0.0;
        double max_abs = 0.0;
        for (size_t i = 0; i < resident.logits.size(); ++i) {
            const double error = (double) resident.logits[i] - streamed.logits[i];
            squared_error += error * error;
            squared_ref += (double) resident.logits[i] * resident.logits[i];
            max_abs = std::max(max_abs, std::fabs(error));
        }
        const double nmse = squared_ref > 0.0 ? squared_error / squared_ref : squared_error;
        std::printf("PGR_MODEL_PARITY_OK mode=%s device=%s compact=%s coupling=%s nmse=%.9g max_abs=%.9g logits=%zu cache=%zu scratch=%zu admitted=%llu\n",
            mtp ? "mtp" : "decoder", gpu ? "gpu" : "default", compact ? "yes" : "no", coupling ? "yes" : "no",
            nmse, max_abs,
            resident.logits.size(), streamed.cache_bytes, streamed.scratch_bytes,
            (unsigned long long) streamed.admitted_resident_bytes);
        if (!coupling_file.empty()) unlink(coupling_file.c_str());
        return nmse <= 1e-8 && max_abs <= 1e-5 ? 0 : 1;
    } catch (const std::exception & error) {
        if (!coupling_file.empty()) unlink(coupling_file.c_str());
        std::fprintf(stderr, "PGR_MODEL_PARITY_FAIL: %s\n", error.what());
        return 1;
    }
}
