/* Copy admitted PGRN records into preallocated GGML expert tensor slots. */
#ifndef PEREGRINE_STAGE_H
#define PEREGRINE_STAGE_H

#include "peregrine_runtime.h"

#include "ggml.h"

#include <stddef.h>
#include <stdint.h>

struct pgr_stage_stats {
    size_t experts_requested;
    size_t experts_copied;
    size_t bytes_uploaded;
    uint64_t fetch_us;
    uint64_t upload_us;
};

/* PGRN records are byte-exact gate, up, down slices in that order.  The
 * destination tensors remain full-shape descriptors, but their backing store
 * may be one shared, fixed scratch allocation. */
int pgr_stage_selected(
        pgr_runtime * runtime,
        uint16_t layer,
        const int32_t * expert_ids,
        size_t expert_id_count,
        struct ggml_tensor * gate,
        struct ggml_tensor * up,
        struct ggml_tensor * down,
        struct pgr_stage_stats * stats,
        char * error,
        size_t error_capacity);

#endif
