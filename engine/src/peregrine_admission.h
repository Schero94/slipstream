/* Pure checked-arithmetic admission planner for bounded expert streaming. */
#ifndef PEREGRINE_ADMISSION_H
#define PEREGRINE_ADMISSION_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    PGR_ADMISSION_REFUSE = 0,
    PGR_ADMISSION_WARN,
    PGR_ADMISSION_OK,
} pgr_admission_status;

typedef enum {
    PGR_LOAD_REFUSE = 0,
    PGR_LOAD_RESIDENT,
    PGR_LOAD_STREAMING,
} pgr_load_mode;

typedef struct {
    uint64_t total_bytes;
    uint64_t available_bytes;
    int      available_known;
    uint64_t model_bytes;
    uint64_t expert_total_bytes;
    uint64_t kv_bytes;
    uint64_t overhead_bytes;
    uint64_t staging_bytes;
    uint64_t min_headroom_bytes;
    uint64_t requested_cache_bytes; /* zero selects the largest safe cache */
    uint64_t already_allocated_bytes; /* subset of resident_bytes present before live snapshot */
} pgr_admission_input;

typedef struct {
    pgr_admission_status status;
    pgr_load_mode mode;
    uint64_t static_ceiling_bytes;
    uint64_t reserved_headroom_bytes;
    uint64_t mandatory_resident_bytes;
    uint64_t expert_cache_bytes;
    uint64_t streamed_expert_bytes;
    uint64_t resident_bytes;
    uint64_t system_free_after_load_bytes;
    uint64_t recommended_wired_limit_mb;
} pgr_admission_plan;

/* Returns zero for a valid decision (including REFUSE), -1 for invalid or
 * overflowing inputs. Invalid inputs always leave plan.status == REFUSE. */
int pgr_admission_compute(const pgr_admission_input * input, pgr_admission_plan * plan);

/* Re-check the user-visible reserve after context/KV/compute allocations that
 * occur later than model admission. Returns zero only when the full reserve
 * still exists in the live system snapshot. */
int pgr_admission_check_live_headroom(
        int available_known, uint64_t available_bytes, uint64_t min_headroom_bytes);

#ifdef __cplusplus
}
#endif
#endif
