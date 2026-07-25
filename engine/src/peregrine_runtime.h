/* Admission-first composition of PGRN, cloxcache, and fixed resident slots. */
#ifndef PEREGRINE_RUNTIME_H
#define PEREGRINE_RUNTIME_H

#include "peregrine_admission.h"
#include "peregrine_stream.h"

#include "ggml-backend.h"

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pgr_runtime pgr_runtime;

typedef struct {
    ggml_backend_buffer_t buffer;
    void * base;
    size_t capacity;
    size_t record_bytes;
    size_t role_offset[3];
    size_t role_bytes[3];
} pgr_runtime_layer_arena;

typedef struct {
    const char * pgrn_path;
    const char * model_sha256;
    pgr_admission_input admission;
    int clox_k;
    uint32_t hot_percent;
    uint8_t promote_hits;
    uint64_t demote_idle_epochs;
    uint64_t cooldown_epochs;
    int io_width;   /* parallel cold-read threads per layer stream; 0/1 = serial */
    const char * predict_path;  /* optional PGCT1 hot-set table for speculative prefetch (NULL = off) */
    const char * coupling_path; /* optional PGCC1 coupled table; preferred over predict_path when set */
    ggml_backend_buffer_type_t cache_buft;
} pgr_runtime_params;

pgr_runtime *pgr_runtime_new(
        const pgr_runtime_params * params, pgr_admission_plan * admitted_plan,
        char * error, size_t error_capacity);
pgr_stream_status pgr_runtime_get(
        pgr_runtime * runtime, uint16_t layer, uint16_t expert,
        const void ** data, size_t * data_size, int * hit);
/* Batch fetch of one layer's selected experts. COLD records are read in parallel
 * (up to the layer stream's io_width); every returned record is pinned until the
 * next pgr_runtime_batch_begin/get_many for this layer. `experts` must be distinct
 * and their count must not exceed the layer's cache capacity. data_size/hit may be
 * NULL. Returns the first read failure's status; earlier experts stay resident. */
pgr_stream_status pgr_runtime_get_many(
        pgr_runtime * runtime, uint16_t layer,
        const uint16_t * experts, int n,
        const void ** data, size_t * data_size, int * hit);
void pgr_runtime_batch_begin(pgr_runtime * runtime, uint16_t layer);
pgr_stream_status pgr_runtime_get_slot(
        pgr_runtime * runtime, uint16_t layer, uint16_t expert,
        const void ** data, size_t * data_size, int * hit,
        int * slot, uint64_t * generation);
int pgr_runtime_layer_arena_get(
        const pgr_runtime * runtime, uint16_t layer,
        pgr_runtime_layer_arena * out);
/* Fixed slot count of a layer's cache partition (0 if the layer is absent). A batch
 * fetch that pins more experts than this must fall back to a serial per-expert path. */
size_t pgr_runtime_layer_capacity(const pgr_runtime * runtime, uint16_t layer);
/* Configured parallel cold-read threads per layer stream (1 = serial). */
int pgr_runtime_io_width(const pgr_runtime * runtime);
/* Best-effort speculative warm of a predicted expert set for `layer` (no pin, absent
 * experts skipped). Returns experts warmed into the cache, or -1 on invalid args. */
int pgr_runtime_prefetch(
        pgr_runtime * runtime, uint16_t layer, const uint16_t * experts, int n);
/* Speculative prefetch driven by the loaded PGCT1 table. kick(layer) warms that layer's
 * predicted hot set on a background thread (no-op if no table); settle(layer) joins that
 * prefetch before the layer is staged. Warm cache only - never changes selected experts. */
void pgr_runtime_prefetch_kick(pgr_runtime * runtime, uint16_t layer);
/* Coupled variant: given the experts that fired at src_layer, warm src_layer+1's predicted
 * set (PGCC1 table). No-op without a coupling table. `fired` are raw router expert ids
 * (int32, out-of-u16-range entries skipped); same background-thread + settle contract.
 * Parity-neutral: a misprediction only wastes a speculative read. */
void pgr_runtime_prefetch_kick_coupled(pgr_runtime * runtime, uint16_t src_layer,
                                       const int32_t * fired, int n_fired);
int pgr_runtime_has_coupling(const pgr_runtime * runtime);
/* Online co-activation predictor (opt-in via PGRN_ONLINE_PREDICT): learns L->L+1 expert
 * coupling live and prefetches the predicted next-layer set. Parity-neutral (warms only). */
int pgr_runtime_has_online(const pgr_runtime * runtime);
void pgr_runtime_prefetch_kick_online(pgr_runtime * runtime, uint16_t src_layer,
                                      const int32_t * fired, int n_fired);
void pgr_runtime_prefetch_settle(pgr_runtime * runtime, uint16_t layer);
size_t pgr_runtime_cache_capacity(const pgr_runtime * runtime);
size_t pgr_runtime_cache_bytes(const pgr_runtime * runtime);
size_t pgr_runtime_high_water_bytes(const pgr_runtime * runtime);
long pgr_runtime_hits(const pgr_runtime * runtime);
long pgr_runtime_misses(const pgr_runtime * runtime);
long pgr_runtime_hot_hits(const pgr_runtime * runtime);
long pgr_runtime_warm_hits(const pgr_runtime * runtime);
uint64_t pgr_runtime_promotions(const pgr_runtime * runtime);
uint64_t pgr_runtime_demotions(const pgr_runtime * runtime);
size_t pgr_runtime_hot_count(const pgr_runtime * runtime);
size_t pgr_runtime_warm_count(const pgr_runtime * runtime);
int pgr_runtime_uses_backend_arena(const pgr_runtime * runtime);
const char *pgr_runtime_error(const pgr_runtime * runtime);
const char *pgr_runtime_model_sha256(const pgr_runtime * runtime);
void pgr_runtime_free(pgr_runtime * runtime);

#ifdef __cplusplus
}
#endif
#endif
