#include "peregrine_compact.h"

#include "ggml-backend.h"

#include <algorithm>
#include <array>
#include <cstdio>
#include <cstring>
#include <new>
#include <vector>

struct pgr_compact_layer {
    uint16_t layer = 0;
    std::array<ggml_tensor *, 3> tensor = {};
};

struct pgr_compact {
    std::vector<unsigned char> metadata;
    ggml_context * context = nullptr;
    std::vector<pgr_compact_layer> layers;
};

static int pgr_compact_fail(char * error, size_t capacity, const char * message) {
    if (error && capacity) std::snprintf(error, capacity, "%s", message);
    return -1;
}

static bool pgr_compact_parse(const char * name, uint16_t * layer, int * role) {
    if (!name || !layer || !role) return false;
    const char * suffixes[] = {
        ".ffn_gate_exps.weight%n",
        ".ffn_up_exps.weight%n",
        ".ffn_down_exps.weight%n",
    };
    for (int candidate = 0; candidate < 3; ++candidate) {
        unsigned value = 0;
        int consumed = 0;
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

pgr_compact * pgr_compact_new(
        ggml_context * external_ctx,
        pgr_runtime * runtime,
        char * error,
        size_t error_capacity) {
    if (error && error_capacity) error[0] = '\0';
    if (!external_ctx || !runtime || !pgr_runtime_uses_backend_arena(runtime)) {
        pgr_compact_fail(error, error_capacity, "compact slots require an external tensor directory and backend arena");
        return nullptr;
    }
    size_t tensor_count = 0;
    for (ggml_tensor * tensor = ggml_get_first_tensor(external_ctx); tensor;
            tensor = ggml_get_next_tensor(external_ctx, tensor)) tensor_count++;
    if (tensor_count == 0 || tensor_count > SIZE_MAX / ggml_tensor_overhead()) {
        pgr_compact_fail(error, error_capacity, "invalid compact tensor count");
        return nullptr;
    }
    auto * compact = new (std::nothrow) pgr_compact;
    if (!compact) return nullptr;
    compact->metadata.resize(ggml_tensor_overhead() * tensor_count + 1024);
    ggml_init_params params = {
        /*.mem_size   =*/ compact->metadata.size(),
        /*.mem_buffer =*/ compact->metadata.data(),
        /*.no_alloc   =*/ true,
    };
    compact->context = ggml_init(params);
    if (!compact->context) {
        pgr_compact_fail(error, error_capacity, "compact tensor metadata allocation failed");
        pgr_compact_free(compact);
        return nullptr;
    }

    for (ggml_tensor * source = ggml_get_first_tensor(external_ctx); source;
            source = ggml_get_next_tensor(external_ctx, source)) {
        uint16_t layer = 0;
        int role = -1;
        pgr_runtime_layer_arena arena{};
        if (!pgr_compact_parse(source->name, &layer, &role) || source->ne[2] <= 0 ||
                pgr_runtime_layer_arena_get(runtime, layer, &arena) != 0 || arena.capacity == 0 ||
                arena.capacity > INT64_MAX || ggml_nbytes(source) % static_cast<size_t>(source->ne[2]) != 0 ||
                ggml_nbytes(source) / static_cast<size_t>(source->ne[2]) != arena.role_bytes[role]) {
            pgr_compact_fail(error, error_capacity, "compact tensor geometry differs from PGRN arena");
            pgr_compact_free(compact);
            return nullptr;
        }
        auto found = std::find_if(compact->layers.begin(), compact->layers.end(),
                [layer](const pgr_compact_layer & item) { return item.layer == layer; });
        if (found == compact->layers.end()) {
            compact->layers.push_back({layer, {}});
            found = std::prev(compact->layers.end());
        }
        if (found->tensor[role]) {
            pgr_compact_fail(error, error_capacity, "duplicate compact tensor role");
            pgr_compact_free(compact);
            return nullptr;
        }
        ggml_tensor * tensor = ggml_new_tensor_3d(
                compact->context, source->type, source->ne[0], source->ne[1],
                static_cast<int64_t>(arena.capacity));
        tensor->nb[2] = arena.record_bytes;
        tensor->nb[3] = arena.record_bytes * arena.capacity;
        ggml_format_name(tensor, "pgrn.compact.%s", source->name);
        auto * role_base = static_cast<unsigned char *>(arena.base) + arena.role_offset[role];
        if (ggml_backend_tensor_alloc(arena.buffer, tensor, role_base) != GGML_STATUS_SUCCESS) {
            pgr_compact_fail(error, error_capacity, "backend rejected compact strided expert tensor");
            pgr_compact_free(compact);
            return nullptr;
        }
        found->tensor[role] = tensor;
    }
    std::sort(compact->layers.begin(), compact->layers.end(),
            [](const pgr_compact_layer & a, const pgr_compact_layer & b) { return a.layer < b.layer; });
    for (const auto & layer : compact->layers) {
        if (!layer.tensor[0] || !layer.tensor[1] || !layer.tensor[2]) {
            pgr_compact_fail(error, error_capacity, "compact layer is missing gate/up/down tensors");
            pgr_compact_free(compact);
            return nullptr;
        }
    }
    return compact;
}

ggml_tensor * pgr_compact_tensor(const pgr_compact * compact, uint16_t layer, int role) {
    if (!compact || role < 0 || role > 2) return nullptr;
    const auto found = std::lower_bound(compact->layers.begin(), compact->layers.end(), layer,
            [](const pgr_compact_layer & item, uint16_t value) { return item.layer < value; });
    return found != compact->layers.end() && found->layer == layer ? found->tensor[role] : nullptr;
}

size_t pgr_compact_layers(const pgr_compact * compact) {
    return compact ? compact->layers.size() : 0;
}

void pgr_compact_free(pgr_compact * compact) {
    if (!compact) return;
    if (compact->context) ggml_free(compact->context);
    delete compact;
}
