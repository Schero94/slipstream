#include "peregrine_system_memory.h"

#include <cstdio>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

int main() {
    pgr_system_memory memory{};
#if defined(__APPLE__) || defined(__linux__)
    CHECK(pgr_system_memory_read(&memory) == 0);
    CHECK(memory.total_bytes > 0);
    CHECK(memory.available_known == 1);
    CHECK(memory.available_bytes > 0);
    CHECK(memory.available_bytes <= memory.total_bytes);
#else
    CHECK(pgr_system_memory_read(&memory) == -1);
#endif
    CHECK(pgr_system_memory_read(nullptr) == -1);
    std::printf("PGR_SYSTEM_MEMORY_OK total=%llu available=%llu known=%d\n",
        (unsigned long long) memory.total_bytes,
        (unsigned long long) memory.available_bytes,
        memory.available_known);
    return 0;
}
