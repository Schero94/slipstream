/* peregrine_predict.h — native expert-prefetch predictor.
 *
 * Loads a compact per-layer "hot set" table (PGCT1) measured offline from routing traces
 * and answers: for layer L, which experts are most likely to fire? The streaming runtime
 * warms that set (pgr_runtime_prefetch) during the previous layer's compute to turn cold
 * decode misses into hits. The predictor never selects experts for compute — it only warms
 * the cache — so it can never change logits (parity-neutral by construction).
 *
 * Binary format (little-endian):
 *   [0]  magic  "PGCT1\0\0\0"          (8 bytes)
 *   [8]  uint32 version                (= 1)
 *   [12] uint32 layer_count
 *   then layer_count records, sorted ascending by layer:
 *        uint16 layer_id
 *        uint16 hot_count
 *        hot_count * uint16 expert_id  (ranked, hottest first)
 */
#ifndef PEREGRINE_PREDICT_H
#define PEREGRINE_PREDICT_H
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pgr_predict pgr_predict;

/* Load + validate a PGCT1 table. Returns NULL on missing file, bad magic/version,
 * unsorted/duplicate layers, or truncation. */
pgr_predict *pgr_predict_load(const char *path);

/* Load from an in-memory image (used by tests / embedded tables). */
pgr_predict *pgr_predict_open(const void *data, size_t size);

/* Copy up to `max` predicted-hot expert ids for `layer` into `out` (ranked, hottest
 * first). Returns the number written, or 0 if the layer is absent / args invalid. */
int pgr_predict_hot(const pgr_predict *p, uint16_t layer, uint16_t *out, int max);

size_t pgr_predict_layer_count(const pgr_predict *p);
void pgr_predict_free(pgr_predict *p);

/* ---- PGCC1 coupled predictor -------------------------------------------------
 *
 * The hot-set table above conditions only on the layer id (a fixed per-layer
 * marginal). The coupled table conditions on which experts actually fired at the
 * source layer L to predict layer L+1's experts - the coupled signal measured to
 * beat the marginal by +14..20pp recall in trace replay. Same parity guarantee:
 * the runtime only warms the predicted set, it never selects experts for compute.
 *
 * Binary format (little-endian):
 *   [0]  magic  "PGCC1\0\0\0"          (8 bytes)
 *   [8]  uint32 version                (= 1)
 *   [12] uint32 layer_count            (source layers present, ascending)
 *   then layer_count layer blocks, sorted ascending by layer:
 *        uint16 layer_id
 *        uint16 expert_count           (source experts with successors, ascending)
 *        then expert_count expert blocks, sorted ascending by expert:
 *             uint16 expert_id
 *             uint16 succ_count
 *             succ_count * (uint16 succ_expert_id, uint16 weight)   ranked, heaviest first
 */
typedef struct pgr_coupling pgr_coupling;

pgr_coupling *pgr_coupling_load(const char *path);
pgr_coupling *pgr_coupling_open(const void *data, size_t size);

/* Given the experts that fired at `src_layer` (fired[0..n_fired)), predict the
 * layer src_layer+1 experts: union the fired experts' successor lists, summing
 * weights, and copy up to `max` ids (heaviest first, ties broken by ascending id)
 * into `out`. Returns the number written, or 0 if the layer is absent / no
 * successors / args invalid. The union scoreboard holds at most 1024 distinct
 * candidates (n_fired * per-expert successors); beyond that, later new ids are
 * dropped - unreachable at realistic top_k, and prefetch-only so it never affects
 * logits, only a marginal recall loss. */
int pgr_coupling_next(const pgr_coupling *c, uint16_t src_layer,
                      const uint16_t *fired, int n_fired, uint16_t *out, int max);

size_t pgr_coupling_layer_count(const pgr_coupling *c);
void pgr_coupling_free(pgr_coupling *c);

#ifdef __cplusplus
}
#endif
#endif
