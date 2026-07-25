#include "peregrine_system_memory.h"

#include <stdio.h>
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
    return 0;
#else
    return -1;
#endif
}
