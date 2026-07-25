#ifndef PEREGRINE_SHA256_H
#define PEREGRINE_SHA256_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

int pgr_sha256_bytes(const void * data, size_t size, char out_hex[65]);
int pgr_sha256_file(const char * path, char out_hex[65]);

#ifdef __cplusplus
}
#endif
#endif
