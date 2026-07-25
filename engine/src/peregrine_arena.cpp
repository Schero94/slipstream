#include "peregrine_arena.h"

#include <cstdio>
#include <new>

struct pgr_arena {
    ggml_backend_buffer_t buffer = nullptr;
    void * base = nullptr;
    size_t bytes = 0;
};

static int pgr_arena_fail(char * error, size_t capacity, const char * message) {
    if (error && capacity) std::snprintf(error, capacity, "%s", message);
    return -1;
}

pgr_arena * pgr_arena_new(
        ggml_backend_buffer_type_t buft, size_t bytes,
        char * error, size_t error_capacity) {
    if (error && error_capacity) error[0] = '\0';
    if (!buft || bytes == 0) {
        pgr_arena_fail(error, error_capacity, "invalid arena buffer type or byte count");
        return nullptr;
    }
    auto * arena = new (std::nothrow) pgr_arena;
    if (!arena) {
        pgr_arena_fail(error, error_capacity, "arena metadata allocation failed");
        return nullptr;
    }
    arena->buffer = ggml_backend_buft_alloc_buffer(buft, bytes);
    if (!arena->buffer) {
        pgr_arena_fail(error, error_capacity, "fixed backend arena allocation failed");
        pgr_arena_free(arena);
        return nullptr;
    }
    arena->base = ggml_backend_buffer_get_base(arena->buffer);
    arena->bytes = ggml_backend_buffer_get_size(arena->buffer);
    if (!arena->base || arena->bytes < bytes) {
        pgr_arena_fail(error, error_capacity, "backend arena is not host-addressable or is truncated");
        pgr_arena_free(arena);
        return nullptr;
    }
    ggml_backend_buffer_clear(arena->buffer, 0);
    ggml_backend_buffer_set_usage(arena->buffer, GGML_BACKEND_BUFFER_USAGE_WEIGHTS);
    return arena;
}

int pgr_arena_slice(
        pgr_arena * arena, size_t offset, size_t bytes, void ** data,
        char * error, size_t error_capacity) {
    if (data) *data = nullptr;
    if (!arena || !data || bytes == 0 || offset > arena->bytes || bytes > arena->bytes - offset) {
        return pgr_arena_fail(error, error_capacity, "arena slice is outside the fixed allocation");
    }
    *data = static_cast<unsigned char *>(arena->base) + offset;
    if (error && error_capacity) error[0] = '\0';
    return 0;
}

size_t pgr_arena_bytes(const pgr_arena * arena) { return arena ? arena->bytes : 0; }
void * pgr_arena_base(const pgr_arena * arena) { return arena ? arena->base : nullptr; }
ggml_backend_buffer_t pgr_arena_buffer(const pgr_arena * arena) { return arena ? arena->buffer : nullptr; }
void pgr_arena_free(pgr_arena * arena) {
    if (!arena) return;
    if (arena->buffer) ggml_backend_buffer_free(arena->buffer);
    delete arena;
}
