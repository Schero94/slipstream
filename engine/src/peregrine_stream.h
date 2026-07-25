/* peregrine_stream.h — bounded, streaming expert cache for the llama.cpp fork. */
#ifndef PEREGRINE_STREAM_H
#define PEREGRINE_STREAM_H
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pgr_stream pgr_stream;

typedef enum {
    PGR_STREAM_OK = 0,
    PGR_STREAM_INVALID,
    PGR_STREAM_IO,
    PGR_STREAM_SHORT_READ,
    PGR_STREAM_OVERFLOW,
    PGR_STREAM_INTEGRITY,
} pgr_stream_status;

typedef pgr_stream_status (*pgr_stream_loader)(
        void * user_data, long key, void * dst, size_t dst_capacity,
        size_t * loaded_bytes, char * error, size_t error_capacity);

typedef struct {
    size_t   slot_bytes;
    int      capacity;
    int      clox_k;
    int      hot_capacity;
    uint8_t  promote_hits;
    uint64_t demote_idle_epochs;
    uint64_t cooldown_epochs;
    int      io_width;   /* parallel staging buffers / cold-read threads for
                          * pgr_stream_get_many; 0 or 1 = serial (default). Grows
                          * the fixed high-water by (io_width-1)*slot_bytes. */
} pgr_stream_params;

/* Open `path` read-only; allocate a FIXED resident buffer of capacity*record_bytes
 * (the HARD cap — resident memory can never exceed this). k is the cloxcache K. */
pgr_stream *pgr_stream_new(const char *path, size_t record_bytes, int capacity, int k);

/* Create a fixed-slot cache whose misses are filled by a bounded loader callback. */
pgr_stream *pgr_stream_new_loader(
        size_t slot_bytes, int capacity, int k,
        pgr_stream_loader loader, void * user_data);

/* Parameterized constructor for logical HOT/WARM slots in the same fixed bytes. */
pgr_stream *pgr_stream_new_loader_tier(
        const pgr_stream_params * params,
        pgr_stream_loader loader, void * user_data);

/* Use caller-owned fixed slot bytes; only one record of staging is allocated. */
pgr_stream *pgr_stream_new_loader_external(
        const pgr_stream_params * params,
        void * slot_storage, size_t slot_storage_bytes,
        pgr_stream_loader loader, void * user_data);

/* Return the expert's resident bytes in `*data`, loading it from `offset` on a
 * miss. A failed or short read is never published into the cache. */
pgr_stream_status pgr_stream_get(
        pgr_stream *s, long key, off_t offset, const void **data, int *hit);
pgr_stream_status pgr_stream_get_key(
        pgr_stream *s, long key, const void **data, size_t *data_size, int *hit);
pgr_stream_status pgr_stream_get_key_heat(
        pgr_stream *s, long key, float heat,
        const void **data, size_t *data_size, int *hit);

/* Batch fetch: resolve `n` keys at once on a loader-backed stream. COLD records are
 * read in parallel across up to `io_width` reader threads (from params), then
 * published in input order on the CALLING thread, so the single-threaded tier policy
 * is never touched concurrently. Every returned slot (hit or freshly loaded) is pinned
 * until the next pgr_stream_batch_begin, so call that first; keys must be distinct
 * within one batch and their count must not exceed the cache capacity. The loader may
 * run on worker threads and so must be reentrant (pread with explicit offset is; shared
 * mutable user_data is not). A failed COLD read never poisons a resident slot; the first
 * failure's status is returned and keys published before it stay resident. heats,
 * size_out and hit_out may be NULL. */
pgr_stream_status pgr_stream_get_many(
        pgr_stream *s,
        const long *keys, const float *heats, int n,
        const void **data_out, size_t *size_out, int *hit_out);

/* Best-effort speculative cache warm: read the COLD keys in parallel and publish them
 * WITHOUT pinning and WITHOUT returning data — pre-load a predicted expert set during the
 * previous layer's compute. Resident keys are left untouched; failed reads are skipped
 * (a misprediction never errors or poisons). Returns experts warmed, -1 on invalid args.
 * Call on a stream that is not mid-batch. */
int pgr_stream_prefetch_many(
        pgr_stream *s, const long *keys, const float *heats, int n);

/* Start a new graph-use epoch, releasing the prior epoch's slot pins. The slot
 * variant pins the returned resident slot until the next begin call. */
void pgr_stream_batch_begin(pgr_stream *s);
pgr_stream_status pgr_stream_get_key_heat_slot(
        pgr_stream *s, long key, float heat,
        const void **data, size_t *data_size, int *hit,
        int *slot, uint64_t *generation);

size_t pgr_stream_resident_bytes(const pgr_stream *s);   /* the hard cap, for admission */
size_t pgr_stream_high_water_bytes(const pgr_stream *s); /* cache + fixed I/O staging */
long   pgr_stream_hits(const pgr_stream *s);
long   pgr_stream_misses(const pgr_stream *s);
long   pgr_stream_hot_hits(const pgr_stream *s);
long   pgr_stream_warm_hits(const pgr_stream *s);
uint64_t pgr_stream_promotions(const pgr_stream *s);
uint64_t pgr_stream_demotions(const pgr_stream *s);
size_t pgr_stream_hot_count(const pgr_stream *s);
size_t pgr_stream_warm_count(const pgr_stream *s);
int pgr_stream_owns_slot_storage(const pgr_stream *s);
const char *pgr_stream_error(const pgr_stream *s);
void   pgr_stream_free(pgr_stream *s);

#ifdef __cplusplus
}
#endif
#endif
