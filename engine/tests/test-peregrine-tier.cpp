#include "peregrine_tier.h"

#include <climits>
#include <cstdint>
#include <cstdlib>
#include <cstdio>
#include <fstream>
#include <memory>
#include <vector>

typedef struct pgr_clox pgr_clox;
extern "C" pgr_clox * pgr_clox_new(int capacity, int k);
extern "C" void pgr_clox_free(pgr_clox * cache);
extern "C" int pgr_clox_lookup(pgr_clox * cache, long key, int * slot);
extern "C" int pgr_clox_insert(pgr_clox * cache, long key, int * slot);

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

static pgr_tier_params params(int hot_capacity) {
    pgr_tier_params result{};
    result.capacity = 8;
    result.clox_k = 4;
    result.hot_capacity = hot_capacity;
    result.promote_hits = 3;
    result.demote_idle_epochs = 8;
    result.cooldown_epochs = 4;
    return result;
}

static int check_clox_compatibility() {
    const std::vector<long> trace = { 1,2,3,4,1,2,5,6,1,7,8,9,2,3,10,1,11,2,12,1 };
    auto p = params(0);
    p.capacity = 5;
    pgr_tier * tier = pgr_tier_new(&p);
    pgr_clox * clox = pgr_clox_new(p.capacity, p.clox_k);
    CHECK(tier != nullptr && clox != nullptr);
    for (long key : trace) {
        int tier_slot = -1;
        int clox_slot = -1;
        uint64_t generation = 0;
        const bool tier_hit = pgr_tier_access(tier, key, &tier_slot, &generation) != PGR_TIER_COLD;
        const bool clox_hit = pgr_clox_lookup(clox, key, &clox_slot) != 0;
        CHECK(tier_hit == clox_hit);
        if (!tier_hit) {
            CHECK(pgr_tier_publish(tier, key, 0.0f, &tier_slot, &generation) == PGR_TIER_WARM);
            CHECK(pgr_clox_insert(clox, key, &clox_slot) != 0);
        }
        CHECK(tier_slot == clox_slot);
        CHECK(generation != 0);
        CHECK(pgr_tier_hot_count(tier) == 0);
        CHECK(pgr_tier_resident_count(tier) <= (size_t) p.capacity);
    }
    pgr_clox_free(clox);
    pgr_tier_free(tier);
    return 0;
}

static int replay_main(int argc, char ** argv) {
    if (argc != 6) {
        std::fprintf(stderr, "usage: %s --replay PAIRS LAYERS TOTAL_CAPACITY HOT_PERCENT\n", argv[0]);
        return 2;
    }
    const int layers = std::atoi(argv[3]);
    const int total_capacity = std::atoi(argv[4]);
    const int hot_percent = std::atoi(argv[5]);
    if (layers < 1 || total_capacity < layers || hot_percent < 0 || hot_percent > 100) return 2;
    std::ifstream input(argv[2], std::ios::binary);
    if (!input) return 2;
    std::vector<pgr_tier *> tiers;
    tiers.reserve((size_t) layers);
    const int per_layer = total_capacity / layers;
    const int remainder = total_capacity % layers;
    for (int layer = 0; layer < layers; ++layer) {
        auto p = params(0);
        p.capacity = per_layer + (layer < remainder ? 1 : 0);
        p.hot_capacity = p.capacity * hot_percent / 100;
        if (hot_percent > 0 && p.capacity > 1 && p.hot_capacity == 0) p.hot_capacity = 1;
        if (p.capacity > 1 && p.hot_capacity >= p.capacity) p.hot_capacity = p.capacity - 1;
        if (p.capacity == 1) p.hot_capacity = 0;
        pgr_tier * tier = pgr_tier_new(&p);
        if (!tier) {
            for (pgr_tier * prior : tiers) pgr_tier_free(prior);
            return 2;
        }
        tiers.push_back(tier);
    }
    uint64_t accesses = 0, hot_hits = 0, warm_hits = 0, misses = 0;
    for (;;) {
        uint16_t pair[2] = {};
        input.read(reinterpret_cast<char *>(pair), sizeof(pair));
        if (input.gcount() == 0) break;
        if (input.gcount() != sizeof(pair) || pair[0] >= tiers.size()) {
            for (pgr_tier * tier : tiers) pgr_tier_free(tier);
            return 2;
        }
        int slot = -1;
        uint64_t generation = 0;
        const pgr_tier_class result = pgr_tier_access(tiers[pair[0]], pair[1], &slot, &generation);
        accesses++;
        if (result == PGR_TIER_HOT) hot_hits++;
        else if (result == PGR_TIER_WARM) warm_hits++;
        else {
            misses++;
            if (pgr_tier_publish(tiers[pair[0]], pair[1], 0.0f, &slot, &generation) != PGR_TIER_WARM) {
                for (pgr_tier * tier : tiers) pgr_tier_free(tier);
                return 2;
            }
        }
    }
    uint64_t promotions = 0, demotions = 0, hot_slots = 0, warm_slots = 0;
    for (pgr_tier * tier : tiers) {
        promotions += pgr_tier_promotions(tier);
        demotions += pgr_tier_demotions(tier);
        hot_slots += pgr_tier_hot_count(tier);
        warm_slots += pgr_tier_warm_count(tier);
        pgr_tier_free(tier);
    }
    std::printf("{\"accesses\":%llu,\"hot_hits\":%llu,\"warm_hits\":%llu,"
                "\"misses\":%llu,\"promotions\":%llu,\"demotions\":%llu,"
                "\"hot_slots\":%llu,\"warm_slots\":%llu}\n",
            (unsigned long long) accesses, (unsigned long long) hot_hits,
            (unsigned long long) warm_hits, (unsigned long long) misses,
            (unsigned long long) promotions, (unsigned long long) demotions,
            (unsigned long long) hot_slots, (unsigned long long) warm_slots);
    return input.bad() ? 2 : 0;
}

int main(int argc, char ** argv) {
    if (argc > 1) return replay_main(argc, argv);
    CHECK(pgr_tier_new(nullptr) == nullptr);
    auto bad = params(2);
    bad.hot_capacity = 9;
    CHECK(pgr_tier_new(&bad) == nullptr);

    auto p = params(2);
    pgr_tier * tier = pgr_tier_new(&p);
    CHECK(tier != nullptr);
    int slot = -1;
    uint64_t generation = 0;
    CHECK(pgr_tier_access(tier, 11, &slot, &generation) == PGR_TIER_COLD);
    CHECK(slot == -1 && generation == 0 && pgr_tier_resident_count(tier) == 0);
    CHECK(pgr_tier_publish(tier, 11, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    const int original_slot = slot;
    const uint64_t original_generation = generation;
    CHECK(pgr_tier_access(tier, 11, &slot, &generation) == PGR_TIER_WARM);
    CHECK(pgr_tier_access(tier, 11, &slot, &generation) == PGR_TIER_HOT);
    CHECK(slot == original_slot && generation == original_generation);
    CHECK(pgr_tier_hot_count(tier) == 1 && pgr_tier_promotions(tier) == 1);

    CHECK(pgr_tier_publish(tier, 22, 1.0f, &slot, &generation) == PGR_TIER_WARM);
    CHECK(pgr_tier_access(tier, 22, &slot, &generation) == PGR_TIER_HOT);
    CHECK(pgr_tier_hot_count(tier) == 2);
    CHECK(pgr_tier_publish(tier, 33, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    CHECK(pgr_tier_access(tier, 33, &slot, &generation) == PGR_TIER_WARM);
    CHECK(pgr_tier_access(tier, 33, &slot, &generation) == PGR_TIER_WARM);
    CHECK(pgr_tier_hot_count(tier) == 2);

    pgr_tier_tick(tier, 16);
    CHECK(pgr_tier_hot_count(tier) == 0);
    CHECK(pgr_tier_demotions(tier) >= 2);

    for (int i = 0; i < 1000; ++i) {
        CHECK(pgr_tier_access(tier, 11, &slot, &generation) != PGR_TIER_COLD);
    }
    CHECK(pgr_tier_slot_score(tier, slot) == UCHAR_MAX);
    for (long key = 100; key < 140; ++key) {
        if (pgr_tier_access(tier, key, &slot, &generation) == PGR_TIER_COLD) {
            CHECK(pgr_tier_publish(tier, key, 0.0f, &slot, &generation) == PGR_TIER_WARM);
        }
        CHECK(pgr_tier_resident_count(tier) <= 8);
        CHECK(pgr_tier_hot_count(tier) <= 2);
        CHECK(pgr_tier_hot_count(tier) + pgr_tier_warm_count(tier) == pgr_tier_resident_count(tier));
    }
    pgr_tier_free(tier);

    p = params(1);
    p.capacity = 2;
    tier = pgr_tier_new(&p);
    CHECK(tier != nullptr);
    CHECK(pgr_tier_publish(tier, 1, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    CHECK(slot == 0 && generation == 1);
    CHECK(pgr_tier_publish(tier, 2, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    CHECK(slot == 1 && generation == 1);
    CHECK(pgr_tier_publish(tier, 3, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    CHECK(slot == 0 && generation == 2);
    CHECK(pgr_tier_access(tier, 1, &slot, &generation) == PGR_TIER_COLD);
    pgr_tier_free(tier);

    CHECK(check_clox_compatibility() == 0);

    p = params(0);
    p.capacity = 2;
    tier = pgr_tier_new(&p);
    CHECK(tier != nullptr);
    CHECK(pgr_tier_publish(tier, 1, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    CHECK(pgr_tier_pin(tier, slot) == 1);
    CHECK(pgr_tier_publish(tier, 2, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    CHECK(pgr_tier_pin(tier, slot) == 1);
    CHECK(pgr_tier_publish(tier, 3, 0.0f, &slot, &generation) == PGR_TIER_COLD);
    CHECK(pgr_tier_access(tier, 1, &slot, &generation) == PGR_TIER_WARM);
    pgr_tier_unpin_all(tier);
    CHECK(pgr_tier_publish(tier, 3, 0.0f, &slot, &generation) == PGR_TIER_WARM);
    pgr_tier_free(tier);

    std::printf("PGR_TIER_OK\n");
    return 0;
}
