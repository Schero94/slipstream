#include "peregrine_system_memory.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(__APPLE__)
#include <mach/mach.h>
#include <sys/sysctl.h>
#elif defined(__linux__)
#include <unistd.h>
#endif

static int pgr_mul_u64(uint64_t a, uint64_t b, uint64_t * out) {
    if (a != 0 && b > UINT64_MAX / a) return -1;
    *out = a * b;
    return 0;
}

#if defined(__linux__)
static int pgr_linux_mem_available(uint64_t * out) {
    FILE * stream = fopen("/proc/meminfo", "r");
    if (!stream) return -1;
    char line[256];
    unsigned long long kib = 0;
    int found = 0;
    while (fgets(line, sizeof(line), stream)) {
        if (sscanf(line, "MemAvailable: %llu kB", &kib) == 1) { found = 1; break; }
    }
    fclose(stream);
    return found && pgr_mul_u64((uint64_t) kib, 1024U, out) == 0 ? 0 : -1;
}

/* PGR_CGROUP_ROOT overrides the cgroup mount point -- for unusual mounts, and so
 * the test can drive the parser from a fixture directory. */
static const char * pgr_cgroup_root(void) {
    const char * env = getenv("PGR_CGROUP_ROOT");
    return (env && *env) ? env : "/sys/fs/cgroup";
}

/* Reads a single-value cgroup file.  Returns 0 on a number, 1 for the literal
 * "max" (cgroup v2's "no limit"), -1 when unreadable. */
static int pgr_read_cgroup_u64(const char * dir, const char * name, uint64_t * out) {
    char path[512];
    if (snprintf(path, sizeof(path), "%s/%s", dir, name) >= (int) sizeof(path)) return -1;
    FILE * stream = fopen(path, "r");
    if (!stream) return -1;
    char token[64] = {0};
    const int got = fscanf(stream, "%63s", token);
    fclose(stream);
    if (got != 1) return -1;
    if (strcmp(token, "max") == 0) return 1;
    char * end = NULL;
    const unsigned long long value = strtoull(token, &end, 10);
    if (end == token) return -1;
    *out = (uint64_t) value;
    return 0;
}

/* Sums the file pages the kernel can drop under pressure.  memory.current counts
 * page cache, which is not really occupied -- ignoring that would understate
 * availability as badly as ignoring the limit overstates it. */
static uint64_t pgr_cgroup_reclaimable(const char * dir, const char * const * keys) {
    char path[512];
    if (snprintf(path, sizeof(path), "%s/memory.stat", dir) >= (int) sizeof(path)) return 0;
    FILE * stream = fopen(path, "r");
    if (!stream) return 0;
    char key[64];
    unsigned long long value = 0;
    uint64_t sum = 0;
    while (fscanf(stream, "%63s %llu", key, &value) == 2) {
        for (size_t i = 0; keys[i]; i++) {
            if (strcmp(key, keys[i]) == 0) {
                if (sum <= UINT64_MAX - value) sum += (uint64_t) value;
                break;
            }
        }
    }
    fclose(stream);
    return sum;
}

/* A cgroup memory cap makes the host's /proc/meminfo the wrong authority: a
 * container limited to 2 GiB still reads the host's 8 GiB and would admit a cache
 * the kernel then OOM-kills.  Fills limit and a matching availability, or returns
 * -1 when this process is not capped (or the files are not readable).
 *
 * Availability inside the cap mirrors the macOS choice of free + inactive: the
 * headroom the cap leaves, plus reclaimable file pages. */
static int pgr_cgroup_memory(uint64_t * limit, uint64_t * available) {
    static const char * const v2_keys[] = { "inactive_file", NULL };
    static const char * const v1_keys[] = { "total_inactive_file", NULL };

    const char * root = pgr_cgroup_root();
    char v1[512];
    const struct {
        const char * dir;
        const char * limit_file;
        const char * usage_file;
        const char * const * keys;
    } layouts[] = {
        { root, "memory.max",           "memory.current",     v2_keys }, /* cgroup v2 */
        { v1,   "memory.limit_in_bytes", "memory.usage_in_bytes", v1_keys }, /* cgroup v1 */
    };
    if (snprintf(v1, sizeof(v1), "%s/memory", root) >= (int) sizeof(v1)) return -1;

    for (size_t i = 0; i < sizeof(layouts) / sizeof(layouts[0]); i++) {
        uint64_t cap = 0;
        if (pgr_read_cgroup_u64(layouts[i].dir, layouts[i].limit_file, &cap) != 0) continue;
        /* cgroup v1 spells "no limit" as a near-UINT64_MAX sentinel rather than
         * "max"; either way a cap that large constrains nothing. */
        if (cap == 0 || cap > (UINT64_MAX >> 1)) continue;
        uint64_t used = 0;
        if (pgr_read_cgroup_u64(layouts[i].dir, layouts[i].usage_file, &used) != 0) used = 0;
        uint64_t free_in_cap = cap > used ? cap - used : 0;
        const uint64_t reclaimable = pgr_cgroup_reclaimable(layouts[i].dir, layouts[i].keys);
        if (free_in_cap <= UINT64_MAX - reclaimable) free_in_cap += reclaimable;
        if (free_in_cap > cap) free_in_cap = cap;
        *limit = cap;
        *available = free_in_cap;
        return 0;
    }
    return -1;
}
#endif

int pgr_system_memory_read(pgr_system_memory * out) {
    if (!out) return -1;
    memset(out, 0, sizeof(*out));
#if defined(__APPLE__)
    uint64_t total = 0;
    size_t size = sizeof(total);
    if (sysctlbyname("hw.memsize", &total, &size, NULL, 0) != 0 || size != sizeof(total) || total == 0) return -1;
    out->total_bytes = total;
    mach_port_t host = mach_host_self();
    vm_size_t page_size = 0;
    vm_statistics64_data_t stats;
    mach_msg_type_number_t count = HOST_VM_INFO64_COUNT;
    if (host_page_size(host, &page_size) == KERN_SUCCESS && page_size > 0 &&
            host_statistics64(host, HOST_VM_INFO64, (host_info64_t) &stats, &count) == KERN_SUCCESS) {
        uint64_t pages = 0;
        if ((uint64_t) stats.free_count <= UINT64_MAX - (uint64_t) stats.inactive_count) {
            pages = (uint64_t) stats.free_count + (uint64_t) stats.inactive_count;
            if (pgr_mul_u64(pages, (uint64_t) page_size, &out->available_bytes) == 0 &&
                    out->available_bytes > 0 && out->available_bytes <= total) out->available_known = 1;
        }
    }
    mach_port_deallocate(mach_task_self(), host);
    return 0;
#elif defined(__linux__)
    const long pages = sysconf(_SC_PHYS_PAGES);
    const long page_size = sysconf(_SC_PAGESIZE);
    if (pages <= 0 || page_size <= 0 || pgr_mul_u64((uint64_t) pages, (uint64_t) page_size, &out->total_bytes) != 0) return -1;
    if (pgr_linux_mem_available(&out->available_bytes) == 0 && out->available_bytes <= out->total_bytes) out->available_known = 1;
    /* Whichever authority is tighter wins: the host may be roomy while this
     * process is capped, and a cap may be generous while the host is under
     * pressure. */
    uint64_t cap = 0, cap_available = 0;
    if (pgr_cgroup_memory(&cap, &cap_available) == 0) {
        if (cap < out->total_bytes) out->total_bytes = cap;
        if (!out->available_known || cap_available < out->available_bytes) {
            out->available_bytes = cap_available;
            out->available_known = 1;
        }
        if (out->available_bytes > out->total_bytes) out->available_bytes = out->total_bytes;
    }
    return 0;
#else
    return -1;
#endif
}

#define PGR_GIB (1024ULL * 1024ULL * 1024ULL)

uint64_t pgr_default_headroom_bytes(uint64_t total_bytes) {
    if (total_bytes == 0) return 0;
#if defined(__APPLE__)
    const uint64_t quarter = total_bytes / 4;
    const uint64_t want = 3 * PGR_GIB;
    return want < quarter ? want : quarter;
#else
    uint64_t want = total_bytes / 8;
    if (want < PGR_GIB / 2) want = PGR_GIB / 2;
    if (want > 3 * PGR_GIB) want = 3 * PGR_GIB;
    /* A reserve at or above the machine leaves nothing to run in; on a host this
     * small the caller is better served by a refusal it can read than by a reserve
     * that makes every plan impossible. Half is the most that can be given back
     * while still reserving something. */
    if (want >= total_bytes) want = total_bytes / 2;
    return want;
#endif
}
