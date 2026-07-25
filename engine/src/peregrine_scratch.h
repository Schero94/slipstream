/* One immutable backend scratch buffer shared by all streamed MoE layers. */
#ifndef PEREGRINE_SCRATCH_H
#define PEREGRINE_SCRATCH_H

#include "peregrine_runtime.h"
#include "peregrine_stage.h"

#include "ggml-backend.h"

#include <stddef.h>
#include <stdint.h>

struct pgr_scratch;

/* Takes tensor metadata from an external-expert context and attaches every
 * separate gate/up/down expert tensor to one of three shared fixed regions.
 * The caller retains the context; the returned object owns the backend buffer. */
pgr_scratch * pgr_scratch_new(
        struct ggml_context * external_ctx,
        ggml_backend_buffer_type_t buft,
        char * error,
        size_t error_capacity);

int pgr_scratch_stage(
        pgr_scratch * scratch,
        pgr_runtime * runtime,
        uint16_t layer,
        const int32_t * expert_ids,
        size_t expert_id_count,
        struct pgr_stage_stats * stats,
        char * error,
        size_t error_capacity);

size_t pgr_scratch_bytes(const pgr_scratch * scratch);
size_t pgr_scratch_layers(const pgr_scratch * scratch);
struct ggml_tensor * pgr_scratch_tensor(const pgr_scratch * scratch, uint16_t layer, int role);
ggml_backend_buffer_t pgr_scratch_buffer(const pgr_scratch * scratch);
void pgr_scratch_free(pgr_scratch * scratch);

enum {
    PGR_SCRATCH_GATE = 0,
    PGR_SCRATCH_UP = 1,
    PGR_SCRATCH_DOWN = 2,
};

#endif
