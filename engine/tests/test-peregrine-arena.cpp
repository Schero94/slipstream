#include "peregrine_arena.h"
#include "../src/peregrine_stream.h"

#include <cstdio>
#include <cstring>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

static pgr_stream_status loader(
        void *, long key, void * dst, size_t capacity,
        size_t * loaded, char *, size_t) {
    std::memset(dst, (int) key, capacity);
    *loaded = capacity;
    return PGR_STREAM_OK;
}

int main() {
    char error[192] = {};
    constexpr size_t SLOT = 64;
    constexpr int CAPACITY = 4;
    pgr_arena * arena = pgr_arena_new(
            ggml_backend_cpu_buffer_type(), SLOT * CAPACITY, error, sizeof(error));
    CHECK(arena != nullptr);
    CHECK(pgr_arena_bytes(arena) >= SLOT * CAPACITY);
    void * slots = nullptr;
    CHECK(pgr_arena_slice(arena, 0, SLOT * CAPACITY, &slots, error, sizeof(error)) == 0);
    CHECK(slots == pgr_arena_base(arena));
    void * invalid = nullptr;
    CHECK(pgr_arena_slice(arena, SLOT * CAPACITY - 1, 2, &invalid, error, sizeof(error)) == -1);
    CHECK(invalid == nullptr && std::strstr(error, "outside") != nullptr);

    pgr_stream_params params{};
    params.slot_bytes = SLOT;
    params.capacity = CAPACITY;
    params.clox_k = 4;
    params.promote_hits = 3;
    params.demote_idle_epochs = 64;
    params.cooldown_epochs = 16;
    pgr_stream * stream = pgr_stream_new_loader_external(
            &params, slots, SLOT * CAPACITY, loader, nullptr);
    CHECK(stream != nullptr);
    CHECK(pgr_stream_owns_slot_storage(stream) == 0);
    CHECK(pgr_stream_resident_bytes(stream) == SLOT * CAPACITY);
    CHECK(pgr_stream_high_water_bytes(stream) == SLOT * (CAPACITY + 1));
    const void * data = nullptr;
    size_t size = 0;
    int hit = -1;
    CHECK(pgr_stream_get_key(stream, 7, &data, &size, &hit) == PGR_STREAM_OK);
    CHECK(hit == 0 && size == SLOT && data == slots);
    CHECK(static_cast<const unsigned char *>(data)[SLOT - 1] == 7);
    CHECK(pgr_stream_get_key(stream, 7, &data, &size, &hit) == PGR_STREAM_OK && hit == 1);
    pgr_stream_free(stream);
    CHECK(static_cast<unsigned char *>(slots)[0] == 7);
    CHECK(pgr_stream_new_loader_external(&params, slots, SLOT - 1, loader, nullptr) == nullptr);
    pgr_arena_free(arena);

    CHECK(pgr_arena_new(nullptr, SLOT, error, sizeof(error)) == nullptr);
    std::printf("PGR_ARENA_OK\n");
    return 0;
}
