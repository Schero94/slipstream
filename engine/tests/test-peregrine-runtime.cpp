#include "../src/peregrine_runtime.h"
#include "../src/peregrine_compact.h"

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <unistd.h>
#include <vector>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)
static constexpr uint64_t GiB = 1024ULL * 1024ULL * 1024ULL;
static constexpr size_t ALIGN = 16384, REC = 4096, DIR_REC = 26, COUNT = 6;
static constexpr uint64_t DIR_OFF = 8 * ALIGN;
static const char * SHA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

static uint32_t crc32_bytes(const unsigned char * data, size_t size) {
    uint32_t crc = 0xffffffffU;
    for (size_t i = 0; i < size; ++i) { crc ^= data[i]; for (int bit = 0; bit < 8; ++bit) crc = (crc >> 1) ^ (0xedb88320U & (0U - (crc & 1U))); }
    return crc ^ 0xffffffffU;
}
static void put16(unsigned char * p, uint16_t v) { p[0] = v & 0xff; p[1] = v >> 8; }
static void put32(unsigned char * p, uint32_t v) { for (int i = 0; i < 4; ++i) p[i] = (unsigned char)(v >> (8 * i)); }
static void put64(unsigned char * p, uint64_t v) { for (int i = 0; i < 8; ++i) p[i] = (unsigned char)(v >> (8 * i)); }
static void put_float(unsigned char * p, float value) { uint32_t bits = 0; std::memcpy(&bits, &value, sizeof(bits)); put32(p, bits); }

static std::string fixture() {
    const std::string json = std::string("{\"metadata\":{\"model_sha256\":\"") + SHA +
        "\",\"geometry\":{\"layers_with_experts\":2,\"experts_per_layer\":3},"
        "\"tensor_directory\":[[0,12,1024,12,1024,12,2048],[1,12,1024,12,1024,12,2048]]},"
        "\"expert_count\":6,\"expert_dir_offset\":131072}";
    std::vector<unsigned char> bytes(DIR_OFF + COUNT * DIR_REC, 0);
    std::memcpy(bytes.data(), "PGRN1\0\0\0", 8); put32(bytes.data() + 8, 1);
    put32(bytes.data() + 12, (uint32_t)json.size()); std::memcpy(bytes.data() + 16, json.data(), json.size());
    for (size_t expert = 0; expert < COUNT; ++expert) {
        unsigned char * blob = bytes.data() + (expert + 1) * ALIGN;
        std::memset(blob, (int)(expert + 1), REC);
        unsigned char * ref = bytes.data() + DIR_OFF + expert * DIR_REC;
        put16(ref, (uint16_t)(expert / 3)); put16(ref + 2, (uint16_t)(expert % 3)); ref[4] = 1; ref[5] = 0;
        put_float(ref + 6, expert == 0 ? 1.0f : 0.0f);
        put64(ref + 10, (expert + 1) * ALIGN); put32(ref + 18, REC);
        put32(ref + 22, crc32_bytes(blob, REC));
    }
    char path[] = "/tmp/pgr_runtime_XXXXXX"; int fd = mkstemp(path); if (fd < 0) return {};
    size_t done = 0; while (done < bytes.size()) { ssize_t n = write(fd, bytes.data() + done, bytes.size() - done); if (n <= 0) break; done += (size_t)n; }
    close(fd); return done == bytes.size() ? path : std::string();
}

static pgr_runtime_params params(const std::string & path) {
    pgr_runtime_params result{}; result.pgrn_path = path.c_str(); result.model_sha256 = SHA; result.clox_k = 4;
    result.admission = {36 * GiB, 30 * GiB, 1, 10 * GiB, 6 * GiB, 2 * GiB, 2 * GiB, 0, 9 * GiB, 4 * REC, 0};
    return result;
}

// PGCC1: source layer 0, source expert 0 -> layer 1 successors {0 (w2), 1 (w1)}.
static std::string coupling_fixture() {
    std::vector<unsigned char> b;
    auto u16 = [&](uint16_t v){ b.push_back(v & 0xff); b.push_back(v >> 8); };
    auto u32 = [&](uint32_t v){ for (int i = 0; i < 4; ++i) b.push_back((unsigned char)(v >> (8 * i))); };
    const char magic[8] = {'P','G','C','C','1',0,0,0};
    b.insert(b.end(), magic, magic + 8);
    u32(1); u32(1);          // version, layer_count
    u16(0); u16(1);          // layer 0, expert_count 1
    u16(0); u16(2);          // source expert 0, succ_count 2
    u16(0); u16(2); u16(1); u16(1);   // (succ 0, w2), (succ 1, w1)
    char path[] = "/tmp/pgr_cpl_XXXXXX"; int fd = mkstemp(path); if (fd < 0) return {};
    size_t done = 0; while (done < b.size()) { ssize_t n = write(fd, b.data() + done, b.size() - done); if (n <= 0) break; done += (size_t)n; }
    close(fd); return done == b.size() ? path : std::string();
}

int main() {
    std::string path = fixture(); CHECK(!path.empty());
    auto p = params(path); pgr_admission_plan plan{}; char error[192] = {};
    pgr_runtime * runtime = pgr_runtime_new(&p, &plan, error, sizeof(error));
    CHECK(runtime != nullptr); CHECK(plan.status == PGR_ADMISSION_OK && plan.mode == PGR_LOAD_STREAMING);
    CHECK(pgr_runtime_cache_capacity(runtime) == 4); CHECK(pgr_runtime_cache_bytes(runtime) == 4 * REC);
    CHECK(pgr_runtime_high_water_bytes(runtime) == 6 * REC);
    const void * data = nullptr; size_t size = 0; int hit = -1;
    CHECK(pgr_runtime_get(runtime, 0, 0, &data, &size, &hit) == PGR_STREAM_OK);
    CHECK(hit == 0 && size == REC && ((const unsigned char *)data)[0] == 1);
    CHECK(pgr_runtime_get(runtime, 0, 0, &data, &size, &hit) == PGR_STREAM_OK && hit == 1);
    CHECK(pgr_runtime_hits(runtime) == 1 && pgr_runtime_misses(runtime) == 1);
    CHECK(pgr_runtime_get(runtime, 1, 0, &data, &size, &hit) == PGR_STREAM_OK && hit == 0);
    CHECK(pgr_runtime_get(runtime, 1, 1, &data, &size, &hit) == PGR_STREAM_OK && hit == 0);
    CHECK(pgr_runtime_get(runtime, 0, 1, &data, &size, &hit) == PGR_STREAM_OK && hit == 0);
    CHECK(pgr_runtime_get(runtime, 0, 2, &data, &size, &hit) == PGR_STREAM_OK && hit == 0);
    // Layer 0 overflow must not evict the two resident layer-1 records.
    CHECK(pgr_runtime_get(runtime, 1, 0, &data, &size, &hit) == PGR_STREAM_OK && hit == 1);
    CHECK(pgr_runtime_get(runtime, 0, 99, &data, &size, &hit) == PGR_STREAM_INVALID);
    CHECK(std::strstr(pgr_runtime_error(runtime), "absent") != nullptr);
    pgr_runtime_free(runtime);

    // --- batch fetch with parallel cold reads (io_width > 1) ---
    {
        auto pb = params(path);
        pb.io_width = 2;                    // two parallel staging slices per layer stream
        pgr_admission_plan bplan{};
        char berr[192] = {};
        pgr_runtime * rb = pgr_runtime_new(&pb, &bplan, berr, sizeof(berr));
        CHECK(rb != nullptr);
        // staging grows from 1 to io_width records per layer: 4 cache + 2 layers*2 staging
        CHECK(pgr_runtime_high_water_bytes(rb) == 8 * REC);
        CHECK(pgr_runtime_layer_capacity(rb, 1) == 2 && pgr_runtime_layer_capacity(rb, 7) == 0);

        const uint16_t experts[2] = {0, 1};
        const void * bd[2] = {nullptr, nullptr};
        size_t bsz[2] = {0, 0};
        int bh[2] = {-1, -1};
        // Layer 1's two experts, read in parallel — both cold, correct bytes in order.
        CHECK(pgr_runtime_get_many(rb, 1, experts, 2, bd, bsz, bh) == PGR_STREAM_OK);
        CHECK(bh[0] == 0 && bh[1] == 0 && bsz[0] == REC && bsz[1] == REC);
        CHECK(((const unsigned char *) bd[0])[0] == 4);  // layer1/expert0 -> global 3 -> byte 4
        CHECK(((const unsigned char *) bd[1])[0] == 5);  // layer1/expert1 -> global 4 -> byte 5
        CHECK(pgr_runtime_misses(rb) == 2 && pgr_runtime_hits(rb) == 0);
        // Second identical batch is a new epoch: both resident, no new cold reads.
        CHECK(pgr_runtime_get_many(rb, 1, experts, 2, bd, bsz, bh) == PGR_STREAM_OK);
        CHECK(bh[0] == 1 && bh[1] == 1 && pgr_runtime_misses(rb) == 2 && pgr_runtime_hits(rb) == 2);
        // An absent expert anywhere in the batch fails closed before any fetch.
        const uint16_t bad[2] = {0, 99};
        CHECK(pgr_runtime_get_many(rb, 1, bad, 2, bd, bsz, bh) == PGR_STREAM_INVALID);
        CHECK(std::strstr(pgr_runtime_error(rb), "absent") != nullptr);
        pgr_runtime_free(rb);
    }

    // Refusals have to name what was measured and which knob to turn. Each of these
    // used to arrive as the same sentence, which told a user nothing they could act on.
    p = params(path); p.admission.available_known = 0;
    CHECK(pgr_runtime_new(&p, &plan, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "could not be read") != nullptr);

    p = params(path); p.admission.min_headroom_bytes = 40 * GiB;   // more than the host
    CHECK(pgr_runtime_new(&p, &plan, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "leaves nothing") != nullptr);
    CHECK(std::strstr(error, "--pgrn-headroom-gb") != nullptr);

    p = params(path); p.admission.requested_cache_bytes = 5 * GiB; // fits nowhere near
    p.admission.min_headroom_bytes = 30 * GiB;                     // ceiling 6 GiB, model needs 8
    CHECK(pgr_runtime_new(&p, &plan, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "too large for this host") != nullptr);

    p = params(path); p.admission.requested_cache_bytes = REC;     // one slot, two layers
    CHECK(pgr_runtime_new(&p, &plan, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "too small") != nullptr);
    CHECK(std::strstr(error, "--pgrn-cache-gb") != nullptr);

    // Zero means "derive it": the case a caller without a control app relies on.
    p = params(path);
    p.admission.requested_cache_bytes = 0;
    p.admission.expert_total_bytes = 8 * REC;   // keep the auto-sized cache test-sized
    runtime = pgr_runtime_new(&p, &plan, error, sizeof(error));
    CHECK(runtime != nullptr);
    CHECK(plan.expert_cache_bytes == 8 * REC);  // all of them fit, so all of them are cached
    CHECK(plan.status == PGR_ADMISSION_OK);
    pgr_runtime_free(runtime);

    // An upper bound above the total expert size is met by caching every expert,
    // rather than refused for being generous.
    p = params(path);
    p.admission.requested_cache_bytes = 20 * GiB;
    p.admission.expert_total_bytes = 8 * REC;
    runtime = pgr_runtime_new(&p, &plan, error, sizeof(error));
    CHECK(runtime != nullptr);
    CHECK(plan.expert_cache_bytes == 8 * REC);
    pgr_runtime_free(runtime);

    p = params(path); p.hot_percent = 50; p.promote_hits = 3; p.demote_idle_epochs = 8; p.cooldown_epochs = 4;
    p.cache_buft = ggml_backend_cpu_buffer_type();
    runtime = pgr_runtime_new(&p, &plan, error, sizeof(error)); CHECK(runtime != nullptr);
    CHECK(pgr_runtime_uses_backend_arena(runtime) == 1);
    CHECK(pgr_runtime_get(runtime, 0, 0, &data, &size, &hit) == PGR_STREAM_OK && hit == 0);
    CHECK(pgr_runtime_get(runtime, 0, 0, &data, &size, &hit) == PGR_STREAM_OK && hit == 1);
    CHECK(pgr_runtime_hot_hits(runtime) == 1 && pgr_runtime_warm_hits(runtime) == 0);
    CHECK(pgr_runtime_hot_count(runtime) == 1 && pgr_runtime_warm_count(runtime) == 0);
    CHECK(pgr_runtime_promotions(runtime) == 1 && pgr_runtime_demotions(runtime) == 0);
    CHECK(pgr_runtime_cache_bytes(runtime) == 4 * REC && pgr_runtime_high_water_bytes(runtime) == 6 * REC);
    pgr_runtime_layer_arena arena{};
    CHECK(pgr_runtime_layer_arena_get(runtime, 0, &arena) == 0);
    CHECK(arena.buffer != nullptr && arena.base != nullptr && arena.capacity == 2);
    CHECK(arena.record_bytes == REC && arena.role_offset[0] == 0);
    CHECK(arena.role_offset[1] == 1024 && arena.role_offset[2] == 2048);
    CHECK(arena.role_bytes[0] == 1024 && arena.role_bytes[1] == 1024 && arena.role_bytes[2] == 2048);
    int slot = -1; uint64_t generation = 0;
    pgr_runtime_batch_begin(runtime, 0);
    CHECK(pgr_runtime_get_slot(runtime, 0, 0, &data, &size, &hit, &slot, &generation) == PGR_STREAM_OK);
    CHECK(slot >= 0 && slot < 2 && generation != 0);
    CHECK(pgr_runtime_get_slot(runtime, 0, 1, &data, &size, &hit, &slot, &generation) == PGR_STREAM_OK);
    CHECK(pgr_runtime_get_slot(runtime, 0, 2, &data, &size, &hit, &slot, &generation) == PGR_STREAM_INVALID);
    pgr_runtime_batch_begin(runtime, 0);
    CHECK(pgr_runtime_get_slot(runtime, 0, 2, &data, &size, &hit, &slot, &generation) == PGR_STREAM_OK);

    std::vector<unsigned char> tensor_meta(8 * ggml_tensor_overhead() + 1024);
    ggml_init_params ctx_params{tensor_meta.size(), tensor_meta.data(), true};
    ggml_context * external = ggml_init(ctx_params);
    CHECK(external != nullptr);
    for (int layer = 0; layer < 2; ++layer) {
        ggml_tensor * gate = ggml_new_tensor_3d(external, GGML_TYPE_F32, 256, 1, 3);
        ggml_tensor * up = ggml_new_tensor_3d(external, GGML_TYPE_F32, 256, 1, 3);
        ggml_tensor * down = ggml_new_tensor_3d(external, GGML_TYPE_F32, 512, 1, 3);
        ggml_format_name(gate, "blk.%d.ffn_gate_exps.weight", layer);
        ggml_format_name(up, "blk.%d.ffn_up_exps.weight", layer);
        ggml_format_name(down, "blk.%d.ffn_down_exps.weight", layer);
    }
    pgr_compact * compact = pgr_compact_new(external, runtime, error, sizeof(error));
    CHECK(compact != nullptr && pgr_compact_layers(compact) == 2);
    ggml_tensor * compact_gate = pgr_compact_tensor(compact, 0, 0);
    ggml_tensor * compact_up = pgr_compact_tensor(compact, 0, 1);
    ggml_tensor * compact_down = pgr_compact_tensor(compact, 0, 2);
    CHECK(compact_gate && compact_up && compact_down);
    CHECK(compact_gate->ne[2] == 2 && compact_gate->nb[2] == REC);
    CHECK(compact_up->data == static_cast<unsigned char *>(arena.base) + 1024);
    CHECK(compact_down->data == static_cast<unsigned char *>(arena.base) + 2048);
    CHECK(compact_gate->buffer == arena.buffer && compact_up->buffer == arena.buffer);
    pgr_compact_free(compact);
    ggml_free(external);
    pgr_runtime_free(runtime);

    // --- coupled speculative prefetch (PGCC1): kick(L, fired) warms L+1's predicted set ---
    {
        std::string cpath = coupling_fixture(); CHECK(!cpath.empty());
        auto pc = params(path); pc.coupling_path = cpath.c_str();
        pgr_admission_plan cplan{}; char cerr[192] = {};
        pgr_runtime * rc = pgr_runtime_new(&pc, &cplan, cerr, sizeof(cerr));
        CHECK(rc != nullptr && pgr_runtime_has_coupling(rc) == 1);
        const int32_t fired[1] = {0};                          // expert 0 fired at layer 0
        pgr_runtime_prefetch_kick_coupled(rc, 0, fired, 1);    // predicts layer 1 -> {0,1}
        pgr_runtime_prefetch_settle(rc, 1);                    // join the background warm
        const void * d = nullptr; size_t s = 0; int h = -1;
        // layer 1's partition holds 2 slots, so the warm budget is cap/2 = 1: only the
        // heaviest successor (expert 0, weight 2) is warmed; expert 1 stays cold. This is the
        // anti-thrash cap - staging keeps room and prefetch never evicts the working set.
        CHECK(pgr_runtime_get(rc, 1, 0, &d, &s, &h) == PGR_STREAM_OK && h == 1);  // warmed
        CHECK(s == REC && ((const unsigned char *) d)[0] == 4);                   // layer1/e0 -> byte 4
        CHECK(pgr_runtime_get(rc, 1, 1, &d, &s, &h) == PGR_STREAM_OK && h == 0);  // capped out, cold
        // out-of-u16-range fired ids are skipped; absent source layer predicts nothing (no crash)
        const int32_t bad_fired[2] = {-1, 70000};
        pgr_runtime_prefetch_kick_coupled(rc, 0, bad_fired, 2); pgr_runtime_prefetch_settle(rc, 1);
        pgr_runtime_prefetch_kick_coupled(rc, 5, fired, 1);     pgr_runtime_prefetch_settle(rc, 6);
        pgr_runtime_free(rc);
        unlink(cpath.c_str());
    }
    // without a coupling table, has_coupling is 0 and the coupled kick is a safe no-op
    {
        auto pn = params(path); pgr_admission_plan nplan{}; char nerr[192] = {};
        pgr_runtime * rn = pgr_runtime_new(&pn, &nplan, nerr, sizeof(nerr));
        CHECK(rn != nullptr && pgr_runtime_has_coupling(rn) == 0);
        const int32_t fired[1] = {0};
        pgr_runtime_prefetch_kick_coupled(rn, 0, fired, 1); pgr_runtime_prefetch_settle(rn, 1);
        pgr_runtime_free(rn);
    }

    p = params(path); p.hot_percent = 101;
    CHECK(pgr_runtime_new(&p, &plan, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "HOT/WARM") != nullptr);
    unlink(path.c_str());
    std::printf("PGR_RUNTIME_OK\n");
    return 0;
}
