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
#include <stdlib.h>
#include <string.h>

typedef struct {
    int      capacity;
    int      k;
    int      count;      /* number of resident keys */
    int      hand;       /* clock hand */
    long    *keys;       /* ring of resident expert keys (layer*100000 + expert) */
    unsigned char *ctr;  /* per-slot counter in [0,k] */
    unsigned char *pin;  /* current graph references; fixed one byte per slot */
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
    return c;
}

void pgr_clox_free(pgr_clox *c) {
    if (c) { free(c->keys); free(c->ctr); free(c->pin); free(c); }
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
    if (!pgr_clox_peek(c, key, &idx)) return 0;
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
        if (out_slot) *out_slot = slot;
        return 1;
    }
    int pinned = 0;
    for (int i = 0; i < c->capacity; ++i) pinned += c->pin[i] != 0;
    if (pinned == c->capacity) return 0;
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
    if (c) memset(c->pin, 0, (size_t)c->capacity);
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
