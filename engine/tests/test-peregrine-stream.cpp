// test-peregrine-stream — bounded streaming expert cache, in the fork's build/test suite.
// Uses explicit checks (NOT assert, which Release/NDEBUG strips) so it truly validates.
#include "../src/peregrine_stream.h"
#include <atomic>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <unistd.h>

#define REC 4096
#define NREC 32
#define CHECK(c) do { if (!(c)) { printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

struct loader_state { int calls; };

static pgr_stream_status test_loader(
        void * user_data, long key, void * dst, size_t capacity,
        size_t * loaded, char * error, size_t error_capacity) {
    auto * state = static_cast<loader_state *>(user_data);
    state->calls++;
    if (key == 99) {
        std::snprintf(error, error_capacity, "injected read failure");
        return PGR_STREAM_IO;
    }
    std::memset(dst, (int) key, capacity);
    *loaded = capacity;
    return PGR_STREAM_OK;
}

// Thread-safe loader for the parallel batch path: the counter is touched from
// worker threads, so it must be atomic. Bytes written are keyed so the caller can
// verify each parallel read landed in the right slot.
struct par_loader_state { std::atomic<int> calls{0}; };

static pgr_stream_status par_loader(
        void * user_data, long key, void * dst, size_t capacity,
        size_t * loaded, char * error, size_t error_capacity) {
    auto * state = static_cast<par_loader_state *>(user_data);
    state->calls.fetch_add(1, std::memory_order_relaxed);
    if (key == 99) {
        std::snprintf(error, error_capacity, "injected read failure");
        return PGR_STREAM_IO;
    }
    std::memset(dst, (int) key, capacity);
    *loaded = capacity;
    return PGR_STREAM_OK;
}

int main() {
    char tmpl[] = "/tmp/pgr_fork_XXXXXX";
    int fd = mkstemp(tmpl);
    CHECK(fd >= 0);
    for (int i = 0; i < NREC; i++) { char b[REC]; memset(b, i, REC); CHECK(write(fd, b, REC) == REC); }
    close(fd);

    pgr_stream *s = pgr_stream_new(tmpl, REC, /*cap*/8, /*k*/4);
    CHECK(s != nullptr);
    CHECK(pgr_stream_resident_bytes(s) == (size_t)REC * 8);   // HARD cap
    CHECK(pgr_stream_high_water_bytes(s) == (size_t)REC * 9); // cache + one fixed staging record

    int hit;
    for (int round = 0; round < 3; round++)
        for (long key = 0; key < 6; key++) {
            off_t off = (off_t)(key % NREC) * REC;
            const void *data = nullptr;
            CHECK(pgr_stream_get(s, key, off, &data, &hit) == PGR_STREAM_OK);
            const unsigned char *p = (const unsigned char *)data;
            CHECK(p[0] == (unsigned char)(key % NREC));          // correct streamed bytes
            CHECK(p[REC - 1] == (unsigned char)(key % NREC));
        }
    CHECK(pgr_stream_misses(s) == 6);    // 6 cold loads in round 0
    CHECK(pgr_stream_hits(s) == 12);     // rounds 1,2 all hits

    // A truncated read fails closed and cannot become a false hit on retry.
    const void *bad = nullptr;
    CHECK(pgr_stream_get(s, 99, (off_t)NREC * REC - REC / 2, &bad, &hit) == PGR_STREAM_SHORT_READ);
    CHECK(bad == nullptr);
    CHECK(pgr_stream_get(s, 99, (off_t)NREC * REC - REC / 2, &bad, &hit) == PGR_STREAM_SHORT_READ);
    CHECK(pgr_stream_hits(s) == 12);
    CHECK(pgr_stream_misses(s) == 6);
    CHECK(std::strstr(pgr_stream_error(s), "short read") != nullptr);

    CHECK(pgr_stream_get(s, 100, (off_t)-1, &bad, &hit) == PGR_STREAM_INVALID);
    CHECK(pgr_stream_get(nullptr, 100, 0, &bad, &hit) == PGR_STREAM_INVALID);
    CHECK(pgr_stream_high_water_bytes(s) == (size_t)REC * 9);

    pgr_stream_free(s);

    pgr_stream_params tier_params{};
    tier_params.slot_bytes = 64;
    tier_params.capacity = 4;
    tier_params.clox_k = 4;
    tier_params.hot_capacity = 1;
    tier_params.promote_hits = 3;
    tier_params.demote_idle_epochs = 8;
    tier_params.cooldown_epochs = 4;
    loader_state loader{};
    s = pgr_stream_new_loader_tier(&tier_params, test_loader, &loader);
    CHECK(s != nullptr);
    const void * record = nullptr;
    size_t record_size = 0;
    CHECK(pgr_stream_get_key_heat(s, 1, 0.0f, &record, &record_size, &hit) == PGR_STREAM_OK);
    CHECK(hit == 0 && record_size == 64 && pgr_stream_misses(s) == 1);
    CHECK(pgr_stream_get_key_heat(s, 1, 0.0f, &record, &record_size, &hit) == PGR_STREAM_OK);
    CHECK(hit == 1 && pgr_stream_warm_hits(s) == 1);
    CHECK(pgr_stream_get_key_heat(s, 1, 0.0f, &record, &record_size, &hit) == PGR_STREAM_OK);
    CHECK(hit == 1 && pgr_stream_hot_hits(s) == 1 && pgr_stream_promotions(s) == 1);
    const long misses_before_error = pgr_stream_misses(s);
    const size_t hot_before_error = pgr_stream_hot_count(s);
    const size_t warm_before_error = pgr_stream_warm_count(s);
    CHECK(pgr_stream_get_key_heat(s, 99, 1.0f, &record, &record_size, &hit) == PGR_STREAM_IO);
    CHECK(record == nullptr && pgr_stream_misses(s) == misses_before_error);
    CHECK(pgr_stream_hot_count(s) == hot_before_error && pgr_stream_warm_count(s) == warm_before_error);
    CHECK(pgr_stream_get_key_heat(s, 99, 1.0f, &record, &record_size, &hit) == PGR_STREAM_IO);
    CHECK(loader.calls == 3 && pgr_stream_high_water_bytes(s) == 5 * 64);
    CHECK(pgr_stream_hot_count(s) + pgr_stream_warm_count(s) <= 4);
    pgr_stream_free(s);

    tier_params.capacity = 2;
    tier_params.hot_capacity = 0;
    loader = {};
    s = pgr_stream_new_loader_tier(&tier_params, test_loader, &loader);
    CHECK(s != nullptr);
    int slot = -1;
    uint64_t generation = 0;
    pgr_stream_batch_begin(s);
    CHECK(pgr_stream_get_key_heat_slot(s, 1, 0.0f, &record, &record_size, &hit,
            &slot, &generation) == PGR_STREAM_OK);
    CHECK(slot == 0 && generation != 0);
    CHECK(pgr_stream_get_key_heat_slot(s, 2, 0.0f, &record, &record_size, &hit,
            &slot, &generation) == PGR_STREAM_OK);
    CHECK(slot == 1 && generation != 0);
    CHECK(pgr_stream_get_key_heat_slot(s, 3, 0.0f, &record, &record_size, &hit,
            &slot, &generation) == PGR_STREAM_INVALID);
    CHECK(record == nullptr && slot == -1);
    pgr_stream_batch_begin(s);
    CHECK(pgr_stream_get_key_heat_slot(s, 3, 0.0f, &record, &record_size, &hit,
            &slot, &generation) == PGR_STREAM_OK);
    pgr_stream_free(s);

    CHECK(pgr_stream_new(tmpl, std::numeric_limits<size_t>::max(), 2, 4) == nullptr);

    // --- parallel batch fetch (io_width > 1): identical residency to the serial path ---
    {
        pgr_stream_params p{};
        p.slot_bytes = 64;
        p.capacity = 8;
        p.clox_k = 4;
        p.hot_capacity = 0;
        p.promote_hits = 3;
        p.demote_idle_epochs = 8;
        p.cooldown_epochs = 4;
        p.io_width = 4;                       // four parallel staging slices / reader threads
        par_loader_state pl;
        pgr_stream * ps = pgr_stream_new_loader_tier(&p, par_loader, &pl);
        CHECK(ps != nullptr);
        // high-water grows by exactly (io_width) staging records over the cache bytes
        CHECK(pgr_stream_high_water_bytes(ps) == (size_t) 64 * (8 + 4));

        long keys[6] = {10, 11, 12, 13, 14, 15};
        const void * d[6];
        size_t sz[6];
        int h[6];

        // First batch: six distinct cold experts, read in parallel across two rounds.
        pgr_stream_batch_begin(ps);
        CHECK(pgr_stream_get_many(ps, keys, nullptr, 6, d, sz, h) == PGR_STREAM_OK);
        for (int i = 0; i < 6; i++) {
            CHECK(h[i] == 0 && sz[i] == 64 && d[i] != nullptr);
            const unsigned char * b = (const unsigned char *) d[i];
            CHECK(b[0] == (unsigned char) keys[i] && b[63] == (unsigned char) keys[i]); // right bytes in right slot
        }
        CHECK(pgr_stream_misses(ps) == 6 && pl.calls.load() == 6);

        // Second identical batch: all resident, zero new loader calls.
        pgr_stream_batch_begin(ps);
        CHECK(pgr_stream_get_many(ps, keys, nullptr, 6, d, sz, h) == PGR_STREAM_OK);
        for (int i = 0; i < 6; i++) CHECK(h[i] == 1 && d[i] != nullptr);
        CHECK(pl.calls.load() == 6 && pgr_stream_misses(ps) == 6);

        // A failing read inside a batch fails closed: keys before it stay resident,
        // keys at/after it are not published, and no slot is poisoned.
        long bad_keys[3] = {20, 99, 21};
        const void * bd[3];
        size_t bsz[3];
        int bh[3];
        const long misses_before = pgr_stream_misses(ps);
        pgr_stream_batch_begin(ps);
        CHECK(pgr_stream_get_many(ps, bad_keys, nullptr, 3, bd, bsz, bh) == PGR_STREAM_IO);
        CHECK(std::strstr(pgr_stream_error(ps), "injected") != nullptr);
        CHECK(pgr_stream_misses(ps) == misses_before + 1); // only key 20 published
        CHECK(bd[1] == nullptr && bd[2] == nullptr);
        pgr_stream_free(ps);
    }

    // --- speculative prefetch: warm cold keys (no pin), then they are hits ---
    {
        pgr_stream_params p{};
        p.slot_bytes = 64; p.capacity = 8; p.clox_k = 4; p.io_width = 4;
        p.promote_hits = 3; p.demote_idle_epochs = 8; p.cooldown_epochs = 4;
        par_loader_state pl;
        pgr_stream * ps = pgr_stream_new_loader_tier(&p, par_loader, &pl);
        CHECK(ps != nullptr);
        long keys[4] = {30, 31, 32, 33};
        // prefetch 4 cold experts -> all warmed, counted as (cold) misses, loader called 4x
        CHECK(pgr_stream_prefetch_many(ps, keys, nullptr, 4) == 4);
        CHECK(pgr_stream_misses(ps) == 4 && pl.calls.load() == 4);
        // now they are resident: a batch fetch is all hits, no new loader calls
        const void * d[4]; size_t sz[4]; int h[4];
        pgr_stream_batch_begin(ps);
        CHECK(pgr_stream_get_many(ps, keys, nullptr, 4, d, sz, h) == PGR_STREAM_OK);
        for (int i = 0; i < 4; i++) {
            CHECK(h[i] == 1);
            CHECK(((const unsigned char *) d[i])[0] == (unsigned char) keys[i]);
        }
        CHECK(pl.calls.load() == 4 && pgr_stream_misses(ps) == 4);
        // prefetching already-resident keys is a no-op (0 warmed, no extra reads)
        CHECK(pgr_stream_prefetch_many(ps, keys, nullptr, 4) == 0);
        CHECK(pl.calls.load() == 4);
        // a failing key (99) is skipped; the good ones still warm
        long mixed[2] = {40, 99};
        CHECK(pgr_stream_prefetch_many(ps, mixed, nullptr, 2) == 1);  // only key 40 warms
        pgr_stream_free(ps);
    }

    unlink(tmpl);
    printf("PGR_STREAM_FORK_OK\n");
    return 0;
}
