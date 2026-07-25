/* peregrine_stream.c — bounded, streaming expert cache (stage 2 of the runtime path).
 *
 * Fuses the verified Peregrine results into one native module:
 *   - cloxcache (peregrine_cache.c) chooses the victim slot (measured eviction),
 *   - a FIXED resident buffer of capacity*record_bytes is the HARD memory cap
 *     (the direct fix for the kernel-panic failure: resident bytes cannot grow),
 *   - pread(2) with F_NOCACHE streams a cold expert into the freed slot on a miss
 *     (the streaming_store method, so the OS cache cannot hide real device cost).
 *
 * The ring slot returned by the cache IS the buffer slot, so eviction and byte
 * placement stay consistent. This is what ggml-metal's expert path calls in stage 3;
 * it is validated standalone here (real file reads, no model, no llama.cpp headers). */
#include "peregrine_stream.h"
#include "peregrine_tier.h"
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifndef F_NOCACHE
#define F_NOCACHE 48   /* macOS: bypass the unified buffer cache */
#endif

struct pgr_stream {
    pgr_tier *policy;
    int       fd;
    size_t    rec;
    int       cap;
    int       io_width; /* >=1; number of parallel staging slices / cold-read threads */
    char     *buf;      /* cap * rec bytes — the hard resident cap */
    int       owns_buf;
    char     *staging;  /* io_width fixed records; failed reads never touch resident slots */
    size_t    cache_bytes;
    size_t    high_water_bytes;
    size_t   *sizes;
    pgr_stream_loader loader;
    void     *loader_user_data;
    long      hits, misses, hot_hits, warm_hits;
    char      error[160];
};

static void pgr_stream_set_error(pgr_stream *s, const char *message) {
    if (!s) return;
    snprintf(s->error, sizeof(s->error), "%s", message ? message : "unknown error");
}

static uintmax_t pgr_off_max(void) {
    return (((uintmax_t)1 << (sizeof(off_t) * CHAR_BIT - 1)) - 1);
}

static pgr_stream *pgr_stream_alloc(
        const pgr_stream_params * params, void * external_slots, size_t external_bytes) {
    pgr_tier_params tier_params;
    const size_t record_bytes = params ? params->slot_bytes : 0;
    const int capacity = params ? params->capacity : 0;
    const int io_width = (params && params->io_width > 1) ? params->io_width : 1;
    if (!params || record_bytes == 0 || capacity < 1 || params->clox_k < 1 ||
            record_bytes > SIZE_MAX / (size_t) capacity) return NULL;
    const size_t cache_bytes = record_bytes * (size_t)capacity;
    if (record_bytes > SIZE_MAX / (size_t)io_width) return NULL;
    const size_t staging_bytes = record_bytes * (size_t)io_width;
    if (cache_bytes > SIZE_MAX - staging_bytes ||
            (size_t)capacity > SIZE_MAX / sizeof(size_t)) return NULL;
    pgr_stream *s = (pgr_stream *)calloc(1, sizeof(pgr_stream));
    if (!s) return NULL;
    s->fd = -1;
    s->rec = record_bytes; s->cap = capacity;
    s->io_width = io_width;
    s->cache_bytes = cache_bytes;
    s->high_water_bytes = cache_bytes + staging_bytes;
    memset(&tier_params, 0, sizeof(tier_params));
    tier_params.capacity = capacity;
    tier_params.clox_k = params->clox_k;
    tier_params.hot_capacity = params->hot_capacity;
    tier_params.promote_hits = params->promote_hits;
    tier_params.demote_idle_epochs = params->demote_idle_epochs;
    tier_params.cooldown_epochs = params->cooldown_epochs;
    s->policy = pgr_tier_new(&tier_params);
    if (external_slots) {
        if (external_bytes < cache_bytes) { pgr_stream_free(s); return NULL; }
        s->buf = (char *) external_slots;
    } else {
        s->buf = (char *) malloc(cache_bytes);
        s->owns_buf = 1;
    }
    s->staging = (char *)malloc(staging_bytes);
    s->sizes = (size_t *)calloc((size_t)capacity, sizeof(size_t));
    if (!s->policy || !s->buf || !s->staging || !s->sizes) { pgr_stream_free(s); return NULL; }
    return s;
}

static pgr_stream_params pgr_stream_compat_params(size_t slot_bytes, int capacity, int k) {
    pgr_stream_params params;
    memset(&params, 0, sizeof(params));
    params.slot_bytes = slot_bytes;
    params.capacity = capacity;
    params.clox_k = k;
    params.promote_hits = 3;
    params.demote_idle_epochs = 64;
    params.cooldown_epochs = 16;
    params.io_width = 1;
    return params;
}

pgr_stream *pgr_stream_new(const char *path, size_t record_bytes, int capacity, int k) {
    const pgr_stream_params params = pgr_stream_compat_params(record_bytes, capacity, k);
    if (!path) return NULL;
    pgr_stream *s = pgr_stream_alloc(&params, NULL, 0);
    if (!s) return NULL;
    s->fd = open(path, O_RDONLY);
    if (s->fd < 0) { pgr_stream_free(s); return NULL; }
    (void)fcntl(s->fd, F_NOCACHE, 1);
    return s;
}

pgr_stream *pgr_stream_new_loader(
        size_t slot_bytes, int capacity, int k,
        pgr_stream_loader loader, void * user_data) {
    const pgr_stream_params params = pgr_stream_compat_params(slot_bytes, capacity, k);
    return pgr_stream_new_loader_tier(&params, loader, user_data);
}

pgr_stream *pgr_stream_new_loader_tier(
        const pgr_stream_params * params,
        pgr_stream_loader loader, void * user_data) {
    if (!loader) return NULL;
    pgr_stream *s = pgr_stream_alloc(params, NULL, 0);
    if (!s) return NULL;
    s->loader = loader;
    s->loader_user_data = user_data;
    return s;
}

pgr_stream *pgr_stream_new_loader_external(
        const pgr_stream_params * params,
        void * slot_storage, size_t slot_storage_bytes,
        pgr_stream_loader loader, void * user_data) {
    if (!loader || !slot_storage) return NULL;
    pgr_stream *s = pgr_stream_alloc(params, slot_storage, slot_storage_bytes);
    if (!s) return NULL;
    s->loader = loader;
    s->loader_user_data = user_data;
    return s;
}

static pgr_stream_status pgr_stream_get_internal(
        pgr_stream *s, long key, off_t offset, float heat, const void **data, int *hit) {
    if (data) *data = NULL;
    if (hit) *hit = 0;
    if (!s || !data || offset < 0) {
        pgr_stream_set_error(s, "invalid stream, output, or negative offset");
        return PGR_STREAM_INVALID;
    }
    if ((uintmax_t)offset > pgr_off_max() - (uintmax_t)(s->rec - 1)) {
        pgr_stream_set_error(s, "record offset overflow");
        return PGR_STREAM_OVERFLOW;
    }

    int slot = -1;
    uint64_t generation = 0;
    const pgr_tier_class resident = pgr_tier_access(s->policy, key, &slot, &generation);
    if (resident != PGR_TIER_COLD) {
        s->hits++;
        if (resident == PGR_TIER_HOT) s->hot_hits++;
        else s->warm_hits++;
        if (hit) *hit = 1;
        *data = s->buf + (size_t)slot * s->rec;
        s->error[0] = '\0';
        return PGR_STREAM_OK;
    }

    /* Read into the one fixed staging record. Residency changes only after the
     * complete read succeeds, so EOF/errors cannot poison an existing slot. */
    size_t got = 0;
    if (s->loader) {
        pgr_stream_status status = s->loader(
            s->loader_user_data, key, s->staging, s->rec, &got,
            s->error, sizeof(s->error));
        if (status != PGR_STREAM_OK) return status;
        if (got == 0 || got > s->rec) {
            pgr_stream_set_error(s, "loader returned an invalid byte count");
            return PGR_STREAM_INVALID;
        }
    } else {
        while (got < s->rec) {
            ssize_t n = pread(s->fd, s->staging + got, s->rec - got, offset + (off_t)got);
            if (n < 0 && errno == EINTR) continue;
            if (n < 0) {
                snprintf(s->error, sizeof(s->error), "pread failed: %s", strerror(errno));
                return PGR_STREAM_IO;
            }
            if (n == 0) {
                snprintf(s->error, sizeof(s->error), "short read: got %zu of %zu bytes", got, s->rec);
                return PGR_STREAM_SHORT_READ;
            }
            got += (size_t)n;
        }
    }

    if (pgr_tier_publish(s->policy, key, heat, &slot, &generation) == PGR_TIER_COLD || slot < 0) {
        pgr_stream_set_error(s, "cache insert failed");
        return PGR_STREAM_INVALID;
    }
    char *dst = s->buf + (size_t)slot * s->rec;
    memcpy(dst, s->staging, got);
    if (got < s->rec) memset(dst + got, 0, s->rec - got);
    s->sizes[slot] = got;
    s->misses++;
    *data = dst;
    s->error[0] = '\0';
    return PGR_STREAM_OK;
}

pgr_stream_status pgr_stream_get(
        pgr_stream *s, long key, off_t offset, const void **data, int *hit) {
    return pgr_stream_get_internal(s, key, offset, 0.0f, data, hit);
}

pgr_stream_status pgr_stream_get_key(
        pgr_stream *s, long key, const void **data, size_t *data_size, int *hit) {
    return pgr_stream_get_key_heat(s, key, 0.0f, data, data_size, hit);
}

pgr_stream_status pgr_stream_get_key_heat(
        pgr_stream *s, long key, float heat,
        const void **data, size_t *data_size, int *hit) {
    if (data_size) *data_size = 0;
    if (!s || !s->loader || !data || !data_size) {
        pgr_stream_set_error(s, "invalid callback-backed stream request");
        return PGR_STREAM_INVALID;
    }
    pgr_stream_status status = pgr_stream_get_internal(s, key, 0, heat, data, hit);
    if (status != PGR_STREAM_OK) return status;
    int slot = 0;
    uint64_t generation = 0;
    if (pgr_tier_peek(s->policy, key, &slot, &generation) == PGR_TIER_COLD) {
        pgr_stream_set_error(s, "loaded key disappeared from cache");
        *data = NULL;
        return PGR_STREAM_INVALID;
    }
    *data_size = s->sizes[slot];
    return PGR_STREAM_OK;
}

void pgr_stream_batch_begin(pgr_stream *s) {
    if (s) pgr_tier_unpin_all(s->policy);
}

pgr_stream_status pgr_stream_get_key_heat_slot(
        pgr_stream *s, long key, float heat,
        const void **data, size_t *data_size, int *hit,
        int *slot, uint64_t *generation) {
    if (slot) *slot = -1;
    if (generation) *generation = 0;
    if (!slot || !generation) {
        pgr_stream_set_error(s, "slot outputs are required");
        return PGR_STREAM_INVALID;
    }
    const pgr_stream_status status = pgr_stream_get_key_heat(s, key, heat, data, data_size, hit);
    if (status != PGR_STREAM_OK) return status;
    if (pgr_tier_peek(s->policy, key, slot, generation) == PGR_TIER_COLD ||
            !pgr_tier_pin(s->policy, *slot)) {
        pgr_stream_set_error(s, "resident slot could not be pinned");
        *data = NULL;
        *data_size = 0;
        *slot = -1;
        *generation = 0;
        return PGR_STREAM_INVALID;
    }
    return PGR_STREAM_OK;
}

/* One cold read, run on a worker thread. Touches only its own staging slice and
 * its own scratch — never the shared tier policy or resident buffer. */
typedef struct {
    pgr_stream       *s;
    long              key;
    char             *dst;    /* private staging slice */
    size_t            got;
    pgr_stream_status status;
    char              err[160];
} pgr_read_task;

static void *pgr_read_worker(void *arg) {
    pgr_read_task *t = (pgr_read_task *)arg;
    t->got = 0;
    t->err[0] = '\0';
    t->status = t->s->loader(
        t->s->loader_user_data, t->key, t->dst, t->s->rec, &t->got,
        t->err, sizeof(t->err));
    if (t->status == PGR_STREAM_OK && (t->got == 0 || t->got > t->s->rec)) {
        snprintf(t->err, sizeof(t->err), "loader returned an invalid byte count");
        t->status = PGR_STREAM_INVALID;
    }
    return NULL;
}

pgr_stream_status pgr_stream_get_many(
        pgr_stream *s,
        const long *keys, const float *heats, int n,
        const void **data_out, size_t *size_out, int *hit_out) {
    if (!s || !s->loader || !keys || !data_out || n < 0) {
        pgr_stream_set_error(s, "invalid batch request");
        return PGR_STREAM_INVALID;
    }
    for (int i = 0; i < n; i++) {
        data_out[i] = NULL;
        if (size_out) size_out[i] = 0;
        if (hit_out)  hit_out[i]  = 0;
    }
    if (n == 0) { s->error[0] = '\0'; return PGR_STREAM_OK; }

    /* Phase 1 (caller thread — the tier policy is single-threaded): resolve hits now
     * and pin them so the cold publishes below can never evict a just-returned slot.
     * Gather the cold indices for parallel reads. */
    int *miss = (int *)malloc((size_t)n * sizeof(int));
    if (!miss) { pgr_stream_set_error(s, "batch scratch allocation failed"); return PGR_STREAM_INVALID; }
    int n_miss = 0;
    for (int i = 0; i < n; i++) {
        int slot = -1;
        uint64_t generation = 0;
        const pgr_tier_class cls = pgr_tier_access(s->policy, keys[i], &slot, &generation);
        if (cls != PGR_TIER_COLD) {
            if (slot < 0 || !pgr_tier_pin(s->policy, slot)) {
                pgr_stream_set_error(s, "resident slot could not be pinned");
                free(miss);
                return PGR_STREAM_INVALID;
            }
            s->hits++;
            if (cls == PGR_TIER_HOT) s->hot_hits++; else s->warm_hits++;
            data_out[i] = s->buf + (size_t)slot * s->rec;
            if (size_out) size_out[i] = s->sizes[slot];
            if (hit_out)  hit_out[i]  = 1;
        } else {
            miss[n_miss++] = i;
        }
    }

    /* Phase 2+3: read cold records in rounds of at most io_width — each into its own
     * staging slice, in parallel — then publish the round in input order (serial, so
     * the tier policy and resident buffer stay single-writer). */
    pgr_read_task *task = (pgr_read_task *)malloc((size_t)s->io_width * sizeof(pgr_read_task));
    pthread_t *th = (pthread_t *)calloc((size_t)s->io_width, sizeof(pthread_t));
    if (!task || !th) {
        free(task); free(th); free(miss);
        pgr_stream_set_error(s, "batch scratch allocation failed");
        return PGR_STREAM_INVALID;
    }

    pgr_stream_status rc = PGR_STREAM_OK;
    for (int base = 0; base < n_miss && rc == PGR_STREAM_OK; base += s->io_width) {
        int m = n_miss - base;
        if (m > s->io_width) m = s->io_width;
        for (int k = 0; k < m; k++) {
            task[k].s   = s;
            task[k].key = keys[miss[base + k]];
            task[k].dst = s->staging + (size_t)k * s->rec;
        }
        /* Launch k=1..m-1 on worker threads; run k=0 inline; then join. io_width<=1
         * or a single cold read never spawns a thread (identical to the serial path). */
        if (s->io_width > 1 && m > 1) {
            for (int k = 1; k < m; k++) {
                if (pthread_create(&th[k], NULL, pgr_read_worker, &task[k]) != 0) {
                    th[k] = 0;
                    pgr_read_worker(&task[k]);   /* fall back to inline on spawn failure */
                }
            }
        }
        pgr_read_worker(&task[0]);
        if (s->io_width > 1 && m > 1) {
            for (int k = 1; k < m; k++) if (th[k]) pthread_join(th[k], NULL);
        }
        for (int k = 0; k < m; k++) {
            const int i = miss[base + k];
            if (task[k].status != PGR_STREAM_OK) {
                pgr_stream_set_error(s, task[k].err[0] ? task[k].err : "cold read failed");
                rc = task[k].status;
                break;
            }
            const float heat = heats ? heats[i] : 0.0f;
            int slot = -1;
            uint64_t generation = 0;
            if (pgr_tier_publish(s->policy, keys[i], heat, &slot, &generation) == PGR_TIER_COLD || slot < 0) {
                pgr_stream_set_error(s, "cache insert failed");
                rc = PGR_STREAM_INVALID;
                break;
            }
            char *dstslot = s->buf + (size_t)slot * s->rec;
            memcpy(dstslot, task[k].dst, task[k].got);
            if (task[k].got < s->rec) memset(dstslot + task[k].got, 0, s->rec - task[k].got);
            s->sizes[slot] = task[k].got;
            s->misses++;
            if (!pgr_tier_pin(s->policy, slot)) {
                pgr_stream_set_error(s, "resident slot could not be pinned");
                rc = PGR_STREAM_INVALID;
                break;
            }
            data_out[i] = dstslot;
            if (size_out) size_out[i] = task[k].got;
            if (hit_out)  hit_out[i]  = 0;
        }
    }

    free(task); free(th); free(miss);
    if (rc == PGR_STREAM_OK) s->error[0] = '\0';
    return rc;
}

int pgr_stream_prefetch_many(
        pgr_stream *s, const long *keys, const float *heats, int n) {
    /* Best-effort cache warm: read the COLD keys in parallel (up to io_width) and publish
     * them WITHOUT pinning and WITHOUT returning data — used to speculatively pre-load a
     * predicted next-layer expert set during the current layer's compute. Resident keys are
     * left untouched (peek, no frequency bump). Failed reads are silently skipped so a
     * misprediction never errors or poisons anything. Returns the count of experts warmed.
     * Call on a stream that is not mid-batch (no pins held). */
    if (!s || !s->loader || !keys || n < 0) return -1;
    if (n == 0) return 0;
    int *miss = (int *)malloc((size_t)n * sizeof(int));
    if (!miss) return -1;
    int n_miss = 0;
    for (int i = 0; i < n; ++i) {
        int slot = -1;
        uint64_t generation = 0;
        if (pgr_tier_peek(s->policy, keys[i], &slot, &generation) == PGR_TIER_COLD) {
            miss[n_miss++] = i;
        }
    }
    if (n_miss == 0) { free(miss); return 0; }

    pgr_read_task *task = (pgr_read_task *)malloc((size_t)s->io_width * sizeof(pgr_read_task));
    pthread_t *th = (pthread_t *)calloc((size_t)s->io_width, sizeof(pthread_t));
    if (!task || !th) { free(task); free(th); free(miss); return -1; }

    int warmed = 0;
    for (int base = 0; base < n_miss; base += s->io_width) {
        int m = n_miss - base;
        if (m > s->io_width) m = s->io_width;
        for (int k = 0; k < m; ++k) {
            task[k].s   = s;
            task[k].key = keys[miss[base + k]];
            task[k].dst = s->staging + (size_t)k * s->rec;
        }
        if (s->io_width > 1 && m > 1) {
            for (int k = 1; k < m; ++k) {
                if (pthread_create(&th[k], NULL, pgr_read_worker, &task[k]) != 0) {
                    th[k] = 0;
                    pgr_read_worker(&task[k]);
                }
            }
        }
        pgr_read_worker(&task[0]);
        if (s->io_width > 1 && m > 1) {
            for (int k = 1; k < m; ++k) if (th[k]) pthread_join(th[k], NULL);
        }
        for (int k = 0; k < m; ++k) {
            if (task[k].status != PGR_STREAM_OK) continue;   /* best-effort: skip failures */
            const int i = miss[base + k];
            const float heat = heats ? heats[i] : 0.0f;
            int slot = -1;
            uint64_t generation = 0;
            if (pgr_tier_publish(s->policy, keys[i], heat, &slot, &generation) == PGR_TIER_COLD || slot < 0) {
                continue;   /* no evictable slot right now — skip, don't error */
            }
            char *dst = s->buf + (size_t)slot * s->rec;
            memcpy(dst, task[k].dst, task[k].got);
            if (task[k].got < s->rec) memset(dst + task[k].got, 0, s->rec - task[k].got);
            s->sizes[slot] = task[k].got;
            s->misses++;
            warmed++;
        }
    }
    free(task); free(th); free(miss);
    return warmed;
}

size_t pgr_stream_resident_bytes(const pgr_stream *s) { return s ? s->cache_bytes : 0; }
size_t pgr_stream_high_water_bytes(const pgr_stream *s) { return s ? s->high_water_bytes : 0; }
long   pgr_stream_hits(const pgr_stream *s)  { return s ? s->hits : 0; }
long   pgr_stream_misses(const pgr_stream *s){ return s ? s->misses : 0; }
long   pgr_stream_hot_hits(const pgr_stream *s) { return s ? s->hot_hits : 0; }
long   pgr_stream_warm_hits(const pgr_stream *s) { return s ? s->warm_hits : 0; }
uint64_t pgr_stream_promotions(const pgr_stream *s) { return s ? pgr_tier_promotions(s->policy) : 0; }
uint64_t pgr_stream_demotions(const pgr_stream *s) { return s ? pgr_tier_demotions(s->policy) : 0; }
size_t pgr_stream_hot_count(const pgr_stream *s) { return s ? pgr_tier_hot_count(s->policy) : 0; }
size_t pgr_stream_warm_count(const pgr_stream *s) { return s ? pgr_tier_warm_count(s->policy) : 0; }
int pgr_stream_owns_slot_storage(const pgr_stream *s) { return s ? s->owns_buf : 0; }
const char *pgr_stream_error(const pgr_stream *s) { return s ? s->error : "invalid stream"; }
void   pgr_stream_free(pgr_stream *s) {
    if (!s) return;
    if (s->policy) pgr_tier_free(s->policy);
    if (s->owns_buf) free(s->buf);
    if (s->staging) free(s->staging);
    if (s->sizes) free(s->sizes);
    if (s->fd >= 0) close(s->fd);
    free(s);
}
