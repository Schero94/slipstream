// test-peregrine-predict — PGCT1 hot-set predictor loader + query.
#include "../src/peregrine_predict.h"
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>

#define CHECK(c) do { if (!(c)) { printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

static void put_u16(std::vector<unsigned char> &v, uint16_t x) { v.push_back(x & 0xff); v.push_back(x >> 8); }
static void put_u32(std::vector<unsigned char> &v, uint32_t x) {
    for (int i = 0; i < 4; ++i) v.push_back((unsigned char)(x >> (8 * i)));
}

// Build a PGCT1 image from {layer -> [expert ids]} pairs (in given order).
static std::vector<unsigned char> build(uint32_t version,
        const std::vector<std::pair<uint16_t, std::vector<uint16_t>>> &layers) {
    std::vector<unsigned char> v;
    const char magic[8] = {'P','G','C','T','1',0,0,0};
    v.insert(v.end(), magic, magic + 8);
    put_u32(v, version);
    put_u32(v, (uint32_t)layers.size());
    for (auto &l : layers) {
        put_u16(v, l.first);
        put_u16(v, (uint16_t)l.second.size());
        for (uint16_t e : l.second) put_u16(v, e);
    }
    return v;
}

// Build a PGCC1 image: layers -> [ (layer, [ (expert, [ (succ_id, weight) ]) ]) ], in given order.
typedef std::vector<std::pair<uint16_t, uint16_t>> CplSucc;      // (succ_id, weight)
typedef std::vector<std::pair<uint16_t, CplSucc>> CplLayer;      // (expert, successors)
static std::vector<unsigned char> build_cpl(uint32_t version,
        const std::vector<std::pair<uint16_t, CplLayer>> &layers) {
    std::vector<unsigned char> v;
    const char magic[8] = {'P','G','C','C','1',0,0,0};
    v.insert(v.end(), magic, magic + 8);
    put_u32(v, version);
    put_u32(v, (uint32_t)layers.size());
    for (auto &l : layers) {
        put_u16(v, l.first);
        put_u16(v, (uint16_t)l.second.size());
        for (auto &e : l.second) {
            put_u16(v, e.first);
            put_u16(v, (uint16_t)e.second.size());
            for (auto &s : e.second) { put_u16(v, s.first); put_u16(v, s.second); }
        }
    }
    return v;
}

int main() {
    // valid table: layers 3 and 7, ranked hot sets
    auto img = build(1, {{3, {11, 22, 33}}, {7, {40, 41}}});
    pgr_predict *p = pgr_predict_open(img.data(), img.size());
    CHECK(p != nullptr);
    CHECK(pgr_predict_layer_count(p) == 2);

    uint16_t out[8];
    // full read of layer 3, ranked order preserved
    CHECK(pgr_predict_hot(p, 3, out, 8) == 3);
    CHECK(out[0] == 11 && out[1] == 22 && out[2] == 33);
    // max clamps
    CHECK(pgr_predict_hot(p, 3, out, 2) == 2 && out[0] == 11 && out[1] == 22);
    // second layer
    CHECK(pgr_predict_hot(p, 7, out, 8) == 2 && out[0] == 40 && out[1] == 41);
    // absent layer -> 0
    CHECK(pgr_predict_hot(p, 5, out, 8) == 0);
    CHECK(pgr_predict_hot(p, 0, out, 8) == 0);
    // invalid args
    CHECK(pgr_predict_hot(p, 3, out, 0) == 0);
    CHECK(pgr_predict_hot(nullptr, 3, out, 8) == 0);
    pgr_predict_free(p);

    // reject: bad magic
    auto bad = img; bad[0] = 'X';
    CHECK(pgr_predict_open(bad.data(), bad.size()) == nullptr);
    // reject: wrong version
    CHECK(pgr_predict_open(build(2, {{3, {1}}}).data(), build(2, {{3, {1}}}).size()) == nullptr);
    // reject: unsorted / duplicate layers
    CHECK(pgr_predict_open(build(1, {{7, {1}}, {3, {2}}}).data(), build(1, {{7, {1}}, {3, {2}}}).size()) == nullptr);
    CHECK(pgr_predict_open(build(1, {{3, {1}}, {3, {2}}}).data(), build(1, {{3, {1}}, {3, {2}}}).size()) == nullptr);
    // reject: truncated (drop last 2 bytes so the final id is missing)
    auto trunc = img; trunc.resize(trunc.size() - 2);
    CHECK(pgr_predict_open(trunc.data(), trunc.size()) == nullptr);
    // too small
    CHECK(pgr_predict_open(img.data(), 4) == nullptr);

    // empty table (0 layers) is valid, queries return 0
    auto empty = build(1, {});
    pgr_predict *pe = pgr_predict_open(empty.data(), empty.size());
    CHECK(pe != nullptr && pgr_predict_layer_count(pe) == 0);
    CHECK(pgr_predict_hot(pe, 3, out, 8) == 0);
    pgr_predict_free(pe);

    // ---- PGCC1 coupled predictor ----
    // layer 1: expert 5 -> two equal-weight successors (50,30) to exercise the id tie-break
    // layer 3: experts 11 and 22 share successor 100 (weights sum on union)
    // layer 7: expert 40 -> single successor
    auto cimg = build_cpl(1, {
        {1, {{5, {{50, 3}, {30, 3}}}}},
        {3, {{11, {{100, 5}, {101, 2}}}, {22, {{100, 3}, {102, 4}}}}},
        {7, {{40, {{200, 1}}}}},
    });
    pgr_coupling *c = pgr_coupling_open(cimg.data(), cimg.size());
    CHECK(c != nullptr);
    CHECK(pgr_coupling_layer_count(c) == 3);

    uint16_t co[8];
    // union {11,22} at layer 3: 100=5+3=8, 102=4, 101=2 -> ranked by weight
    uint16_t fired_both[2] = {11, 22};
    CHECK(pgr_coupling_next(c, 3, fired_both, 2, co, 8) == 3);
    CHECK(co[0] == 100 && co[1] == 102 && co[2] == 101);
    // max clamps to the heaviest
    CHECK(pgr_coupling_next(c, 3, fired_both, 2, co, 2) == 2 && co[0] == 100 && co[1] == 102);
    CHECK(pgr_coupling_next(c, 3, fired_both, 1, co, 8) == 2 && co[0] == 100 && co[1] == 101); // only expert 11
    // single fired expert 22: 102=4, 100=3
    uint16_t fired_22[1] = {22};
    CHECK(pgr_coupling_next(c, 3, fired_22, 1, co, 8) == 2 && co[0] == 102 && co[1] == 100);
    // equal weights -> lower id first (30 before 50)
    uint16_t fired_5[1] = {5};
    CHECK(pgr_coupling_next(c, 1, fired_5, 1, co, 8) == 2 && co[0] == 30 && co[1] == 50);
    // absent expert at present layer -> 0
    uint16_t fired_absent[1] = {99};
    CHECK(pgr_coupling_next(c, 3, fired_absent, 1, co, 8) == 0);
    // absent layer -> 0
    CHECK(pgr_coupling_next(c, 5, fired_both, 2, co, 8) == 0);
    // invalid args
    CHECK(pgr_coupling_next(c, 3, fired_both, 0, co, 8) == 0);
    CHECK(pgr_coupling_next(c, 3, fired_both, 2, co, 0) == 0);
    CHECK(pgr_coupling_next(nullptr, 3, fired_both, 2, co, 8) == 0);
    CHECK(pgr_coupling_next(c, 3, nullptr, 2, co, 8) == 0);
    pgr_coupling_free(c);

    // reject: bad magic
    auto cbad = cimg; cbad[0] = 'X';
    CHECK(pgr_coupling_open(cbad.data(), cbad.size()) == nullptr);
    // reject: wrong version
    auto cv2 = build_cpl(2, {{1, {{5, {{50, 3}}}}}});
    CHECK(pgr_coupling_open(cv2.data(), cv2.size()) == nullptr);
    // reject: unsorted layers
    auto cul = build_cpl(1, {{7, {{1, {{2, 1}}}}}, {3, {{1, {{2, 1}}}}}});
    CHECK(pgr_coupling_open(cul.data(), cul.size()) == nullptr);
    // reject: unsorted experts within a layer
    auto cue = build_cpl(1, {{3, {{22, {{1, 1}}}, {11, {{2, 1}}}}}});
    CHECK(pgr_coupling_open(cue.data(), cue.size()) == nullptr);
    // reject: truncated successor pair
    auto ctr = cimg; ctr.resize(ctr.size() - 2);
    CHECK(pgr_coupling_open(ctr.data(), ctr.size()) == nullptr);

    // empty coupling table is valid, queries return 0
    auto cempty = build_cpl(1, {});
    pgr_coupling *ce = pgr_coupling_open(cempty.data(), cempty.size());
    CHECK(ce != nullptr && pgr_coupling_layer_count(ce) == 0);
    CHECK(pgr_coupling_next(ce, 3, fired_both, 2, co, 8) == 0);
    pgr_coupling_free(ce);

    printf("PGR_PREDICT_OK\n");
    return 0;
}
