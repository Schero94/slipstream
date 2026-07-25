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

#ifdef __cplusplus
}
#endif
#endif
