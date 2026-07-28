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
    // Online co-activation predictor (opt-in via PGRN_ONLINE_PREDICT): learns the
    // L -> L+1 expert coupling live from observed routing, replacing a static table.
    uint16_t * co_counts = nullptr;   // [layers * experts * experts] saturating counts
    int co_layers = 0, co_experts = 0;
    int co_prev_n = 0;
    int co_prev_layer = -1;
    uint16_t co_prev[256] = {};
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

/* Parse "pgrn-partition-weights v1" (one "<layer_id> <weight>" line per layer,
 * weights > 0) and apportion `capacity` slots by largest remainder with a floor
 * of one slot per layer. Returns false (equal split) on any problem. */
static bool pgr_runtime_weighted_sizes(
        pgrn_file * store, const char * path, size_t layer_count, size_t capacity,
        std::vector<size_t> & sizes) {
    if (!store || !path || !path[0] || layer_count == 0 || capacity < layer_count) return false;
    FILE * fh = fopen(path, "r");
    if (!fh) return false;
    char magic[64] = {0};
    if (!fgets(magic, sizeof(magic), fh) ||
            strncmp(magic, "pgrn-partition-weights v1", 25) != 0) {
        fclose(fh);
        return false;
    }
    std::vector<long> ids;
    std::vector<double> vals;
    long layer_id = 0;
    double weight = 0.0;
    while (fscanf(fh, "%ld %lf", &layer_id, &weight) == 2) {
        if (layer_id < 0 || layer_id > UINT16_MAX || !(weight > 0.0)) {
            fclose(fh);
            return false;
        }
        ids.push_back(layer_id);
        vals.push_back(weight);
    }
    fclose(fh);
    // map the store's layer order onto the file's weights; every layer must be covered
    std::vector<double> w(layer_count, 0.0);
    double wsum = 0.0;
    for (size_t index = 0; index < layer_count; ++index) {
        uint16_t layer = 0;
        if (!pgrn_layer_at(store, index, &layer)) return false;
        double found = 0.0;
        for (size_t k = 0; k < ids.size(); ++k) {
            if (ids[k] == (long) layer) { found = vals[k]; break; }
        }
        if (!(found > 0.0)) return false;
        w[index] = found;
        wsum += found;
    }
    if (!(wsum > 0.0)) return false;
    // largest-remainder apportionment, floor 1, exact total == capacity
    sizes.assign(layer_count, 1);
    std::vector<double> frac(layer_count, 0.0);
    size_t assigned = 0;
    for (size_t i = 0; i < layer_count; ++i) {
        const double raw = (double) capacity * (w[i] / wsum);
        size_t base = (size_t) raw;
        if (base < 1) base = 1;
        if (base > INT_MAX) base = INT_MAX;
        sizes[i] = base;
        frac[i] = raw - (double) base;
        assigned += base;
    }
    while (assigned < capacity) {
        size_t best = 0;
        for (size_t i = 1; i < layer_count; ++i) {
            if (frac[i] > frac[best]) best = i;
        }
        sizes[best] += 1;
        frac[best] = -1.0;
        ++assigned;
    }
    while (assigned > capacity) {
        size_t biggest = 0;
        for (size_t i = 1; i < layer_count; ++i) {
            if (sizes[i] > sizes[biggest]) biggest = i;
        }
        if (sizes[biggest] <= 1) return false;
        sizes[biggest] -= 1;
        --assigned;
    }
    for (size_t i = 0; i < layer_count; ++i) {
        if (sizes[i] == 0 || sizes[i] > INT_MAX) return false;
    }
    return true;
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
    /* Online co-activation predictor: allocate the live L->L+1 count table when opted in
     * and the table stays within a modest RAM budget (<=16 MB). Model-agnostic; parity-safe. */
    if (getenv("PGRN_ONLINE_PREDICT")) {
        const int L = (int) pgrn_layer_count(runtime->store);
        const int E = (int) pgrn_experts_per_layer(runtime->store);
        if (L > 1 && E > 0 && E <= 512 && (long) L * E * E <= 8L * 1024 * 1024) {
            runtime->co_counts = (uint16_t *) calloc((size_t) L * E * E, sizeof(uint16_t));
            if (runtime->co_counts) { runtime->co_layers = L; runtime->co_experts = E; }
        }
    }
    if (pgr_admission_compute(&input, admitted_plan) != 0) {
        pgr_runtime_copy_error(error, error_capacity,
                "admission inputs are invalid or overflow 64-bit arithmetic");
        pgr_runtime_free(runtime);
        return nullptr;
    }
    if (admitted_plan->status != PGR_ADMISSION_OK ||
            admitted_plan->expert_cache_bytes < slot_bytes) {
        /* One message for every cause left a user holding a refusal with no idea which
         * number to change. Each of these is a different situation with a different
         * remedy, so each says what it measured and what it wanted. */
        const uint64_t MiB = 1024ULL * 1024ULL;
        const uint64_t fits = admitted_plan->static_ceiling_bytes >
                              admitted_plan->mandatory_resident_bytes
                ? admitted_plan->static_ceiling_bytes - admitted_plan->mandatory_resident_bytes : 0;
        char detail[352];
        if (admitted_plan->status == PGR_ADMISSION_OK) {
            /* Reached only via the slot check below the plan: the plan itself is fine. */
            std::snprintf(detail, sizeof(detail),
                    "admitted expert cache %llu MiB is smaller than a single expert (%llu MiB); "
                    "raise --pgrn-cache-gb or reduce --pgrn-headroom-gb",
                    (unsigned long long) (admitted_plan->expert_cache_bytes / MiB),
                    (unsigned long long) ((uint64_t) slot_bytes / MiB));
        } else switch (admitted_plan->reason) {
            case PGR_REASON_HEADROOM_EXCEEDS_RAM:
                std::snprintf(detail, sizeof(detail),
                        "the reserve of %llu MiB leaves nothing of the %llu MiB this host reports; "
                        "lower --pgrn-headroom-gb",
                        (unsigned long long) (input.min_headroom_bytes / MiB),
                        (unsigned long long) (input.total_bytes / MiB));
                break;
            case PGR_REASON_MODEL_EXCEEDS_CEILING:
                std::snprintf(detail, sizeof(detail),
                        "the model needs %llu MiB resident before any expert cache, but only %llu MiB "
                        "remains of %llu MiB after the %llu MiB reserve; this model is too large for "
                        "this host",
                        (unsigned long long) (admitted_plan->mandatory_resident_bytes / MiB),
                        (unsigned long long) (admitted_plan->static_ceiling_bytes / MiB),
                        (unsigned long long) (input.total_bytes / MiB),
                        (unsigned long long) (admitted_plan->reserved_headroom_bytes / MiB));
                break;
            case PGR_REASON_CACHE_REQUEST_TOO_LARGE:
                std::snprintf(detail, sizeof(detail),
                        "requested expert cache %llu MiB exceeds the %llu MiB that fits; omit "
                        "--pgrn-cache-gb to take what fits, or lower --pgrn-headroom-gb",
                        (unsigned long long) (input.requested_cache_bytes / MiB),
                        (unsigned long long) (fits / MiB));
                break;
            case PGR_REASON_AVAILABLE_UNKNOWN:
                pgr_runtime_copy_error(error, error_capacity,
                        "current free memory could not be read on this host, so admission fails "
                        "closed rather than guess");
                pgr_runtime_free(runtime);
                return nullptr;
            case PGR_REASON_RESERVE_AT_RISK:
                std::snprintf(detail, sizeof(detail),
                        "the plan fits the machine but not the moment: %llu MiB is free now and "
                        "%llu MiB resident is planned, which would break the %llu MiB reserve; "
                        "close something or lower --pgrn-cache-gb",
                        (unsigned long long) (input.available_bytes / MiB),
                        (unsigned long long) (admitted_plan->resident_bytes / MiB),
                        (unsigned long long) (admitted_plan->reserved_headroom_bytes / MiB));
                break;
            default:
                std::snprintf(detail, sizeof(detail),
                        "admission refused the PGRN cache without a stated reason (status %d)",
                        (int) admitted_plan->status);
                break;
        }
        pgr_runtime_copy_error(error, error_capacity, detail);
        pgr_runtime_free(runtime);
        return nullptr;
    }
    uint64_t capacity64 = admitted_plan->expert_cache_bytes / slot_bytes;
    if (capacity64 < layer_count || capacity64 > INT_MAX || capacity64 > SIZE_MAX / slot_bytes) {
        /* Almost always the first condition, and almost always a cache the user set too
         * small: every layer needs at least one slot to hold the expert it is computing.
         * Say the number that would work instead of calling the value invalid. */
        char detail[352];
        const uint64_t MiB = 1024ULL * 1024ULL;
        if (capacity64 < layer_count) {
            const uint64_t need = (uint64_t) layer_count * slot_bytes;
            /* Round the suggestion up to the precision it is printed at. A value that
             * is merely close would be rejected on the next attempt, which is worse
             * than offering nothing. */
            const uint64_t gib_hundredths = (need * 100 + (1024ULL * MiB) - 1) / (1024ULL * MiB);
            std::snprintf(detail, sizeof(detail),
                    "an expert cache of %llu MiB is too small: each of the %llu layers needs one "
                    "slot of %llu KiB, so use at least %llu MiB (--pgrn-cache-gb %.2f) or omit the "
                    "flag to take what fits",
                    (unsigned long long) (admitted_plan->expert_cache_bytes / MiB),
                    (unsigned long long) layer_count,
                    (unsigned long long) ((uint64_t) slot_bytes / 1024ULL),
                    (unsigned long long) ((need + MiB - 1) / MiB),
                    (double) gib_hundredths / 100.0);
        } else {
            std::snprintf(detail, sizeof(detail),
                    "expert cache of %llu MiB in slots of %llu MiB overflows the addressable slot "
                    "count on this build",
                    (unsigned long long) (admitted_plan->expert_cache_bytes / MiB),
                    (unsigned long long) ((uint64_t) slot_bytes / MiB));
        }
        pgr_runtime_copy_error(error, error_capacity, detail);
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
    /* Opt-in width-weighted partition: apportion slots by measured per-layer
     * working-set width instead of the equal split. Parity-safe (sizing only
     * moves the hit-rate; every load stays CRC-checked), so any file/shape
     * problem falls back to the equal split instead of failing the load. */
    std::vector<size_t> weighted_sizes;
    const bool weighted = pgr_runtime_weighted_sizes(
            runtime->store, params->weights_path, layer_count, runtime->capacity, weighted_sizes);
    if (params->weights_path && params->weights_path[0] && !weighted) {
        fprintf(stderr, "[peregrine] partition weights unusable (%s) — equal split\n", params->weights_path);
    }
    if (weighted) {
        fprintf(stderr, "[peregrine] width-weighted cache partition active (%s)\n", params->weights_path);
    }
    size_t arena_offset = 0;
    for (size_t index = 0; index < layer_count; ++index) {
        uint16_t layer = 0;
        const size_t partition_capacity = weighted
                ? weighted_sizes[index]
                : per_layer + (index < remainder ? 1 : 0);
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

int pgr_runtime_has_online(const pgr_runtime * runtime) {
    return runtime && runtime->co_counts ? 1 : 0;
}

/* Online co-activation predictor: learns which experts of layer L+1 co-fire with the
 * experts that fired at layer L, live, then prefetches the predicted set for L+1. No
 * table file, model-agnostic, parity-neutral (warms cache only, never changes output). */
void pgr_runtime_prefetch_kick_online(pgr_runtime * runtime, uint16_t src_layer,
                                      const int32_t * fired, int n_fired) {
    if (!runtime || !runtime->co_counts || !fired || n_fired <= 0) return;
    const int E = runtime->co_experts, L = runtime->co_layers;
    /* dedup the current layer's fired experts */
    uint16_t cur[256]; int cn = 0;
    for (int i = 0; i < n_fired && cn < 256; ++i) {
        const int32_t v = fired[i];
        if (v < 0 || v >= E) continue;
        bool dup = false;
        for (int k = 0; k < cn; ++k) if (cur[k] == v) { dup = true; break; }
        if (!dup) cur[cn++] = (uint16_t) v;
    }
    if (cn == 0) return;
    /* observe: previous layer's fired -> this layer's fired (only across adjacent layers) */
    if (runtime->co_prev_layer == (int) src_layer - 1 && runtime->co_prev_n > 0) {
        const size_t lbase = (size_t) runtime->co_prev_layer * (size_t) E * (size_t) E;
        for (int p = 0; p < runtime->co_prev_n; ++p) {
            uint16_t * row = runtime->co_counts + lbase + (size_t) runtime->co_prev[p] * (size_t) E;
            for (int c = 0; c < cn; ++c) if (row[cur[c]] < 0xFFFF) row[cur[c]]++;
        }
    }
    /* predict layer src_layer+1 from counts[src_layer][cur][*], warm the top-`budget` */
    const int target = (int) src_layer + 1;
    if (target < L) {
        pgr_runtime_prefetch_join(runtime);   /* one prefetch in flight */
        int budget = (int) pgr_runtime_layer_capacity(runtime, (uint16_t) target) / 2;
        if (budget > E) budget = E;
        if (budget > 256) budget = 256;
        if (budget > 0) {
            std::vector<uint32_t> score((size_t) E, 0u);
            const size_t lbase = (size_t) src_layer * (size_t) E * (size_t) E;
            for (int c = 0; c < cn; ++c) {
                const uint16_t * row = runtime->co_counts + lbase + (size_t) cur[c] * (size_t) E;
                for (int e = 0; e < E; ++e) score[e] += row[e];
            }
            uint16_t hot[256]; int hn = 0;
            for (int pick = 0; pick < budget; ++pick) {
                int best = -1; uint32_t bestv = 0;
                for (int e = 0; e < E; ++e) if (score[e] > bestv) { bestv = score[e]; best = e; }
                if (best < 0) break;
                hot[hn++] = (uint16_t) best; score[best] = 0;
            }
            if (hn > 0) {
                std::vector<uint16_t> experts(hot, hot + hn);
                runtime->prefetch_pending = true;
                runtime->prefetch_layer = (uint16_t) target;
                runtime->prefetch_thread = std::thread([runtime, target, experts]() {
                    pgr_runtime_prefetch(runtime, (uint16_t) target, experts.data(), (int) experts.size());
                });
            }
        }
    }
    /* remember current as previous for the next layer's observation */
    for (int c = 0; c < cn; ++c) runtime->co_prev[c] = cur[c];
    runtime->co_prev_n = cn;
    runtime->co_prev_layer = (int) src_layer;
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
    free(runtime->co_counts);
    for (pgr_stream * stream : runtime->streams) pgr_stream_free(stream);
    pgr_arena_free(runtime->arena);
    pgrn_close(runtime->store);
    delete runtime;
}
