/* peregrine_predict.c — PGCT1 per-layer hot-set predictor (see header). */
#include "peregrine_predict.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct { uint16_t layer; uint16_t hot_count; const uint16_t *ids; } pgr_layer_rec;

struct pgr_predict {
    unsigned char *blob;
    size_t         blob_size;
    uint32_t       layer_count;
    pgr_layer_rec *recs;
};

static uint16_t pgr_rd_u16(const unsigned char *p) { return (uint16_t)(p[0] | ((uint16_t)p[1] << 8)); }
static uint32_t pgr_rd_u32(const unsigned char *p) {
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

void pgr_predict_free(pgr_predict *p) {
    if (!p) return;
    free(p->recs);
    free(p->blob);
    free(p);
}

pgr_predict *pgr_predict_open(const void *data, size_t size) {
    if (!data || size < 16) return NULL;
    const unsigned char *raw = (const unsigned char *)data;
    if (memcmp(raw, "PGCT1\0\0\0", 8) != 0) return NULL;
    if (pgr_rd_u32(raw + 8) != 1u) return NULL;
    const uint32_t n = pgr_rd_u32(raw + 12);

    pgr_predict *p = (pgr_predict *)calloc(1, sizeof(pgr_predict));
    if (!p) return NULL;
    p->blob = (unsigned char *)malloc(size ? size : 1);
    if (!p->blob) { pgr_predict_free(p); return NULL; }
    memcpy(p->blob, raw, size);
    p->blob_size = size;
    p->layer_count = n;
    if (n == 0) { p->recs = NULL; return p; }
    p->recs = (pgr_layer_rec *)calloc(n, sizeof(pgr_layer_rec));
    if (!p->recs) { pgr_predict_free(p); return NULL; }

    size_t cursor = 16;
    int32_t prev_layer = -1;
    for (uint32_t i = 0; i < n; ++i) {
        if (cursor + 4 > p->blob_size) { pgr_predict_free(p); return NULL; }
        const uint16_t layer = pgr_rd_u16(p->blob + cursor);
        const uint16_t hot   = pgr_rd_u16(p->blob + cursor + 2);
        cursor += 4;
        if ((size_t)hot * 2u > p->blob_size - cursor) { pgr_predict_free(p); return NULL; }
        if ((int32_t)layer <= prev_layer) { pgr_predict_free(p); return NULL; }  /* strictly ascending */
        prev_layer = (int32_t)layer;
        p->recs[i].layer = layer;
        p->recs[i].hot_count = hot;
        p->recs[i].ids = (const uint16_t *)(p->blob + cursor);  /* raw LE u16s; decoded on read */
        cursor += (size_t)hot * 2u;
    }
    return p;
}

pgr_predict *pgr_predict_load(const char *path) {
    if (!path) return NULL;
    FILE *f = fopen(path, "rb");
    if (!f) return NULL;
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); return NULL; }
    long sz = ftell(f);
    if (sz < 0 || fseek(f, 0, SEEK_SET) != 0) { fclose(f); return NULL; }
    unsigned char *buf = (unsigned char *)malloc((size_t)sz ? (size_t)sz : 1);
    if (!buf) { fclose(f); return NULL; }
    size_t got = fread(buf, 1, (size_t)sz, f);
    fclose(f);
    pgr_predict *p = (got == (size_t)sz) ? pgr_predict_open(buf, (size_t)sz) : NULL;
    free(buf);
    return p;
}

static const pgr_layer_rec *pgr_predict_find(const pgr_predict *p, uint16_t layer) {
    size_t lo = 0, hi = p->layer_count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        if (p->recs[mid].layer < layer) lo = mid + 1;
        else hi = mid;
    }
    if (lo < p->layer_count && p->recs[lo].layer == layer) return &p->recs[lo];
    return NULL;
}

int pgr_predict_hot(const pgr_predict *p, uint16_t layer, uint16_t *out, int max) {
    if (!p || !out || max <= 0 || p->layer_count == 0) return 0;
    const pgr_layer_rec *r = pgr_predict_find(p, layer);
    if (!r) return 0;
    int count = r->hot_count < (uint16_t)max ? (int)r->hot_count : max;
    for (int i = 0; i < count; ++i) out[i] = pgr_rd_u16((const unsigned char *)(r->ids + i));
    return count;
}

size_t pgr_predict_layer_count(const pgr_predict *p) { return p ? p->layer_count : 0; }

/* ---- PGCC1 coupled predictor ------------------------------------------------- */

typedef struct { uint16_t expert; uint16_t succ_count; const unsigned char *succ; } pgr_cpl_expert;
typedef struct { uint16_t layer; uint16_t expert_count; pgr_cpl_expert *experts; } pgr_cpl_layer;

struct pgr_coupling {
    unsigned char *blob;
    size_t         blob_size;
    uint32_t       layer_count;
    pgr_cpl_layer *layers;
};

void pgr_coupling_free(pgr_coupling *c) {
    if (!c) return;
    if (c->layers) {
        for (uint32_t i = 0; i < c->layer_count; ++i) free(c->layers[i].experts);
        free(c->layers);
    }
    free(c->blob);
    free(c);
}

pgr_coupling *pgr_coupling_open(const void *data, size_t size) {
    if (!data || size < 16) return NULL;
    const unsigned char *raw = (const unsigned char *)data;
    if (memcmp(raw, "PGCC1\0\0\0", 8) != 0) return NULL;
    if (pgr_rd_u32(raw + 8) != 1u) return NULL;
    const uint32_t n = pgr_rd_u32(raw + 12);

    pgr_coupling *c = (pgr_coupling *)calloc(1, sizeof(pgr_coupling));
    if (!c) return NULL;
    c->blob = (unsigned char *)malloc(size ? size : 1);
    if (!c->blob) { pgr_coupling_free(c); return NULL; }
    memcpy(c->blob, raw, size);
    c->blob_size = size;
    c->layer_count = n;
    if (n == 0) { c->layers = NULL; return c; }
    c->layers = (pgr_cpl_layer *)calloc(n, sizeof(pgr_cpl_layer));
    if (!c->layers) { pgr_coupling_free(c); return NULL; }

    size_t cursor = 16;
    int32_t prev_layer = -1;
    for (uint32_t i = 0; i < n; ++i) {
        if (cursor + 4 > c->blob_size) { pgr_coupling_free(c); return NULL; }
        const uint16_t layer  = pgr_rd_u16(c->blob + cursor);
        const uint16_t ecount = pgr_rd_u16(c->blob + cursor + 2);
        cursor += 4;
        if ((int32_t)layer <= prev_layer) { pgr_coupling_free(c); return NULL; }  /* strictly ascending */
        prev_layer = (int32_t)layer;
        c->layers[i].layer = layer;
        c->layers[i].expert_count = ecount;
        if (ecount == 0) continue;
        c->layers[i].experts = (pgr_cpl_expert *)calloc(ecount, sizeof(pgr_cpl_expert));
        if (!c->layers[i].experts) { pgr_coupling_free(c); return NULL; }
        int32_t prev_expert = -1;
        for (uint16_t j = 0; j < ecount; ++j) {
            if (cursor + 4 > c->blob_size) { pgr_coupling_free(c); return NULL; }
            const uint16_t expert = pgr_rd_u16(c->blob + cursor);
            const uint16_t scount = pgr_rd_u16(c->blob + cursor + 2);
            cursor += 4;
            if ((int32_t)expert <= prev_expert) { pgr_coupling_free(c); return NULL; }  /* strictly ascending */
            prev_expert = (int32_t)expert;
            if ((size_t)scount * 4u > c->blob_size - cursor) { pgr_coupling_free(c); return NULL; }
            c->layers[i].experts[j].expert = expert;
            c->layers[i].experts[j].succ_count = scount;
            c->layers[i].experts[j].succ = c->blob + cursor;  /* raw LE (id,weight) pairs */
            cursor += (size_t)scount * 4u;
        }
    }
    return c;
}

pgr_coupling *pgr_coupling_load(const char *path) {
    if (!path) return NULL;
    FILE *f = fopen(path, "rb");
    if (!f) return NULL;
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); return NULL; }
    long sz = ftell(f);
    if (sz < 0 || fseek(f, 0, SEEK_SET) != 0) { fclose(f); return NULL; }
    unsigned char *buf = (unsigned char *)malloc((size_t)sz ? (size_t)sz : 1);
    if (!buf) { fclose(f); return NULL; }
    size_t got = fread(buf, 1, (size_t)sz, f);
    fclose(f);
    pgr_coupling *c = (got == (size_t)sz) ? pgr_coupling_open(buf, (size_t)sz) : NULL;
    free(buf);
    return c;
}

static const pgr_cpl_layer *pgr_coupling_find_layer(const pgr_coupling *c, uint16_t layer) {
    size_t lo = 0, hi = c->layer_count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        if (c->layers[mid].layer < layer) lo = mid + 1;
        else hi = mid;
    }
    if (lo < c->layer_count && c->layers[lo].layer == layer) return &c->layers[lo];
    return NULL;
}

static const pgr_cpl_expert *pgr_coupling_find_expert(const pgr_cpl_layer *l, uint16_t expert) {
    size_t lo = 0, hi = l->expert_count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        if (l->experts[mid].expert < expert) lo = mid + 1;
        else hi = mid;
    }
    if (lo < l->expert_count && l->experts[lo].expert == expert) return &l->experts[lo];
    return NULL;
}

#define PGR_CPL_CAND_MAX 1024

int pgr_coupling_next(const pgr_coupling *c, uint16_t src_layer,
                      const uint16_t *fired, int n_fired, uint16_t *out, int max) {
    if (!c || !fired || !out || n_fired <= 0 || max <= 0 || c->layer_count == 0) return 0;
    const pgr_cpl_layer *l = pgr_coupling_find_layer(c, src_layer);
    if (!l || l->expert_count == 0) return 0;

    uint16_t cand_id[PGR_CPL_CAND_MAX];
    uint32_t cand_w[PGR_CPL_CAND_MAX];   /* u32 so summed u16 weights cannot overflow */
    int ncand = 0;

    for (int f = 0; f < n_fired; ++f) {
        const pgr_cpl_expert *e = pgr_coupling_find_expert(l, fired[f]);
        if (!e) continue;
        for (uint16_t s = 0; s < e->succ_count; ++s) {
            const unsigned char *pair = e->succ + (size_t)s * 4u;
            const uint16_t id = pgr_rd_u16(pair);
            const uint16_t w  = pgr_rd_u16(pair + 2);
            int slot = -1;
            for (int k = 0; k < ncand; ++k) { if (cand_id[k] == id) { slot = k; break; } }
            if (slot >= 0) cand_w[slot] += w;
            else if (ncand < PGR_CPL_CAND_MAX) { cand_id[ncand] = id; cand_w[ncand] = w; ++ncand; }
        }
    }
    if (ncand == 0) return 0;

    int want = max < ncand ? max : ncand;
    char used[PGR_CPL_CAND_MAX] = {0};
    for (int i = 0; i < want; ++i) {
        int best = -1;
        for (int k = 0; k < ncand; ++k) {
            if (used[k]) continue;
            if (best < 0 || cand_w[k] > cand_w[best] ||
                (cand_w[k] == cand_w[best] && cand_id[k] < cand_id[best])) best = k;
        }
        used[best] = 1;
        out[i] = cand_id[best];
    }
    return want;
}

size_t pgr_coupling_layer_count(const pgr_coupling *c) { return c ? c->layer_count : 0; }
