/* peregrine_io.h — one place to say how a file will be read, per platform.
 *
 * PGRN manages its expert cache in user space against an explicit RAM budget, so
 * the kernel keeping a second copy is not a bonus: it is a competitor for the very
 * memory the budget is protecting. On macOS one fcntl says all of that at once.
 * Linux needs the same intent spelled out differently, and — this is the part that
 * bit us — `fcntl(fd, F_NOCACHE, 1)` is not portable in the silent way it looks:
 * F_NOCACHE is undefined there, so the previous `#define F_NOCACHE 48` fallback
 * issued an unknown fcntl command that failed with EINVAL and was discarded. The
 * reads then ran fully buffered with readahead — the opposite of what every call
 * site asked for.
 *
 * The functions below therefore name the access pattern rather than the syscall:
 *
 *   pgr_io_hint_random      scattered record reads (expert streaming)
 *   pgr_io_hint_sequential  one pass front to back (hashing, converting)
 *   pgr_io_forget           this range is consumed; no need to keep it
 *
 * PGRN_BUFFERED disables all of it, for drives that genuinely run smoother through
 * the page cache (DRAM-less externals). It used to be honoured at one call site
 * only; now it holds everywhere.
 *
 * The macOS path is deliberately unchanged — plain F_NOCACHE, no added F_RDAHEAD —
 * because every published measurement was taken with exactly that.
 */
#ifndef PEREGRINE_IO_H
#define PEREGRINE_IO_H

#include <stdlib.h>

#if defined(__APPLE__)
#include <fcntl.h>
#elif defined(__linux__)
#include <fcntl.h>
#endif

#ifdef __cplusplus
extern "C" {
#endif

static inline int pgr_io_buffered_requested(void) {
    return getenv("PGRN_BUFFERED") != NULL;
}

static inline void pgr_io_hint_random(int fd) {
    if (fd < 0 || pgr_io_buffered_requested()) return;
#if defined(__APPLE__)
    (void) fcntl(fd, F_NOCACHE, 1);
#elif defined(__linux__)
    /* POSIX_FADV_RANDOM switches readahead off: an expert record is one bounded
     * read at an offset nobody can predict from the last one, so reading ahead
     * only spends bandwidth on data that will be dropped. */
    (void) posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM);
#else
    (void) fd;
#endif
}

static inline void pgr_io_hint_sequential(int fd) {
    if (fd < 0 || pgr_io_buffered_requested()) return;
#if defined(__APPLE__)
    (void) fcntl(fd, F_NOCACHE, 1);
#elif defined(__linux__)
    (void) posix_fadvise(fd, 0, 0, POSIX_FADV_SEQUENTIAL);
#else
    (void) fd;
#endif
}

/* Best effort by construction: Linux only drops clean pages, and macOS has
 * already been told not to keep any. Length 0 means "to end of file". */
static inline void pgr_io_forget(int fd, long long offset, long long length) {
    if (fd < 0 || pgr_io_buffered_requested()) return;
#if defined(__linux__)
    (void) posix_fadvise(fd, (off_t) offset, (off_t) length, POSIX_FADV_DONTNEED);
#else
    (void) fd; (void) offset; (void) length;
#endif
}

#ifdef __cplusplus
}
#endif
#endif
