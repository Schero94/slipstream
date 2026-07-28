#include "peregrine_system_memory.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>

#if defined(__linux__)
#include <sys/stat.h>
#include <unistd.h>
#endif

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

#if defined(__linux__)
static const uint64_t KiB = 1024, MiB = 1024 * KiB;

static bool write_file(const char * dir, const char * name, const char * body) {
    char path[512];
    std::snprintf(path, sizeof(path), "%s/%s", dir, name);
    FILE * stream = std::fopen(path, "w");
    if (!stream) return false;
    const bool ok = std::fputs(body, stream) >= 0;
    return std::fclose(stream) == 0 && ok;
}

static bool remove_file(const char * dir, const char * name) {
    char path[512];
    std::snprintf(path, sizeof(path), "%s/%s", dir, name);
    return std::remove(path) == 0;
}

static bool read_with_cgroup_root(const char * root, pgr_system_memory * out) {
    setenv("PGR_CGROUP_ROOT", root, 1);
    const bool ok = pgr_system_memory_read(out) == 0;
    unsetenv("PGR_CGROUP_ROOT");
    return ok;
}
#endif

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

#if defined(__linux__)
    // A cgroup memory cap is the tighter authority: inside a container
    // /proc/meminfo still reports the host, so admission would size a cache the
    // kernel then OOM-kills.  Fixtures stand in for real cgroup files so the
    // parser is exercised without needing to be capped ourselves.
    char root[] = "/tmp/pgr_cgroup_XXXXXX";
    CHECK(mkdtemp(root) != nullptr);
    char v1dir[512];
    std::snprintf(v1dir, sizeof(v1dir), "%s/memory", root);
    CHECK(mkdir(v1dir, 0700) == 0);

    // Baseline: no cgroup files at all -> host figures, unclamped.  Everything
    // below is compared against this rather than against absolute numbers, which
    // depend on the machine running the test.
    pgr_system_memory host{};
    CHECK(read_with_cgroup_root(root, &host));
    CHECK(host.total_bytes > 64 * MiB);   // fixtures cap at 64 MiB

    // cgroup v2, capped.  Availability inside the cap counts the headroom plus
    // reclaimable file pages, mirroring free + inactive on macOS.
    CHECK(write_file(root, "memory.max", "67108864\n"));
    CHECK(write_file(root, "memory.current", "16777216\n"));
    CHECK(write_file(root, "memory.stat", "anon 12582912\ninactive_file 4194304\nslab 131072\n"));
    pgr_system_memory capped{};
    CHECK(read_with_cgroup_root(root, &capped));
    CHECK(capped.total_bytes == 64 * MiB);
    CHECK(capped.available_known == 1);
    CHECK(capped.available_bytes == 64 * MiB - 16 * MiB + 4 * MiB);

    // An unreadable memory.stat must not be fatal, only more conservative.
    CHECK(remove_file(root, "memory.stat"));
    pgr_system_memory no_stat{};
    CHECK(read_with_cgroup_root(root, &no_stat));
    CHECK(no_stat.total_bytes == 64 * MiB);
    CHECK(no_stat.available_bytes == 64 * MiB - 16 * MiB);

    // "max" is cgroup v2 for "no limit" -> fall back to the host figures.
    CHECK(write_file(root, "memory.max", "max\n"));
    pgr_system_memory uncapped{};
    CHECK(read_with_cgroup_root(root, &uncapped));
    CHECK(uncapped.total_bytes == host.total_bytes);

    // cgroup v1 keeps the same numbers under different names, one level down.
    CHECK(remove_file(root, "memory.max"));
    CHECK(write_file(v1dir, "memory.limit_in_bytes", "67108864\n"));
    CHECK(write_file(v1dir, "memory.usage_in_bytes", "16777216\n"));
    CHECK(write_file(v1dir, "memory.stat", "cache 4194304\ntotal_inactive_file 4194304\n"));
    pgr_system_memory v1{};
    CHECK(read_with_cgroup_root(root, &v1));
    CHECK(v1.total_bytes == 64 * MiB);
    CHECK(v1.available_bytes == 64 * MiB - 16 * MiB + 4 * MiB);

    // v1 spells "no limit" as a near-UINT64_MAX sentinel instead of "max".
    CHECK(write_file(v1dir, "memory.limit_in_bytes", "9223372036854771712\n"));
    pgr_system_memory v1_uncapped{};
    CHECK(read_with_cgroup_root(root, &v1_uncapped));
    CHECK(v1_uncapped.total_bytes == host.total_bytes);

    remove_file(v1dir, "memory.limit_in_bytes");
    remove_file(v1dir, "memory.usage_in_bytes");
    remove_file(v1dir, "memory.stat");
    remove_file(root, "memory.current");
    rmdir(v1dir);
    rmdir(root);
    std::printf("PGR_CGROUP_CLAMP_OK cap=%lluMiB avail=%lluMiB (host total %lluMiB)\n",
        (unsigned long long) (capped.total_bytes / MiB),
        (unsigned long long) (capped.available_bytes / MiB),
        (unsigned long long) (host.total_bytes / MiB));
#endif

    // ---- default reserve policy ----
    // Properties that have to hold on every platform, because a caller relies on
    // them to get a usable plan without naming a number.
    const uint64_t GiB = 1024ULL * 1024ULL * 1024ULL;
    CHECK(pgr_default_headroom_bytes(0) == 0);
    for (uint64_t total = GiB; total <= 512 * GiB; total *= 2) {
        const uint64_t reserve = pgr_default_headroom_bytes(total);
        CHECK(reserve > 0);
        CHECK(reserve < total);            // something must be left to run in
        CHECK(reserve <= 3 * GiB);         // the reserve never grows without bound
    }
    // Non-decreasing in machine size: a bigger host must never reserve less.
    uint64_t previous = 0;
    for (uint64_t total = GiB; total <= 512 * GiB; total += GiB) {
        const uint64_t reserve = pgr_default_headroom_bytes(total);
        CHECK(reserve >= previous);
        previous = reserve;
    }
    // The one measured point: 3 GiB on the 36 GiB machine every published number
    // comes from. Both policies agree here, which is why it is the anchor.
    CHECK(pgr_default_headroom_bytes(36 * GiB) == 3 * GiB);
#if defined(__APPLE__)
    // Capped at a quarter so a small Mac can still stream at all.
    CHECK(pgr_default_headroom_bytes(8 * GiB) == 2 * GiB);
    CHECK(pgr_default_headroom_bytes(4 * GiB) == 1 * GiB);
#else
    // An eighth, floored: a 2 GiB container keeps 512 MiB and can still stream.
    CHECK(pgr_default_headroom_bytes(8 * GiB) == 1 * GiB);
    CHECK(pgr_default_headroom_bytes(2 * GiB) == GiB / 2);
#endif

    std::printf("PGR_HEADROOM_OK 2GiB=%lluMiB 8GiB=%lluMiB 36GiB=%lluMiB\n",
        (unsigned long long) (pgr_default_headroom_bytes(2 * GiB) / (1024 * 1024)),
        (unsigned long long) (pgr_default_headroom_bytes(8 * GiB) / (1024 * 1024)),
        (unsigned long long) (pgr_default_headroom_bytes(36 * GiB) / (1024 * 1024)));
    std::printf("PGR_SYSTEM_MEMORY_OK total=%llu available=%llu known=%d\n",
        (unsigned long long) memory.total_bytes,
        (unsigned long long) memory.available_bytes,
        memory.available_known);
    return 0;
}
