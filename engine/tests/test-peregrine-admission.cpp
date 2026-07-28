#include "../src/peregrine_admission.h"

#include <cstdint>
#include <cstdio>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)
static constexpr uint64_t GiB = 1024ULL * 1024ULL * 1024ULL;

static pgr_admission_input base() {
    return {
        36 * GiB, 36 * GiB, 1,
        23 * GiB, 18 * GiB,
        2 * GiB, 2 * GiB, 0,
        9 * GiB, 0, 0,
    };
}

int main() {
    pgr_admission_plan plan{};
    auto in = base();
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_OK);
    CHECK(plan.mode == PGR_LOAD_RESIDENT);
    CHECK(plan.resident_bytes == 27 * GiB);
    CHECK(plan.streamed_expert_bytes == 0);
    CHECK(plan.recommended_wired_limit_mb == 27 * 1024);

    in = base(); in.model_bytes = 40 * GiB; in.expert_total_bytes = 34 * GiB;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_OK && plan.mode == PGR_LOAD_STREAMING);
    CHECK(plan.mandatory_resident_bytes == 10 * GiB);
    CHECK(plan.expert_cache_bytes == 17 * GiB);
    CHECK(plan.streamed_expert_bytes == 17 * GiB);
    CHECK(plan.resident_bytes == plan.static_ceiling_bytes);

    // A static fit is insufficient: the requested foreground headroom must
    // remain available after allocating the complete admitted resident set.
    in.available_bytes = 35 * GiB;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_WARN);

    in.available_bytes = 36 * GiB;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_OK);
    CHECK(plan.system_free_after_load_bytes == 9 * GiB);

    in = base(); in.already_allocated_bytes = 20 * GiB; in.available_bytes = 16 * GiB;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_OK);
    CHECK(plan.system_free_after_load_bytes == 9 * GiB);
    in.available_bytes -= 1;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_WARN);

    in = base(); in.available_known = 0;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_REFUSE);

    in = base(); in.model_bytes = 30 * GiB; in.expert_total_bytes = 0;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_REFUSE);

    // Asking for more cache than the machine can hold is refused: the request does
    // not fit, and quietly shrinking it would hide that from the caller.
    in = base(); in.model_bytes = 40 * GiB; in.expert_total_bytes = 34 * GiB;
    in.requested_cache_bytes = 18 * GiB;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_REFUSE);

    // Asking for more cache than there are experts is a different thing entirely:
    // the parameter is an upper bound, and a bound above the total is satisfied by
    // caching all of them. Callers cannot read expert_total_bytes before opening the
    // file, so a round number has to be allowed to be too generous. base() has 18 GiB
    // of experts and room for exactly that, so the clamp lands on a resident plan.
    in = base(); in.requested_cache_bytes = 30 * GiB;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_OK);
    CHECK(plan.expert_cache_bytes == 18 * GiB);
    CHECK(plan.streamed_expert_bytes == 0);
    CHECK(plan.mode == PGR_LOAD_RESIDENT);

    // Over-generous *and* beyond the machine: clamping to the expert total comes
    // first, and 34 GiB of experts still exceeds the 17 GiB that fits, so this is
    // refused rather than quietly shrunk. Omitting the flag is what asks for "as
    // much as fits" — the case asserted further up.
    in = base(); in.model_bytes = 40 * GiB; in.expert_total_bytes = 34 * GiB;
    in.requested_cache_bytes = 99 * GiB;
    CHECK(pgr_admission_compute(&in, &plan) == 0);
    CHECK(plan.status == PGR_ADMISSION_REFUSE);

    in = base(); in.kv_bytes = UINT64_MAX;
    CHECK(pgr_admission_compute(&in, &plan) != 0);
    CHECK(plan.status == PGR_ADMISSION_REFUSE);

    CHECK(pgr_admission_check_live_headroom(1, 9 * GiB, 9 * GiB) == 0);
    CHECK(pgr_admission_check_live_headroom(1, 9 * GiB - 1, 9 * GiB) != 0);
    CHECK(pgr_admission_check_live_headroom(0, 20 * GiB, 9 * GiB) != 0);
    CHECK(pgr_admission_check_live_headroom(1, 20 * GiB, 0) != 0);

    std::printf("PGR_ADMISSION_OK\n");
    return 0;
}
