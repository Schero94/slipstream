/* peregrine_cache.c — CLOCK-LRU-K expert eviction for the llama.cpp fork.
 *
 * Native port of the measured cloxcache policy (adopted from colibri PR #223,
 * validated in bench/m0d/cloxcache.py): each resident expert slot holds a counter
 * in [0,K]; a hit bumps it (frequency memory), eviction sweeps a clock hand
 * decrementing counters (the decay = recency pressure) and evicts the first slot
 * to reach 0. O(1) amortized. This is the eviction brain a bounded, streaming
 * expert cache in ggml-metal will call — the runtime foundation, not a reference.
 *
 * Standalone C (no llama.cpp headers yet) so it compiles + self-tests in isolation
 * before being wired into the expert-residency path. */
#include <math.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <strings.h>
#include <stdlib.h>
#include <string.h>

/* Opt-in eviction experiment (PGRN_EVICT, default off = CLOCK-LRU-K unchanged):
 *   lp           CLOCK-LRU-K + lookahead-protect: a lookahead kick may not evict the
 *                experts it predicts (resident ones or ones it just loaded); applied in
 *                peregrine_stream/runtime via pgr_evict_protect().
 *   decay[:H]    evict the lowest exponentially decayed demand count (arXiv 2611.15842
 *                App. C; Colibri qwen36_expected_policy.h), half-life H ubatches
 *                (default 16), per-expert history kept across evictions, ties -> least
 *                recently used; implies lookahead-protect. Only the HOT-less cache
 *                (--pgrn-hot-percent 0, the default) uses this file's victim choice;
 *                with a HOT tier decay is inert (the runtime warns).
 * Lossless: only the choice of victim changes, never which experts are computed. */
enum { PGR_EVICT_CLOX = 0, PGR_EVICT_LP = 1, PGR_EVICT_DECAY = 2 };
#define PGR_DECAY_IDS 1024   /* expert ids tracked for decayed history (>= n_expert) */
#define PGR_DECAY_HALF_DEFAULT 16.0
#define PGR_DECAY_HALF_MIN 1e-3  /* below this exp() underflows every count to 0 at once */
static int    pgr_evict_mode_v = PGR_EVICT_CLOX;
static double pgr_evict_lambda = 0.0;
static pthread_once_t pgr_evict_once = PTHREAD_ONCE_INIT;

/* Case-insensitive exact parse of a PGRN_EVICT value. Accepted: unset/"" / "off" / "clox"
 * (default CLOCK-LRU-K), "lp", "decay", "decay:<H>" (H half-life in ubatches, finite,
 * > 0; values below 1e-3 are clamped to 1e-3). Returns 0 when the value was accepted as
 * written, 1 when it was accepted with a correction (H clamped, or H rejected -> default 16),
 * -1 for an unknown value (mode = clox). */
int pgr_evict_parse(const char *e, int *mode, double *half);
int pgr_evict_parse(const char *e, int *mode, double *half) {
    *mode = PGR_EVICT_CLOX;
    *half = PGR_DECAY_HALF_DEFAULT;
    if (!e || !e[0] || strcasecmp(e, "off") == 0 || strcasecmp(e, "clox") == 0) return 0;
    if (strcasecmp(e, "lp") == 0) { *mode = PGR_EVICT_LP; return 0; }
    if (strcasecmp(e, "decay") == 0) { *mode = PGR_EVICT_DECAY; return 0; }
    if (strncasecmp(e, "decay:", 6) == 0) {
        *mode = PGR_EVICT_DECAY;
        char *end = NULL;
        const double h = strtod(e + 6, &end);
        if (end == e + 6 || *end != '\0' || !isfinite(h) || h <= 0.0) return 1;   /* default H */
        if (h < PGR_DECAY_HALF_MIN) { *half = PGR_DECAY_HALF_MIN; return 1; }
        *half = h;
        return 0;
    }
    return -1;
}

static void pgr_evict_init(void) {
    const char *e = getenv("PGRN_EVICT");
    int mode = PGR_EVICT_CLOX;
    double half = PGR_DECAY_HALF_DEFAULT;
    const int rc = pgr_evict_parse(e, &mode, &half);
    if (rc < 0) {
        fprintf(stderr, "[peregrine] WARN: unknown PGRN_EVICT='%s' (expected off|lp|decay|decay:<H>) - using the default CLOCK-LRU-K\n", e);
    } else if (rc > 0) {
        fprintf(stderr, "[peregrine] WARN: PGRN_EVICT='%s': half-life must be a finite number > 0 (min %g) - using H=%g\n",
                e, PGR_DECAY_HALF_MIN, half);
    }
    pgr_evict_lambda = log(2.0) / half;
    pgr_evict_mode_v = mode;
}

static int pgr_evict_mode(void) {
    pthread_once(&pgr_evict_once, pgr_evict_init);
    return pgr_evict_mode_v;
}

typedef struct {
    int      capacity;
    int      k;
    int      count;      /* number of resident keys */
    int      hand;       /* clock hand */
    long    *keys;       /* ring of resident expert keys (layer*100000 + expert) */
    unsigned char *ctr;  /* per-slot counter in [0,k] */
    unsigned char *pin;  /* current graph references; fixed one byte per slot */
    /* PGRN_EVICT=decay only (NULL otherwise) */
    int       mode;
    uint64_t  epoch;     /* demand ubatches seen (ticks at the first lookup after unpin_all) */
    int       tick;      /* unpin_all happened since the last tick */
    uint64_t *last;      /* per-slot last use epoch (LRU tie break) */
    double   *hval;      /* per-expert decayed demand count ... */
    uint64_t *hepoch;    /* ... and the epoch it was last updated */
} pgr_clox;

/* prototypes (silence -Wmissing-prototypes; these are the module's public API) */
pgr_clox *pgr_clox_new(int capacity, int k);
void      pgr_clox_free(pgr_clox *c);
int       pgr_clox_access2(pgr_clox *c, long key, int *out_slot);
int       pgr_clox_access(pgr_clox *c, long key);
int       pgr_clox_lookup(pgr_clox *c, long key, int *out_slot);
int       pgr_clox_peek(const pgr_clox *c, long key, int *out_slot);
int       pgr_clox_insert(pgr_clox *c, long key, int *out_slot);
int       pgr_clox_pin(pgr_clox *c, int slot);
void      pgr_clox_unpin_all(pgr_clox *c);
int       pgr_evict_protect(void);
int       pgr_evict_is_decay(void);

int pgr_evict_protect(void) { return pgr_evict_mode() != PGR_EVICT_CLOX; }
int pgr_evict_is_decay(void) { return pgr_evict_mode() == PGR_EVICT_DECAY; }

static double pgr_decay_val(const pgr_clox *c, long key) {
    const long id = key & 0xFFFF;
    if (id >= PGR_DECAY_IDS || c->hval[id] == 0.0) return 0.0;
    return c->hval[id] * exp(-pgr_evict_lambda * (double)(c->epoch - c->hepoch[id]));
}

static void pgr_decay_bump(pgr_clox *c, long key) {
    const long id = key & 0xFFFF;
    if (c->tick) { c->epoch++; c->tick = 0; }
    if (id >= PGR_DECAY_IDS) return;
    c->hval[id] = pgr_decay_val(c, key) + 1.0;
    c->hepoch[id] = c->epoch;
}

pgr_clox *pgr_clox_new(int capacity, int k) {
    if (capacity < 1 || k < 1) return NULL;
    pgr_clox *c = (pgr_clox *)calloc(1, sizeof(pgr_clox));
    if (!c) return NULL;
    c->capacity = capacity; c->k = k; c->count = 0; c->hand = 0;
    c->keys = (long *)malloc(sizeof(long) * capacity);
    c->ctr  = (unsigned char *)calloc(capacity, 1);
    c->pin  = (unsigned char *)calloc(capacity, 1);
    if (!c->keys || !c->ctr || !c->pin) {
        free(c->keys); free(c->ctr); free(c->pin); free(c); return NULL;
    }
    c->mode = pgr_evict_mode();
    if (c->mode == PGR_EVICT_DECAY) {
        c->last   = (uint64_t *)calloc(capacity, sizeof(uint64_t));
        c->hval   = (double *)calloc(PGR_DECAY_IDS, sizeof(double));
        c->hepoch = (uint64_t *)calloc(PGR_DECAY_IDS, sizeof(uint64_t));
        if (!c->last || !c->hval || !c->hepoch) {
            free(c->last); free(c->hval); free(c->hepoch);
            free(c->keys); free(c->ctr); free(c->pin); free(c); return NULL;
        }
    }
    return c;
}

void pgr_clox_free(pgr_clox *c) {
    if (c) {
        free(c->last); free(c->hval); free(c->hepoch);
        free(c->keys); free(c->ctr); free(c->pin); free(c);
    }
}

static int pgr_clox_find(const pgr_clox *c, long key) {
    for (int i = 0; i < c->count; i++) if (c->keys[i] == key) return i;
    return -1;
}

int pgr_clox_peek(const pgr_clox *c, long key, int *out_slot) {
    if (!c) return 0;
    int idx = pgr_clox_find(c, key);
    if (idx < 0) return 0;
    if (out_slot) *out_slot = idx;
    return 1;
}

/* Probe without changing residency on a miss. Hits retain the original
 * CLOCK-LRU-K frequency update. This lets I/O complete before a miss is
 * published, so a short read can never create a false cache hit. */
int pgr_clox_lookup(pgr_clox *c, long key, int *out_slot) {
    if (!c) return 0;
    int idx = -1;
    /* lookup is the demand path only (prefetch uses peek), so a miss here is a demand
     * request too: count it in the decayed history before the insert that follows. */
    if (c->mode == PGR_EVICT_DECAY) pgr_decay_bump(c, key);
    if (!pgr_clox_peek(c, key, &idx)) return 0;
    if (c->mode == PGR_EVICT_DECAY) c->last[idx] = c->epoch;
    if (c->ctr[idx] < c->k) c->ctr[idx]++;
    if (out_slot) *out_slot = idx;
    return 1;
}

/* Publish a fully loaded missing key and return its resident slot. */
int pgr_clox_insert(pgr_clox *c, long key, int *out_slot) {
    if (!c) return 0;
    if (c->count < c->capacity) {
        int slot = c->count;
        c->keys[slot] = key; c->ctr[slot] = 1; c->count++;
        if (c->mode == PGR_EVICT_DECAY) c->last[slot] = c->epoch;
        if (out_slot) *out_slot = slot;
        return 1;
    }
    int pinned = 0;
    for (int i = 0; i < c->capacity; ++i) pinned += c->pin[i] != 0;
    if (pinned == c->capacity) return 0;
    if (c->mode == PGR_EVICT_DECAY) {
        int victim = -1;
        double vv = 0.0;
        for (int i = 0; i < c->capacity; ++i) {
            if (c->pin[i]) continue;
            const double v = pgr_decay_val(c, c->keys[i]);
            if (victim < 0 || v < vv || (v == vv && c->last[i] < c->last[victim])) { victim = i; vv = v; }
        }
        c->keys[victim] = key; c->ctr[victim] = 1; c->last[victim] = c->epoch;
        if (out_slot) *out_slot = victim;
        return 1;
    }
    for (;;) {
        if (c->pin[c->hand]) {
            c->hand = (c->hand + 1) % c->capacity;
            continue;
        }
        if (c->ctr[c->hand] > 0) {
            c->ctr[c->hand]--;
            c->hand = (c->hand + 1) % c->capacity;
        } else {
            int slot = c->hand;
            c->keys[slot] = key; c->ctr[slot] = 1;
            c->hand = (c->hand + 1) % c->capacity;
            if (out_slot) *out_slot = slot;
            return 1;
        }
    }
}

int pgr_clox_pin(pgr_clox *c, int slot) {
    if (!c || slot < 0 || slot >= c->count) return 0;
    c->pin[slot] = 1;
    return 1;
}

void pgr_clox_unpin_all(pgr_clox *c) {
    if (c) { memset(c->pin, 0, (size_t)c->capacity); c->tick = 1; }
}

/* access one expert key; also report the ring slot now holding it (== buffer slot).
 * returns 1 on hit (already resident), 0 on miss (loaded now). */
int pgr_clox_access2(pgr_clox *c, long key, int *out_slot) {
    if (pgr_clox_lookup(c, key, out_slot)) return 1;
    return pgr_clox_insert(c, key, out_slot) ? 0 : -1;
}

/* access one expert key. returns 1 on hit (already resident), 0 on miss (loaded now). */
int pgr_clox_access(pgr_clox *c, long key) {
    return pgr_clox_access2(c, key, NULL);
}
