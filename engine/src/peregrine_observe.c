/* peregrine_observe.c — run our cloxcache as a NATIVE flow inside llama.cpp inference.
 *
 * Called from build_moe_ffn (via ggml_map_custom1) with the REAL per-token expert
 * selections at runtime. Feeds them through the measured cloxcache at a bounded
 * capacity and tracks the LIVE in-engine hit-rate — i.e. exactly how many expert
 * accesses our GPU-resident cache would serve without a stream, on the model that is
 * actually running. Compute-neutral: the caller copies the tensor through unchanged, so
 * model output is byte-identical. This is the foundation the residency rerouting builds
 * on (stage: measure natively in the live flow, then reroute compute). */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef struct pgr_clox pgr_clox;
extern pgr_clox *pgr_clox_new(int capacity, int k);
extern int       pgr_clox_access(pgr_clox *c, long key);

static pgr_clox *g_cache = NULL;
static long g_hits = 0, g_acc = 0;
static int  g_cap = 0, g_k = 4;
static int  g_enabled = -1;   /* -1 unknown, 0 off, 1 on */

static int pgr_on(void) {
    if (g_enabled < 0) g_enabled = getenv("PGR_STREAM_EXPERTS") != NULL ? 1 : 0;
    return g_enabled;
}

static void pgr_ensure(void) {
    if (g_cache) return;
    const char *c = getenv("PGR_STREAM_CAP");
    g_cap = c ? atoi(c) : 4000;                 /* resident expert slots (GPU-resident set) */
    if (g_cap < 1) g_cap = 4000;
    const char *k = getenv("PGR_STREAM_K");
    g_k = k ? atoi(k) : 4;
    if (g_k < 1) g_k = 4;
    g_cache = pgr_clox_new(g_cap, g_k);
}

/* feed one layer's runtime expert selections into the live cache (single-threaded) */
void pgr_observe(int layer, const int32_t *ids, int n) {
    if (!pgr_on()) return;                       /* default off; no work unless enabled */
    pgr_ensure();
    if (!g_cache) return;
    for (int i = 0; i < n; i++) {
        int e = ids[i];
        if (e < 0) continue;                    /* padding / unused slot */
        long key = (long)layer * 100000L + (long)e;
        g_acc++;
        g_hits += pgr_clox_access(g_cache, key);
    }
    if (g_acc > 0 && (g_acc % 5000) < (long)n) {
        fprintf(stderr, "[peregrine] live expert-cache: %.2f%% hit (%ld/%ld), cap=%d experts\n",
                100.0 * (double)g_hits / (double)g_acc, g_hits, g_acc, g_cap);
    }
}
