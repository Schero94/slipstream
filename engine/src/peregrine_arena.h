/* One fixed ggml backend allocation for compact PGRN expert slots. */
#ifndef PEREGRINE_ARENA_H
#define PEREGRINE_ARENA_H

#include "ggml-backend.h"

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pgr_arena pgr_arena;

pgr_arena * pgr_arena_new(
        ggml_backend_buffer_type_t buft, size_t bytes,
        char * error, size_t error_capacity);
int pgr_arena_slice(
        pgr_arena * arena, size_t offset, size_t bytes, void ** data,
        char * error, size_t error_capacity);
size_t pgr_arena_bytes(const pgr_arena * arena);
void * pgr_arena_base(const pgr_arena * arena);
ggml_backend_buffer_t pgr_arena_buffer(const pgr_arena * arena);
void pgr_arena_free(pgr_arena * arena);

#ifdef __cplusplus
}
#endif
#endif
