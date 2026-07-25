#include "peregrine_runtime.h"

#include "peregrine_arena.h"
#include "peregrine_pgrn.h"
#include "peregrine_predict.h"

#include <climits>
#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <new>
#include <thread>
#include <vector>

struct pgr_runtime {
    pgrn_file * store = nullptr;
    pgr_arena * arena = nullptr;
    std::vector<uint16_t> layers;
    std::vector<pgr_stream *> streams;
    std::vector<void *> partition_base;
    std::vector<size_t> partition_capacity;
    size_t capacity = 0;
    size_t cache_bytes = 0;
    size_t high_water_bytes = 0;
    int io_width = 1;
    // Speculative next-layer prefetch (opt-in, parity-neutral — warms cache only). One
    // prefetch in flight, running on a background thread against a DIFFERENT layer stream
    // than the one the main thread stages, joined before that stream is staged.
    pgr_predict * predict = nullptr;
    pgr_coupling * coupling = nullptr;
    std::thread prefetch_thread;
    bool prefetch_pending = false;
    uint16_t prefetch_layer = 0;
    char error[192] = {};
};

const char *pgr_runtime_model_sha256(const pgr_runtime * runtime) {
    return runtime && runtime->store ? pgrn_model_sha256(runtime->store) : "";
}

static void pgr_runtime_copy_error(char * dst, size_t capacity, const char * message) {
    if (dst && capacity) std::snprintf(dst, capacity, "%s", message ? message : "unknown error");
}

static pgr_stream_status pgr_runtime_load(
        void * user_data, long key, void * dst, size_t dst_capacity,
        size_t * loaded_bytes, char * error, size_t error_capacity) {
    auto * runtime = static_cast<pgr_runtime *>(user_data);
    const uint16_t layer = static_cast<uint16_t>((static_cast<unsigned long>(key) >> 16) & 0xffffUL);
    const uint16_t expert = static_cast<uint16_t>(static_cast<unsigned long>(key) & 0xffffUL);
    pgrn_expert_ref ref{};
    if (!pgrn_find(runtime->store, layer, expert, &ref)) {
        pgr_runtime_copy_error(error, error_capacity, "expert is absent from PGRN directory");
        return PGR_STREAM_INVALID;
    }
    if (ref.nbytes > dst_capacity) {
        pgr_runtime_copy_error(error, error_capacity, "expert exceeds fixed slot size");
        return PGR_STREAM_OVERFLOW;
    }
    /* Thread-safe read: pgr_stream_get_many may run this loader on worker threads, so it
     * must not touch shared pgrn state. Errors land in the caller-owned `error` buffer. */
    char read_error[160] = {};
    if (pgrn_read_expert_mt(runtime->store, &ref, dst, dst_capacity, read_error, sizeof(read_error)) != 0) {
        pgr_runtime_copy_error(error, error_capacity, read_error);
        return std::strstr(read_error, "CRC") ? PGR_STREAM_INTEGRITY : PGR_STREAM_IO;
    }
    *loaded_bytes = ref.nbytes;
    return PGR_STREAM_OK;
}

pgr_runtime *pgr_runtime_new(
        const pgr_runtime_params * params, pgr_admission_plan * admitted_plan,
        char * error, size_t error_capacity) {
    if (admitted_plan) std::memset(admitted_plan, 0, sizeof(*admitted_plan));
    if (!params || !params->pgrn_path || !params->model_sha256 || !admitted_plan) {
        pgr_runtime_copy_error(error, error_capacity, "invalid runtime parameters");
        return nullptr;
    }
    if (params->hot_percent > 100 ||
            (params->hot_percent > 0 &&
             (params->promote_hits == 0 || params->demote_idle_epochs == 0))) {
        pgr_runtime_copy_error(error, error_capacity, "invalid PGRN HOT/WARM policy parameters");
        return nullptr;
    }
    auto * runtime = new (std::nothrow) pgr_runtime;
    if (!runtime) {
        pgr_runtime_copy_error(error, error_capacity, "runtime allocation failed");
        return nullptr;
    }
    runtime->store = pgrn_open(params->pgrn_path, params->model_sha256);
    if (!runtime->store) {
        pgr_runtime_copy_error(error, error_capacity, "PGRN validation or model identity failed");
        pgr_runtime_free(runtime);
        return nullptr;
    }
    const size_t slot_bytes = pgrn_max_expert_bytes(runtime->store);
    pgr_admission_input input = params->admission;
    const size_t layer_count = pgrn_layer_count(runtime->store);
    /* Each layer stream keeps io_width cold-read staging records (io_width=1 is the
     * qualified default). Admit the full staging so raising --pgrn-io-threads can never
     * silently overrun the usability headroom — admission fails closed instead. */
    const uint64_t io_width = params->io_width > 1 ? (uint64_t) params->io_width : 1;
    const int staging_overflow =
            slot_bytes == 0 || layer_count == 0 ||
            io_width > UINT64_MAX / (uint64_t) slot_bytes ||
            (uint64_t) layer_count > UINT64_MAX / (io_width * (uint64_t) slot_bytes);
    const uint64_t stream_staging =
            staging_overflow ? 0 : (uint64_t) layer_count * io_width * (uint64_t) slot_bytes;
    if (staging_overflow || input.staging_bytes > UINT64_MAX - stream_staging) {
        pgr_runtime_copy_error(error, error_capacity, "invalid PGRN slot size or staging overflow");
        pgr_runtime_free(runtime);
        return nullptr;
    }
    input.staging_bytes += stream_staging;
    runtime->io_width = (int) io_width;
    /* Opt-in predicted-prefetch table. Best-effort: a bad/absent path just disables
     * prefetch (perf feature, not a safety gate) — the model still loads. */
    if (params->predict_path && params->predict_path[0]) {
        runtime->predict = pgr_predict_load(params->predict_path);
    }
    if (params->coupling_path && params->coupling_path[0]) {
        runtime->coupling = pgr_coupling_load(params->coupling_path);
    }
    if (pgr_admission_compute(&input, admitted_plan) != 0 ||
            admitted_plan->status != PGR_ADMISSION_OK ||
            admitted_plan->expert_cache_bytes < slot_bytes) {
        pgr_runtime_copy_error(error, error_capacity, "native memory admission refused PGRN cache");
        pgr_runtime_free(runtime);
        return nullptr;
    }
    uint64_t capacity64 = admitted_plan->expert_cache_bytes / slot_bytes;
    if (capacity64 < layer_count || capacity64 > INT_MAX || capacity64 > SIZE_MAX / slot_bytes) {
        pgr_runtime_copy_error(error, error_capacity, "PGRN cache capacity is invalid");
        pgr_runtime_free(runtime);
        return nullptr;
    }
    runtime->capacity = static_cast<size_t>(capacity64);
    runtime->cache_bytes = runtime->capacity * slot_bytes;
    admitted_plan->expert_cache_bytes = runtime->cache_bytes;
    admitted_plan->resident_bytes = admitted_plan->mandatory_resident_bytes + runtime->cache_bytes;
    const uint64_t future_resident = admitted_plan->resident_bytes - input.already_allocated_bytes;
    admitted_plan->system_free_after_load_bytes = input.available_bytes > future_resident
        ? input.available_bytes - future_resident : 0;
    admitted_plan->streamed_expert_bytes = input.expert_total_bytes > runtime->cache_bytes
        ? input.expert_total_bytes - runtime->cache_bytes : 0;
    admitted_plan->mode = admitted_plan->streamed_expert_bytes ? PGR_LOAD_STREAMING : PGR_LOAD_RESIDENT;

    if (params->cache_buft) {
        runtime->arena = pgr_arena_new(
                params->cache_buft, runtime->cache_bytes, runtime->error, sizeof(runtime->error));
        if (!runtime->arena) {
            pgr_runtime_copy_error(error, error_capacity,
                    runtime->error[0] ? runtime->error : "fixed PGRN backend arena allocation failed");
            pgr_runtime_free(runtime);
            return nullptr;
        }
    }

    runtime->layers.reserve(layer_count);
    runtime->streams.reserve(layer_count);
    runtime->partition_base.reserve(layer_count);
    runtime->partition_capacity.reserve(layer_count);
    const size_t per_layer = runtime->capacity / layer_count;
    const size_t remainder = runtime->capacity % layer_count;
    size_t arena_offset = 0;
    for (size_t index = 0; index < layer_count; ++index) {
        uint16_t layer = 0;
        const size_t partition_capacity = per_layer + (index < remainder ? 1 : 0);
        if (!pgrn_layer_at(runtime->store, index, &layer) || partition_capacity == 0 ||
                partition_capacity > INT_MAX) {
            pgr_runtime_copy_error(error, error_capacity, "invalid PGRN layer partition");
            pgr_runtime_free(runtime);
            return nullptr;
        }
        size_t hot_capacity = partition_capacity * params->hot_percent / 100U;
        if (params->hot_percent > 0 && partition_capacity > 1 && hot_capacity == 0) hot_capacity = 1;
        if (partition_capacity > 1 && hot_capacity >= partition_capacity) hot_capacity = partition_capacity - 1;
        if (partition_capacity == 1) hot_capacity = 0;
        pgr_stream_params stream_params{};
        stream_params.slot_bytes = slot_bytes;
        stream_params.capacity = static_cast<int>(partition_capacity);
        stream_params.clox_k = params->clox_k > 0 ? params->clox_k : 4;
        stream_params.hot_capacity = static_cast<int>(hot_capacity);
        stream_params.promote_hits = params->promote_hits > 0 ? params->promote_hits : 3;
        stream_params.demote_idle_epochs = params->demote_idle_epochs > 0 ? params->demote_idle_epochs : 64;
        stream_params.cooldown_epochs = params->cooldown_epochs;
        stream_params.io_width = params->io_width;
        pgr_stream * stream = nullptr;
        if (runtime->arena) {
            const size_t partition_bytes = partition_capacity * slot_bytes;
            void * partition = nullptr;
            if (pgr_arena_slice(runtime->arena, arena_offset, partition_bytes,
                    &partition, runtime->error, sizeof(runtime->error)) != 0) {
                pgr_runtime_copy_error(error, error_capacity, runtime->error);
                pgr_runtime_free(runtime);
                return nullptr;
            }
            stream = pgr_stream_new_loader_external(
                    &stream_params, partition, partition_bytes, pgr_runtime_load, runtime);
            arena_offset += partition_bytes;
        } else {
            stream = pgr_stream_new_loader_tier(&stream_params, pgr_runtime_load, runtime);
        }
        if (!stream) {
            pgr_runtime_copy_error(error, error_capacity, "fixed PGRN layer cache allocation failed");
            pgr_runtime_free(runtime);
            return nullptr;
        }
        runtime->layers.push_back(layer);
        runtime->streams.push_back(stream);
        runtime->partition_base.push_back(runtime->arena
                ? static_cast<unsigned char *>(pgr_arena_base(runtime->arena)) + arena_offset - partition_capacity * slot_bytes
                : nullptr);
        runtime->partition_capacity.push_back(partition_capacity);
        runtime->high_water_bytes += pgr_stream_high_water_bytes(stream);
    }
    if (error && error_capacity) error[0] = '\0';
    return runtime;
}

static size_t pgr_runtime_layer_index(const pgr_runtime * runtime, uint16_t layer) {
    if (!runtime) return SIZE_MAX;
    const auto found = std::lower_bound(runtime->layers.begin(), runtime->layers.end(), layer);
    return found == runtime->layers.end() || *found != layer
        ? SIZE_MAX : static_cast<size_t>(found - runtime->layers.begin());
}

void pgr_runtime_batch_begin(pgr_runtime * runtime, uint16_t layer) {
    const size_t index = pgr_runtime_layer_index(runtime, layer);
    if (index != SIZE_MAX) pgr_stream_batch_begin(runtime->streams[index]);
}

pgr_stream_status pgr_runtime_get_slot(
        pgr_runtime * runtime, uint16_t layer, uint16_t expert,
        const void ** data, size_t * data_size, int * hit,
        int * slot, uint64_t * generation) {
    if (!runtime) return PGR_STREAM_INVALID;
    const size_t index = pgr_runtime_layer_index(runtime, layer);
    if (index == SIZE_MAX) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", "layer is absent from PGRN cache partitions");
        return PGR_STREAM_INVALID;
    }
    pgrn_expert_ref ref{};
    if (!pgrn_find(runtime->store, layer, expert, &ref)) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", "expert is absent from PGRN directory");
        return PGR_STREAM_INVALID;
    }
    const long key = static_cast<long>((static_cast<unsigned long>(layer) << 16) | expert);
    const pgr_stream_status status = pgr_stream_get_key_heat_slot(
            runtime->streams[index], key, ref.heat, data, data_size, hit, slot, generation);
    if (status != PGR_STREAM_OK) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", pgr_stream_error(runtime->streams[index]));
    } else {
        runtime->error[0] = '\0';
    }
    return status;
}

pgr_stream_status pgr_runtime_get_many(
        pgr_runtime * runtime, uint16_t layer,
        const uint16_t * experts, int n,
        const void ** data, size_t * data_size, int * hit) {
    enum { PGR_MAX_BATCH = 256 };
    if (!runtime || (!experts && n != 0) || !data || n < 0) return PGR_STREAM_INVALID;
    if (n > PGR_MAX_BATCH) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", "expert batch exceeds fixed maximum");
        return PGR_STREAM_INVALID;
    }
    const size_t index = pgr_runtime_layer_index(runtime, layer);
    if (index == SIZE_MAX) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", "layer is absent from PGRN cache partitions");
        return PGR_STREAM_INVALID;
    }
    /* Resolve each expert against the PGRN directory (heat + existence) before the
     * batch fetch; the directory lookup is read-only and cheap. */
    long  keys[PGR_MAX_BATCH];
    float heats[PGR_MAX_BATCH];
    for (int i = 0; i < n; ++i) {
        pgrn_expert_ref ref{};
        if (!pgrn_find(runtime->store, layer, experts[i], &ref)) {
            std::snprintf(runtime->error, sizeof(runtime->error), "%s", "expert is absent from PGRN directory");
            return PGR_STREAM_INVALID;
        }
        keys[i]  = static_cast<long>((static_cast<unsigned long>(layer) << 16) | experts[i]);
        heats[i] = ref.heat;
    }
    /* One epoch per layer stream: release the previous token's pins, then fetch this
     * token's selected experts with cold reads running in parallel. */
    pgr_stream_batch_begin(runtime->streams[index]);
    const pgr_stream_status status = pgr_stream_get_many(
            runtime->streams[index], keys, heats, n, data, data_size, hit);
    if (status != PGR_STREAM_OK) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", pgr_stream_error(runtime->streams[index]));
    } else {
        runtime->error[0] = '\0';
    }
    return status;
}

size_t pgr_runtime_layer_capacity(const pgr_runtime * runtime, uint16_t layer) {
    const size_t index = pgr_runtime_layer_index(runtime, layer);
    return index == SIZE_MAX ? 0 : runtime->partition_capacity[index];
}

int pgr_runtime_io_width(const pgr_runtime * runtime) {
    return runtime ? runtime->io_width : 1;
}

int pgr_runtime_prefetch(
        pgr_runtime * runtime, uint16_t layer, const uint16_t * experts, int n) {
    enum { PGR_MAX_BATCH = 256 };
    if (!runtime || (!experts && n != 0) || n < 0) return -1;
    if (n == 0) return 0;
    if (n > PGR_MAX_BATCH) n = PGR_MAX_BATCH;   /* best-effort: warm what fits the buffer */
    const size_t index = pgr_runtime_layer_index(runtime, layer);
    if (index == SIZE_MAX) return 0;            /* unknown layer -> nothing to warm */
    long  keys[PGR_MAX_BATCH];
    float heats[PGR_MAX_BATCH];
    int valid = 0;
    for (int i = 0; i < n; ++i) {
        pgrn_expert_ref ref{};
        if (!pgrn_find(runtime->store, layer, experts[i], &ref)) continue;  /* skip absent */
        keys[valid]  = static_cast<long>((static_cast<unsigned long>(layer) << 16) | experts[i]);
        heats[valid] = ref.heat;
        ++valid;
    }
    if (valid == 0) return 0;
    return pgr_stream_prefetch_many(runtime->streams[index], keys, heats, valid);
}

int pgr_runtime_layer_arena_get(
        const pgr_runtime * runtime, uint16_t layer,
        pgr_runtime_layer_arena * out) {
    if (out) std::memset(out, 0, sizeof(*out));
    if (!runtime || !out || !runtime->arena) return -1;
    const size_t index = pgr_runtime_layer_index(runtime, layer);
    pgrn_tensor_layout layout{};
    if (index == SIZE_MAX || !pgrn_find_layout(runtime->store, layer, &layout)) return -1;
    out->buffer = pgr_arena_buffer(runtime->arena);
    out->base = runtime->partition_base[index];
    out->capacity = runtime->partition_capacity[index];
    out->record_bytes = pgrn_max_expert_bytes(runtime->store);
    size_t offset = 0;
    for (size_t role = 0; role < 3; ++role) {
        out->role_offset[role] = offset;
        out->role_bytes[role] = static_cast<size_t>(layout.nbytes[role]);
        offset += out->role_bytes[role];
    }
    return offset <= out->record_bytes ? 0 : -1;
}

pgr_stream_status pgr_runtime_get(
        pgr_runtime * runtime, uint16_t layer, uint16_t expert,
        const void ** data, size_t * data_size, int * hit) {
    if (!runtime) return PGR_STREAM_INVALID;
    const auto found = std::lower_bound(runtime->layers.begin(), runtime->layers.end(), layer);
    if (found == runtime->layers.end() || *found != layer) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", "layer is absent from PGRN cache partitions");
        return PGR_STREAM_INVALID;
    }
    const size_t index = static_cast<size_t>(found - runtime->layers.begin());
    const long key = static_cast<long>((static_cast<unsigned long>(layer) << 16) | expert);
    pgrn_expert_ref ref{};
    if (!pgrn_find(runtime->store, layer, expert, &ref)) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", "expert is absent from PGRN directory");
        return PGR_STREAM_INVALID;
    }
    pgr_stream_status status = pgr_stream_get_key_heat(
            runtime->streams[index], key, ref.heat, data, data_size, hit);
    if (status != PGR_STREAM_OK) {
        std::snprintf(runtime->error, sizeof(runtime->error), "%s", pgr_stream_error(runtime->streams[index]));
    } else {
        runtime->error[0] = '\0';
    }
    return status;
}

size_t pgr_runtime_cache_capacity(const pgr_runtime * runtime) { return runtime ? runtime->capacity : 0; }
size_t pgr_runtime_cache_bytes(const pgr_runtime * runtime) { return runtime ? runtime->cache_bytes : 0; }
size_t pgr_runtime_high_water_bytes(const pgr_runtime * runtime) {
    return runtime ? runtime->high_water_bytes : 0;
}
long pgr_runtime_hits(const pgr_runtime * runtime) {
    long total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_hits(stream);
    return total;
}
long pgr_runtime_misses(const pgr_runtime * runtime) {
    long total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_misses(stream);
    return total;
}
long pgr_runtime_hot_hits(const pgr_runtime * runtime) {
    long total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_hot_hits(stream);
    return total;
}
long pgr_runtime_warm_hits(const pgr_runtime * runtime) {
    long total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_warm_hits(stream);
    return total;
}
uint64_t pgr_runtime_promotions(const pgr_runtime * runtime) {
    uint64_t total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_promotions(stream);
    return total;
}
uint64_t pgr_runtime_demotions(const pgr_runtime * runtime) {
    uint64_t total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_demotions(stream);
    return total;
}
size_t pgr_runtime_hot_count(const pgr_runtime * runtime) {
    size_t total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_hot_count(stream);
    return total;
}
size_t pgr_runtime_warm_count(const pgr_runtime * runtime) {
    size_t total = 0;
    if (runtime) for (const pgr_stream * stream : runtime->streams) total += pgr_stream_warm_count(stream);
    return total;
}
int pgr_runtime_uses_backend_arena(const pgr_runtime * runtime) {
    return runtime && runtime->arena ? 1 : 0;
}
const char *pgr_runtime_error(const pgr_runtime * runtime) { return runtime ? runtime->error : "invalid runtime"; }

/* Join any in-flight prefetch so no background thread touches a stream we are about to
 * free or stage. Internal; also the backstop the kick/settle helpers rely on. */
static void pgr_runtime_prefetch_join(pgr_runtime * runtime) {
    if (runtime && runtime->prefetch_pending) {
        if (runtime->prefetch_thread.joinable()) runtime->prefetch_thread.join();
        runtime->prefetch_pending = false;
    }
}

void pgr_runtime_prefetch_kick(pgr_runtime * runtime, uint16_t layer) {
    if (!runtime || !runtime->predict) return;
    pgr_runtime_prefetch_join(runtime);   /* one prefetch in flight */
    uint16_t hot[256];
    const int n = pgr_predict_hot(runtime->predict, layer, hot, 256);
    if (n <= 0) return;
    std::vector<uint16_t> experts(hot, hot + n);   /* thread owns its own copy */
    runtime->prefetch_pending = true;
    runtime->prefetch_layer = layer;
    runtime->prefetch_thread = std::thread([runtime, layer, experts]() {
        /* runs against streams[layer]; the main thread stages a DIFFERENT stream until
         * pgr_runtime_prefetch_settle(layer) joins this — so no shared tier state. */
        pgr_runtime_prefetch(runtime, layer, experts.data(), (int) experts.size());
    });
}

int pgr_runtime_has_coupling(const pgr_runtime * runtime) {
    return runtime && runtime->coupling ? 1 : 0;
}

void pgr_runtime_prefetch_kick_coupled(pgr_runtime * runtime, uint16_t src_layer,
                                       const int32_t * fired, int n_fired) {
    if (!runtime || !runtime->coupling || !fired || n_fired <= 0) return;
    pgr_runtime_prefetch_join(runtime);   /* one prefetch in flight */
    uint16_t fired_u16[256];
    int nf = 0;
    for (int i = 0; i < n_fired && nf < 256; ++i) {
        const int32_t v = fired[i];
        if (v < 0 || v > 0xFFFF) continue;
        const uint16_t u = (uint16_t) v;
        bool dup = false;
        for (int k = 0; k < nf; ++k) if (fired_u16[k] == u) { dup = true; break; }
        if (!dup) fired_u16[nf++] = u;
    }
    if (nf == 0) return;
    const uint16_t target = (uint16_t) (src_layer + 1);
    /* Never warm more than half the target layer's cache partition: staging at that layer
     * still needs its slots, so an unbounded warm would evict the working set and thrash a
     * small cache (measured: warming ~256/layer into a ~12-slot partition regressed decode
     * ~6x). Cap scales with the cache - big caches warm more, tiny caches warm little. */
    const int cap = (int) pgr_runtime_layer_capacity(runtime, target);
    const int budget = cap / 2;
    if (budget < 1) return;
    uint16_t hot[256];
    const int want = budget < 256 ? budget : 256;
    const int n = pgr_coupling_next(runtime->coupling, src_layer, fired_u16, nf, hot, want);
    if (n <= 0) return;
    std::vector<uint16_t> experts(hot, hot + n);   /* thread owns its own copy */
    runtime->prefetch_pending = true;
    runtime->prefetch_layer = target;
    runtime->prefetch_thread = std::thread([runtime, target, experts]() {
        pgr_runtime_prefetch(runtime, target, experts.data(), (int) experts.size());
    });
}

void pgr_runtime_prefetch_settle(pgr_runtime * runtime, uint16_t layer) {
    if (runtime && runtime->prefetch_pending && runtime->prefetch_layer == layer) {
        pgr_runtime_prefetch_join(runtime);
    }
}

void pgr_runtime_free(pgr_runtime * runtime) {
    if (!runtime) return;
    pgr_runtime_prefetch_join(runtime);   /* no background thread past this point */
    pgr_predict_free(runtime->predict);
    pgr_coupling_free(runtime->coupling);
    for (pgr_stream * stream : runtime->streams) pgr_stream_free(stream);
    pgr_arena_free(runtime->arena);
    pgrn_close(runtime->store);
    delete runtime;
}
