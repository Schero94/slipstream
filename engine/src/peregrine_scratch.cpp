#include "peregrine_scratch.h"

#include <algorithm>
#include <array>
#include <cstdio>
#include <cstring>
#include <limits>
#include <new>
#include <vector>

struct pgr_scratch_layer {
    uint16_t layer = 0;
    std::array<ggml_tensor *, 3> tensor = {};
};

struct pgr_scratch {
    ggml_backend_buffer_t buffer = nullptr;
    size_t bytes = 0;
    std::vector<pgr_scratch_layer> layers;
};

static int pgr_scratch_fail(char * error, size_t capacity, const char * message) {
    if (error && capacity) std::snprintf(error, capacity, "%s", message ? message : "scratch error");
    return -1;
}

static bool pgr_scratch_parse(const char * name, uint16_t * layer, int * role) {
    if (!name || !layer || !role) return false;
    unsigned value = 0;
    int consumed = 0;
    const char * suffixes[] = {
        ".ffn_gate_exps.weight%n",
        ".ffn_up_exps.weight%n",
        ".ffn_down_exps.weight%n",
    };
    for (int candidate = 0; candidate < 3; ++candidate) {
        consumed = 0;
        char format[64];
        std::snprintf(format, sizeof(format), "blk.%%u%s", suffixes[candidate]);
        if (std::sscanf(name, format, &value, &consumed) == 1 && consumed > 0 &&
                name[consumed] == '\0' && value <= UINT16_MAX) {
            *layer = static_cast<uint16_t>(value);
            *role = candidate;
            return true;
        }
    }
    return false;
}

static bool pgr_scratch_add(size_t a, size_t b, size_t * out) {
    if (a > std::numeric_limits<size_t>::max() - b) return false;
    *out = a + b;
    return true;
}

static bool pgr_scratch_align(size_t value, size_t alignment, size_t * out) {
    if (alignment == 0 || (alignment & (alignment - 1)) != 0) return false;
    size_t sum = 0;
    if (!pgr_scratch_add(value, alignment - 1, &sum)) return false;
    *out = sum & ~(alignment - 1);
    return true;
}

pgr_scratch * pgr_scratch_new(
        ggml_context * external_ctx,
        ggml_backend_buffer_type_t buft,
        char * error,
        size_t error_capacity) {
    if (error && error_capacity) error[0] = '\0';
    if (!external_ctx || !buft) {
        pgr_scratch_fail(error, error_capacity, "invalid scratch context or buffer type");
        return nullptr;
    }

    auto * scratch = new (std::nothrow) pgr_scratch;
    if (!scratch) {
        pgr_scratch_fail(error, error_capacity, "scratch allocation failed");
        return nullptr;
    }
    std::array<size_t, 3> region_bytes = {};
    for (ggml_tensor * tensor = ggml_get_first_tensor(external_ctx); tensor;
            tensor = ggml_get_next_tensor(external_ctx, tensor)) {
        uint16_t layer = 0;
        int role = -1;
        if (!pgr_scratch_parse(tensor->name, &layer, &role)) {
            pgr_scratch_fail(error, error_capacity, "external context contains a non-separate expert tensor");
            pgr_scratch_free(scratch);
            return nullptr;
        }
        auto found = std::find_if(scratch->layers.begin(), scratch->layers.end(),
                [layer](const pgr_scratch_layer & item) { return item.layer == layer; });
        if (found == scratch->layers.end()) {
            scratch->layers.push_back({layer, {}});
            found = std::prev(scratch->layers.end());
        }
        if (found->tensor[role] != nullptr || tensor->ne[2] <= 0 || tensor->buffer || tensor->data) {
            pgr_scratch_fail(error, error_capacity, "duplicate, allocated, or invalid external expert tensor");
            pgr_scratch_free(scratch);
            return nullptr;
        }
        found->tensor[role] = tensor;
        region_bytes[role] = std::max(region_bytes[role], ggml_backend_buft_get_alloc_size(buft, tensor));
    }
    if (scratch->layers.empty()) {
        pgr_scratch_fail(error, error_capacity, "external expert context is empty");
        pgr_scratch_free(scratch);
        return nullptr;
    }
    std::sort(scratch->layers.begin(), scratch->layers.end(),
            [](const pgr_scratch_layer & a, const pgr_scratch_layer & b) { return a.layer < b.layer; });
    for (const auto & layer : scratch->layers) {
        if (!layer.tensor[0] || !layer.tensor[1] || !layer.tensor[2] ||
                layer.tensor[0]->ne[2] != layer.tensor[1]->ne[2] ||
                layer.tensor[0]->ne[2] != layer.tensor[2]->ne[2]) {
            pgr_scratch_fail(error, error_capacity, "each streamed layer requires matching gate/up/down tensors");
            pgr_scratch_free(scratch);
            return nullptr;
        }
    }

    const size_t alignment = ggml_backend_buft_get_alignment(buft);
    std::array<size_t, 3> offset = {};
    size_t cursor = 0;
    for (int role = 0; role < 3; ++role) {
        if (!pgr_scratch_align(cursor, alignment, &offset[role]) ||
                !pgr_scratch_add(offset[role], region_bytes[role], &cursor)) {
            pgr_scratch_fail(error, error_capacity, "scratch size overflow");
            pgr_scratch_free(scratch);
            return nullptr;
        }
    }
    if (!pgr_scratch_align(cursor, alignment, &scratch->bytes) || scratch->bytes == 0) {
        pgr_scratch_fail(error, error_capacity, "invalid scratch size");
        pgr_scratch_free(scratch);
        return nullptr;
    }
    scratch->buffer = ggml_backend_buft_alloc_buffer(buft, scratch->bytes);
    if (!scratch->buffer) {
        pgr_scratch_fail(error, error_capacity, "fixed backend scratch allocation failed");
        pgr_scratch_free(scratch);
        return nullptr;
    }
    void * base = ggml_backend_buffer_get_base(scratch->buffer);
    if (!base) {
        pgr_scratch_fail(error, error_capacity, "scratch backend exposes no allocation base");
        pgr_scratch_free(scratch);
        return nullptr;
    }
    for (auto & layer : scratch->layers) {
        for (int role = 0; role < 3; ++role) {
            const enum ggml_status status = ggml_backend_tensor_alloc(
                    scratch->buffer, layer.tensor[role], static_cast<unsigned char *>(base) + offset[role]);
            if (status != GGML_STATUS_SUCCESS) {
                pgr_scratch_fail(error, error_capacity, "failed to bind tensor to fixed scratch region");
                pgr_scratch_free(scratch);
                return nullptr;
            }
        }
    }
    ggml_backend_buffer_clear(scratch->buffer, 0);
    ggml_backend_buffer_set_usage(scratch->buffer, GGML_BACKEND_BUFFER_USAGE_WEIGHTS);
    return scratch;
}

static pgr_scratch_layer * pgr_scratch_find(pgr_scratch * scratch, uint16_t layer) {
    if (!scratch) return nullptr;
    auto found = std::lower_bound(scratch->layers.begin(), scratch->layers.end(), layer,
            [](const pgr_scratch_layer & item, uint16_t value) { return item.layer < value; });
    return found != scratch->layers.end() && found->layer == layer ? &*found : nullptr;
}

int pgr_scratch_stage(
        pgr_scratch * scratch,
        pgr_runtime * runtime,
        uint16_t layer,
        const int32_t * expert_ids,
        size_t expert_id_count,
        pgr_stage_stats * stats,
        char * error,
        size_t error_capacity) {
    pgr_scratch_layer * found = pgr_scratch_find(scratch, layer);
    if (!found) return pgr_scratch_fail(error, error_capacity, "selected layer is absent from scratch directory");
    return pgr_stage_selected(runtime, layer, expert_ids, expert_id_count,
            found->tensor[PGR_SCRATCH_GATE], found->tensor[PGR_SCRATCH_UP], found->tensor[PGR_SCRATCH_DOWN],
            stats, error, error_capacity);
}

size_t pgr_scratch_bytes(const pgr_scratch * scratch) { return scratch ? scratch->bytes : 0; }
size_t pgr_scratch_layers(const pgr_scratch * scratch) { return scratch ? scratch->layers.size() : 0; }
ggml_tensor * pgr_scratch_tensor(const pgr_scratch * scratch, uint16_t layer, int role) {
    if (!scratch || role < 0 || role > 2) return nullptr;
    auto found = std::lower_bound(scratch->layers.begin(), scratch->layers.end(), layer,
            [](const pgr_scratch_layer & item, uint16_t value) { return item.layer < value; });
    return found != scratch->layers.end() && found->layer == layer ? found->tensor[role] : nullptr;
}
ggml_backend_buffer_t pgr_scratch_buffer(const pgr_scratch * scratch) { return scratch ? scratch->buffer : nullptr; }
void pgr_scratch_free(pgr_scratch * scratch) {
    if (!scratch) return;
    if (scratch->buffer) ggml_backend_buffer_free(scratch->buffer);
    delete scratch;
}
