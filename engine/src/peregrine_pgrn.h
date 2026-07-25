/* Native, no-mmap reader for the bounded Peregrine expert container. */
#ifndef PEREGRINE_PGRN_H
#define PEREGRINE_PGRN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pgrn_file pgrn_file;

typedef struct {
    uint16_t layer;
    uint16_t expert;
    uint8_t  precision;
    uint8_t  flags;
    float    heat;
    uint64_t offset;
    uint32_t nbytes;
    uint32_t crc32;
} pgrn_expert_ref;

typedef struct {
    uint16_t layer;
    uint32_t ggml_type[3];
    uint64_t nbytes[3];
} pgrn_tensor_layout;

/* Open and validate metadata + directory only. No tensor payload is mapped. */
pgrn_file * pgrn_open(const char * path, const char * expected_sha256);

int         pgrn_find(const pgrn_file * file, uint16_t layer, uint16_t expert, pgrn_expert_ref * out);
int         pgrn_find_layout(const pgrn_file * file, uint16_t layer, pgrn_tensor_layout * out);
int         pgrn_read_expert(pgrn_file * file, const pgrn_expert_ref * ref, void * dst, size_t dst_size);
/* Thread-safe expert read: reads via pread with an explicit offset and writes any
 * error into the caller-owned `err` buffer only — never the shared pgrn_file state —
 * so it is safe to call concurrently on one open file with distinct `dst` buffers. */
int         pgrn_read_expert_mt(const pgrn_file * file, const pgrn_expert_ref * ref,
                                void * dst, size_t dst_size, char * err, size_t err_capacity);
size_t      pgrn_count(const pgrn_file * file);
size_t      pgrn_layer_count(const pgrn_file * file);
int         pgrn_layer_at(const pgrn_file * file, size_t index, uint16_t * layer);
uint32_t    pgrn_experts_per_layer(const pgrn_file * file);
size_t      pgrn_max_expert_bytes(const pgrn_file * file);
uint64_t    pgrn_total_expert_bytes(const pgrn_file * file);
const char *pgrn_model_sha256(const pgrn_file * file);
const char *pgrn_error(const pgrn_file * file);
void        pgrn_close(pgrn_file * file);

#ifdef __cplusplus
}
#endif
#endif
