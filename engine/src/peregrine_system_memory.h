/* Conservative current-memory snapshot for admission-first startup. */
#ifndef PEREGRINE_SYSTEM_MEMORY_H
#define PEREGRINE_SYSTEM_MEMORY_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    uint64_t total_bytes;
    uint64_t available_bytes;
    int available_known;
} pgr_system_memory;

/* Returns zero when total memory is known.  Unknown current availability is
 * represented explicitly and will be refused by pgr_admission_compute. */
int pgr_system_memory_read(pgr_system_memory * out);

/* How much memory to keep out of the engine's reach, for a machine of this size.
 *
 * This exists so that streaming needs one flag rather than three: a caller that
 * knows the model but not the host should not have to invent this number. What the
 * reserve protects differs by platform, so the policy does too.
 *
 * macOS: 3 GiB, the value qualified by two consecutive passing runs at a 14 GiB
 * cache. 1.5 GiB was tried and empirically refused — three of four runs stalled at
 * 0.07-0.09 tok/s with zero swapouts, i.e. Metal residency thrash rather than
 * paging, which no swap counter would have caught. It is capped at a quarter of
 * total so that a small Mac is not locked out of streaming altogether; that cap is
 * reasoning, not a measurement, since every number here comes from a 36 GiB machine.
 *
 * Elsewhere: there is no Metal residency to protect, and the reserve is there to
 * stay clear of the OOM killer. An eighth of total, floored at 512 MiB and capped at
 * the macOS figure. On Linux total_bytes is already the cgroup limit where one is
 * set, so a container gets a reserve proportional to what it was actually given.
 *
 * Returns 0 only for total_bytes == 0. */
uint64_t pgr_default_headroom_bytes(uint64_t total_bytes);

#ifdef __cplusplus
}
#endif
#endif
