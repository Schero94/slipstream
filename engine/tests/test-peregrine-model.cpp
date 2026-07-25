#include "common.h"
#include "llama.h"
#include "llama-model.h"

#include <algorithm>
#include <chrono>
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
    mp.use_mmap = false;
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

    llama_batch prompt = llama_batch_init(4, 0, 1);
    for (int i = 0; i < 4; ++i) common_batch_add(prompt, i + 1, i, {0}, i == 3);
    const auto prompt_start = clock::now();
    const int prompt_rc = llama_decode(ctx.get(), prompt);
    const auto prompt_end = clock::now();
    llama_batch_free(prompt);
    if (prompt_rc != 0) throw std::runtime_error("prompt decode failed");
    const llama_perf_context_data prompt_perf = llama_perf_context(ctx.get());

    const auto generation_start = clock::now();
    for (int i = 0; i < n_tokens; ++i) {
        llama_batch batch = llama_batch_init(1, 0, 1);
        common_batch_add(batch, 1, 4 + i, {0}, true);
        const int rc = llama_decode(ctx.get(), batch);
        llama_batch_free(batch);
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
    mp.use_mmap = pgrn == nullptr;
    if (gpu) mp.n_gpu_layers = 99;
    if (pgrn) {
        mp.pgrn_path = pgrn;
        // Layer-partitioned caches require at least one fixed slot per routed
        // layer; keep the tiny fixture budget bounded but above that minimum.
        mp.pgrn_cache_bytes = 3ULL * 1179648ULL;
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
    llama_batch batch = llama_batch_init(token_count, mtp ? n_embd : 0, 1);
    if (mtp) {
        batch.token = (llama_token *) std::malloc(sizeof(llama_token) * token_count);
        if (!batch.token) {
            llama_batch_free(batch);
            throw std::runtime_error("MTP token allocation failed");
        }
    }
    for (int i = 0; i < token_count; ++i) common_batch_add(batch, tokens[i], i, {0}, true);
    if (mtp) {
        for (int token = 0; token < token_count; ++token) {
            for (int32_t dim = 0; dim < n_embd; ++dim) {
                batch.embd[(size_t) token * n_embd + dim] = (float) ((dim % 7) - 3) * 1.0e-4f;
            }
        }
    }
    if (llama_decode(ctx.get(), batch) != 0) {
        llama_batch_free(batch);
        throw std::runtime_error("decode failed");
    }
    const llama_perf_context_data perf = llama_perf_context(ctx.get());
    if (pgrn && (perf.pgrn_cache_bytes == 0 || perf.pgrn_cache_high_water_bytes < perf.pgrn_cache_bytes ||
            perf.pgrn_cache_hits + perf.pgrn_cache_misses == 0 || perf.pgrn_experts_staged == 0 ||
            (!compact && perf.pgrn_bytes_uploaded == 0))) {
        llama_batch_free(batch);
        throw std::runtime_error("PGRN runtime telemetry invariants failed");
    }
    const int32_t n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(model.get()));
    const float * logits = llama_get_logits_ith(ctx.get(), token_count - 1);
    result.logits.assign(logits, logits + n_vocab);
    llama_batch_free(batch);
    return result;
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
