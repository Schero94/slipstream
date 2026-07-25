#include "peregrine_admission.h"

#include <stddef.h>
#include <string.h>

#define PGR_CURRENT_MARGIN_BYTES (1024ULL * 1024ULL * 1024ULL)
#define PGR_MIB (1024ULL * 1024ULL)

static int pgr_add_u64(uint64_t a, uint64_t b, uint64_t * out) {
    if (a > UINT64_MAX - b) return -1;
    *out = a + b;
    return 0;
}

int pgr_admission_compute(const pgr_admission_input * input, pgr_admission_plan * plan) {
    if (!plan) return -1;
    memset(plan, 0, sizeof(*plan));
    plan->status = PGR_ADMISSION_REFUSE;
    plan->mode = PGR_LOAD_REFUSE;
    if (!input || input->total_bytes == 0 || input->model_bytes == 0 ||
            input->expert_total_bytes > input->model_bytes ||
            input->requested_cache_bytes > input->expert_total_bytes) return -1;

    if (input->total_bytes <= input->min_headroom_bytes) return 0;
    plan->reserved_headroom_bytes = input->min_headroom_bytes;
    plan->static_ceiling_bytes = input->total_bytes - input->min_headroom_bytes;
    plan->recommended_wired_limit_mb = plan->static_ceiling_bytes / PGR_MIB;

    const uint64_t dense_bytes = input->model_bytes - input->expert_total_bytes;
    uint64_t mandatory = 0;
    if (pgr_add_u64(dense_bytes, input->kv_bytes, &mandatory) != 0 ||
            pgr_add_u64(mandatory, input->overhead_bytes, &mandatory) != 0 ||
            pgr_add_u64(mandatory, input->staging_bytes, &mandatory) != 0) return -1;
    plan->mandatory_resident_bytes = mandatory;
    if (mandatory > plan->static_ceiling_bytes) return 0;

    const uint64_t max_cache = plan->static_ceiling_bytes - mandatory;
    uint64_t cache = input->requested_cache_bytes;
    if (cache == 0) cache = input->expert_total_bytes < max_cache ? input->expert_total_bytes : max_cache;
    if (cache > max_cache) return 0;

    plan->expert_cache_bytes = cache;
    plan->streamed_expert_bytes = input->expert_total_bytes - cache;
    if (pgr_add_u64(mandatory, cache, &plan->resident_bytes) != 0) return -1;
    plan->system_free_after_load_bytes = input->total_bytes - plan->resident_bytes;
    plan->mode = plan->streamed_expert_bytes ? PGR_LOAD_STREAMING : PGR_LOAD_RESIDENT;

    /* Unknown current memory fails closed. Static fit alone did not prevent the
     * original watchdog failure when other processes already held memory. */
    if (!input->available_known) {
        plan->mode = PGR_LOAD_REFUSE;
        return 0;
    }
    if (input->already_allocated_bytes > plan->resident_bytes) return -1;
    const uint64_t future_resident = plan->resident_bytes - input->already_allocated_bytes;
    const uint64_t current_reserve = input->min_headroom_bytes > PGR_CURRENT_MARGIN_BYTES
        ? input->min_headroom_bytes : PGR_CURRENT_MARGIN_BYTES;
    uint64_t current_need = 0;
    if (pgr_add_u64(future_resident, current_reserve, &current_need) != 0) return -1;
    plan->system_free_after_load_bytes = input->available_bytes > future_resident
        ? input->available_bytes - future_resident : 0;
    plan->status = input->available_bytes < current_need ? PGR_ADMISSION_WARN : PGR_ADMISSION_OK;
    return 0;
}

int pgr_admission_check_live_headroom(
        int available_known, uint64_t available_bytes, uint64_t min_headroom_bytes) {
    return available_known && min_headroom_bytes > 0 && available_bytes >= min_headroom_bytes ? 0 : -1;
}
