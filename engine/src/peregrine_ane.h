/* Bounded Core ML proposal adapter. Proposals are never authoritative tokens. */
#ifndef PEREGRINE_ANE_H
#define PEREGRINE_ANE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pgr_ane pgr_ane;

typedef struct {
    const char * compiled_model_path;
    const char * expected_model_sha256;
    const char * expected_package_sha256;
    const char * architecture;
    const char * expected_precision;
    const char * input_name;
    const char * output_name;
    size_t input_count;
    size_t candidate_count;
    uint64_t allocation_budget_bytes;
} pgr_ane_params;

typedef struct {
    uint64_t calls;
    uint64_t candidates;
    uint64_t failures;
    uint64_t prediction_us;
    uint64_t allocated_bytes;
} pgr_ane_stats;

typedef struct {
    size_t input_count;
    size_t candidate_count;
    int32_t pad_token;
} pgr_ane_info;

typedef struct {
    const char * manifest_path;
    const char * expected_model_sha256;
    const char * expected_architecture;
    uint32_t expected_vocabulary;
    uint64_t allocation_budget_bytes;
} pgr_ane_manifest_params;

/* Canonical SHA-256 over sorted relative paths and regular-file contents. */
int pgr_ane_package_sha256(
        const char * compiled_model_path,
        char output_hex[65],
        char * error,
        size_t error_capacity);

pgr_ane * pgr_ane_new(const pgr_ane_params * params, char * error, size_t error_capacity);

/* The manifest is the only production entry point. It binds a relative package
 * path, package hash, source-model identity and fixed tensor contract. */
pgr_ane * pgr_ane_new_from_manifest(
        const pgr_ane_manifest_params * params,
        pgr_ane_info * info,
        char * error,
        size_t error_capacity);

/* Returns proposals only. The caller must send every candidate through the full
 * target-model verification path before accepting a token. */
int pgr_ane_propose(
        pgr_ane * adapter,
        const int32_t * input_tokens,
        size_t input_count,
        const int32_t ** candidates,
        size_t * candidate_count,
        char * error,
        size_t error_capacity);

void pgr_ane_get_stats(const pgr_ane * adapter, pgr_ane_stats * stats);
void pgr_ane_free(pgr_ane * adapter);

#ifdef __cplusplus
}
#endif
#endif
