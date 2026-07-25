#include "peregrine_tier.h"

#include <limits.h>
#include <stdlib.h>
#include <string.h>

typedef struct pgr_clox pgr_clox;
extern pgr_clox * pgr_clox_new(int capacity, int k);
extern void       pgr_clox_free(pgr_clox * cache);
extern int        pgr_clox_lookup(pgr_clox * cache, long key, int * slot);
extern int        pgr_clox_peek(const pgr_clox * cache, long key, int * slot);
extern int        pgr_clox_insert(pgr_clox * cache, long key, int * slot);
extern int        pgr_clox_pin(pgr_clox * cache, int slot);
extern void       pgr_clox_unpin_all(pgr_clox * cache);

struct pgr_tier {
    int capacity;
    int clox_k;
    int hot_capacity;
    int count;
    int hand;
    uint8_t promote_hits;
    uint64_t demote_idle_epochs;
    uint64_t cooldown_epochs;
    uint64_t epoch;
    uint64_t promotions;
    uint64_t demotions;
    pgr_clox * compatibility;
    long * keys;
    uint64_t * generations;
    uint64_t * last_access;
    uint64_t * cooldown_until;
    uint8_t * state;
    uint8_t * score;
    uint8_t * clock;
    uint8_t * pinned;
};

static int pgr_tier_find(const pgr_tier * tier, long key) {
    int i;
    for (i = 0; i < tier->count; ++i) {
        if (tier->state[i] != PGR_TIER_COLD && tier->keys[i] == key) return i;
    }
    return -1;
}

static uint64_t pgr_tier_add_epoch(uint64_t value, uint64_t add) {
    return value > UINT64_MAX - add ? UINT64_MAX : value + add;
}

static void pgr_tier_advance(pgr_tier * tier, uint64_t epochs) {
    tier->epoch = pgr_tier_add_epoch(tier->epoch, epochs);
}

static void pgr_tier_demote_idle(pgr_tier * tier) {
    int i;
    if (!tier || tier->hot_capacity == 0) return;
    for (i = 0; i < tier->count; ++i) {
        if (tier->state[i] != PGR_TIER_HOT || tier->epoch < tier->cooldown_until[i]) continue;
        if (tier->epoch - tier->last_access[i] < tier->demote_idle_epochs) continue;
        tier->state[i] = PGR_TIER_WARM;
        tier->clock[i] = 1;
        tier->demotions++;
    }
}

static int pgr_tier_hot_count_int(const pgr_tier * tier) {
    int i;
    int count = 0;
    for (i = 0; i < tier->count; ++i) count += tier->state[i] == PGR_TIER_HOT;
    return count;
}

static int pgr_tier_coldest_hot(const pgr_tier * tier, int require_cooldown) {
    int i;
    int found = -1;
    for (i = 0; i < tier->count; ++i) {
        if (tier->state[i] != PGR_TIER_HOT || tier->pinned[i]) continue;
        if (require_cooldown && tier->epoch < tier->cooldown_until[i]) continue;
        if (found < 0 || tier->score[i] < tier->score[found] ||
                (tier->score[i] == tier->score[found] &&
                 tier->last_access[i] < tier->last_access[found])) {
            found = i;
        }
    }
    return found;
}

static void pgr_tier_promote(pgr_tier * tier, int slot) {
    int victim;
    if (tier->hot_capacity == 0 || tier->state[slot] != PGR_TIER_WARM ||
            tier->score[slot] < tier->promote_hits) return;
    if (pgr_tier_hot_count_int(tier) >= tier->hot_capacity) {
        victim = pgr_tier_coldest_hot(tier, 1);
        if (victim < 0 || tier->score[victim] >= tier->score[slot]) return;
        tier->state[victim] = PGR_TIER_WARM;
        tier->clock[victim] = 1;
        tier->demotions++;
    }
    tier->state[slot] = PGR_TIER_HOT;
    tier->cooldown_until[slot] = pgr_tier_add_epoch(tier->epoch, tier->cooldown_epochs);
    tier->promotions++;
}

static int pgr_tier_warm_victim(pgr_tier * tier) {
    int visited = 0;
    while (visited < tier->capacity * (tier->clox_k + 1)) {
        const int slot = tier->hand;
        tier->hand = (tier->hand + 1) % tier->capacity;
        visited++;
        if (slot >= tier->count || tier->state[slot] != PGR_TIER_WARM || tier->pinned[slot]) continue;
        if (tier->clock[slot] > 0) {
            tier->clock[slot]--;
            continue;
        }
        return slot;
    }
    return -1;
}

pgr_tier * pgr_tier_new(const pgr_tier_params * params) {
    pgr_tier * tier;
    size_t capacity;
    if (!params || params->capacity < 1 || params->clox_k < 1 ||
            params->clox_k > UCHAR_MAX || params->hot_capacity < 0 ||
            params->hot_capacity > params->capacity || params->promote_hits < 1 ||
            params->demote_idle_epochs < 1) return NULL;
    capacity = (size_t) params->capacity;
    tier = (pgr_tier *) calloc(1, sizeof(*tier));
    if (!tier) return NULL;
    tier->capacity = params->capacity;
    tier->clox_k = params->clox_k;
    tier->hot_capacity = params->hot_capacity;
    tier->promote_hits = params->promote_hits;
    tier->demote_idle_epochs = params->demote_idle_epochs;
    tier->cooldown_epochs = params->cooldown_epochs;
    tier->keys = (long *) malloc(sizeof(long) * capacity);
    tier->generations = (uint64_t *) calloc(capacity, sizeof(uint64_t));
    tier->last_access = (uint64_t *) calloc(capacity, sizeof(uint64_t));
    tier->cooldown_until = (uint64_t *) calloc(capacity, sizeof(uint64_t));
    tier->state = (uint8_t *) calloc(capacity, 1);
    tier->score = (uint8_t *) calloc(capacity, 1);
    tier->clock = (uint8_t *) calloc(capacity, 1);
    tier->pinned = (uint8_t *) calloc(capacity, 1);
    if (params->hot_capacity == 0) tier->compatibility = pgr_clox_new(params->capacity, params->clox_k);
    if (!tier->keys || !tier->generations || !tier->last_access ||
            !tier->cooldown_until || !tier->state || !tier->score || !tier->clock || !tier->pinned ||
            (params->hot_capacity == 0 && !tier->compatibility)) {
        pgr_tier_free(tier);
        return NULL;
    }
    return tier;
}

void pgr_tier_free(pgr_tier * tier) {
    if (!tier) return;
    pgr_clox_free(tier->compatibility);
    free(tier->keys);
    free(tier->generations);
    free(tier->last_access);
    free(tier->cooldown_until);
    free(tier->state);
    free(tier->score);
    free(tier->clock);
    free(tier->pinned);
    free(tier);
}

pgr_tier_class pgr_tier_access(
        pgr_tier * tier, long key, int * slot, uint64_t * generation) {
    int found;
    if (slot) *slot = -1;
    if (generation) *generation = 0;
    if (!tier) return PGR_TIER_COLD;
    pgr_tier_advance(tier, 1);
    pgr_tier_demote_idle(tier);
    if (tier->compatibility) {
        if (!pgr_clox_lookup(tier->compatibility, key, &found)) return PGR_TIER_COLD;
    } else {
        found = pgr_tier_find(tier, key);
        if (found < 0) return PGR_TIER_COLD;
        if (tier->score[found] < UINT8_MAX) tier->score[found]++;
        if (tier->clock[found] < tier->clox_k) tier->clock[found]++;
        tier->last_access[found] = tier->epoch;
        pgr_tier_promote(tier, found);
    }
    if (slot) *slot = found;
    if (generation) *generation = tier->generations[found];
    return (pgr_tier_class) tier->state[found];
}

pgr_tier_class pgr_tier_peek(
        const pgr_tier * tier, long key, int * slot, uint64_t * generation) {
    int found;
    if (slot) *slot = -1;
    if (generation) *generation = 0;
    if (!tier) return PGR_TIER_COLD;
    if (tier->compatibility) {
        if (!pgr_clox_peek(tier->compatibility, key, &found)) return PGR_TIER_COLD;
    } else {
        found = pgr_tier_find(tier, key);
        if (found < 0) return PGR_TIER_COLD;
    }
    if (slot) *slot = found;
    if (generation) *generation = tier->generations[found];
    return (pgr_tier_class) tier->state[found];
}

pgr_tier_class pgr_tier_publish(
        pgr_tier * tier, long key, float heat, int * slot, uint64_t * generation) {
    int found;
    int victim;
    if (slot) *slot = -1;
    if (generation) *generation = 0;
    if (!tier) return PGR_TIER_COLD;
    found = pgr_tier_find(tier, key);
    if (found >= 0) return pgr_tier_access(tier, key, slot, generation);
    if (tier->compatibility) {
        if (!pgr_clox_insert(tier->compatibility, key, &victim)) return PGR_TIER_COLD;
    } else if (tier->count < tier->capacity) {
        victim = tier->count++;
    } else {
        victim = pgr_tier_warm_victim(tier);
        if (victim < 0) {
            victim = pgr_tier_coldest_hot(tier, 0);
            if (victim < 0) return PGR_TIER_COLD;
            tier->demotions++;
        }
    }
    if (tier->compatibility && victim >= tier->count) tier->count = victim + 1;
    tier->keys[victim] = key;
    tier->state[victim] = PGR_TIER_WARM;
    if (heat < 0.0f) heat = 0.0f;
    if (heat > 1.0f) heat = 1.0f;
    tier->score[victim] = tier->hot_capacity > 0 && tier->promote_hits > 1
            ? (uint8_t) (1 + heat * (float) (tier->promote_hits - 1)) : 1;
    tier->clock[victim] = 1;
    tier->last_access[victim] = tier->epoch;
    tier->cooldown_until[victim] = 0;
    tier->generations[victim]++;
    if (tier->generations[victim] == 0) tier->generations[victim] = 1;
    if (slot) *slot = victim;
    if (generation) *generation = tier->generations[victim];
    return PGR_TIER_WARM;
}

int pgr_tier_pin(pgr_tier * tier, int slot) {
    if (!tier || slot < 0 || slot >= tier->count) return 0;
    if (tier->compatibility) return pgr_clox_pin(tier->compatibility, slot);
    tier->pinned[slot] = 1;
    return 1;
}

void pgr_tier_unpin_all(pgr_tier * tier) {
    if (!tier) return;
    if (tier->compatibility) pgr_clox_unpin_all(tier->compatibility);
    else memset(tier->pinned, 0, (size_t) tier->capacity);
}

void pgr_tier_tick(pgr_tier * tier, uint64_t epochs) {
    if (!tier || epochs == 0) return;
    pgr_tier_advance(tier, epochs);
    pgr_tier_demote_idle(tier);
}

size_t pgr_tier_resident_count(const pgr_tier * tier) { return tier ? (size_t) tier->count : 0; }
size_t pgr_tier_hot_count(const pgr_tier * tier) { return tier ? (size_t) pgr_tier_hot_count_int(tier) : 0; }
size_t pgr_tier_warm_count(const pgr_tier * tier) {
    return tier ? (size_t) tier->count - pgr_tier_hot_count(tier) : 0;
}
uint64_t pgr_tier_promotions(const pgr_tier * tier) { return tier ? tier->promotions : 0; }
uint64_t pgr_tier_demotions(const pgr_tier * tier) { return tier ? tier->demotions : 0; }
uint64_t pgr_tier_epoch(const pgr_tier * tier) { return tier ? tier->epoch : 0; }
pgr_tier_class pgr_tier_slot_class(const pgr_tier * tier, int slot) {
    if (!tier || slot < 0 || slot >= tier->count) return PGR_TIER_COLD;
    return (pgr_tier_class) tier->state[slot];
}
uint8_t pgr_tier_slot_score(const pgr_tier * tier, int slot) {
    return !tier || slot < 0 || slot >= tier->count ? 0 : tier->score[slot];
}
