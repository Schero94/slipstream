/* Compact ggml tensor views over the one fixed PGRN backend arena. */
#ifndef PEREGRINE_COMPACT_H
#define PEREGRINE_COMPACT_H

#include "peregrine_runtime.h"

#include "ggml.h"

#include <stddef.h>
#include <stdint.h>

struct pgr_compact;

pgr_compact * pgr_compact_new(
        ggml_context * external_ctx,
        pgr_runtime * runtime,
        char * error,
        size_t error_capacity);

ggml_tensor * pgr_compact_tensor(const pgr_compact * compact, uint16_t layer, int role);
size_t pgr_compact_layers(const pgr_compact * compact);
void pgr_compact_free(pgr_compact * compact);

#endif
