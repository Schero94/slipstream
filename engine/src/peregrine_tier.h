/* Fixed-allocation HOT/WARM policy for PGRN expert slots. */
#ifndef PEREGRINE_TIER_H
#define PEREGRINE_TIER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pgr_tier pgr_tier;

typedef enum {
    PGR_TIER_COLD = 0,
    PGR_TIER_WARM = 1,
    PGR_TIER_HOT  = 2,
} pgr_tier_class;

typedef struct {
    int      capacity;
    int      clox_k;
    int      hot_capacity;
    uint8_t  promote_hits;
    uint64_t demote_idle_epochs;
    uint64_t cooldown_epochs;
} pgr_tier_params;

pgr_tier * pgr_tier_new(const pgr_tier_params * params);
void       pgr_tier_free(pgr_tier * tier);

/* Probe a key and update its bounded online score. COLD never changes residency. */
pgr_tier_class pgr_tier_access(
        pgr_tier * tier, long key, int * slot, uint64_t * generation);

/* Observe residency without updating score, clock state, or policy time. */
pgr_tier_class pgr_tier_peek(
        const pgr_tier * tier, long key, int * slot, uint64_t * generation);

/* Publish bytes that have already been read and validated. */
pgr_tier_class pgr_tier_publish(
        pgr_tier * tier, long key, float heat, int * slot, uint64_t * generation);

/* Protect slots referenced by the current graph from eviction. */
int  pgr_tier_pin(pgr_tier * tier, int slot);
void pgr_tier_unpin_all(pgr_tier * tier);

/* Advance policy time without allocating history. */
void pgr_tier_tick(pgr_tier * tier, uint64_t epochs);

size_t   pgr_tier_resident_count(const pgr_tier * tier);
size_t   pgr_tier_hot_count(const pgr_tier * tier);
size_t   pgr_tier_warm_count(const pgr_tier * tier);
uint64_t pgr_tier_promotions(const pgr_tier * tier);
uint64_t pgr_tier_demotions(const pgr_tier * tier);
uint64_t pgr_tier_epoch(const pgr_tier * tier);
pgr_tier_class pgr_tier_slot_class(const pgr_tier * tier, int slot);
uint8_t  pgr_tier_slot_score(const pgr_tier * tier, int slot);

#ifdef __cplusplus
}
#endif
#endif
