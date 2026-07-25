/* PGRN v1 reader. Metadata and expert bytes are read with pread; mmap is never used. */
#include "peregrine_pgrn.h"

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#if defined(PGR_HAVE_ZLIB)
#include <zlib.h>
#endif

#define PGRN_ALIGN       16384U
#define PGRN_HEADER_FIXED 16U
#define PGRN_DIR_RECORD    26U
#define PGRN_MAX_EXPERTS 1000000U

#ifndef F_NOCACHE
#define F_NOCACHE 48
#endif

struct pgrn_file {
    int fd;
    uint64_t file_size;
    pgrn_expert_ref * refs;
    pgrn_tensor_layout * layouts;
    size_t count;
    size_t layer_count;
    uint32_t experts_per_layer;
    size_t max_expert_bytes;
    uint64_t total_expert_bytes;
    char model_sha256[65];
    char error[192];
};

static uint16_t pgrn_u16(const unsigned char * p) {
    return (uint16_t)p[0] | ((uint16_t)p[1] << 8);
}

static uint32_t pgrn_u32(const unsigned char * p) {
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) |
        ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

static uint64_t pgrn_u64(const unsigned char * p) {
    uint64_t value = 0;
    for (int i = 7; i >= 0; --i) value = (value << 8) | p[i];
    return value;
}

static void pgrn_set_error(pgrn_file * file, const char * message) {
    if (file) snprintf(file->error, sizeof(file->error), "%s", message ? message : "unknown error");
}

static int pgrn_read_full(int fd, void * dst, size_t size, uint64_t offset) {
    unsigned char * out = (unsigned char *)dst;
    size_t done = 0;
    while (done < size) {
        ssize_t n = pread(fd, out + done, size - done, (off_t)(offset + done));
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        done += (size_t)n;
    }
    return 0;
}

static const unsigned char * pgrn_find_bytes(
        const unsigned char * data, size_t size, const char * needle, size_t needle_size,
        const unsigned char * after) {
    size_t start = after ? (size_t)(after - data) : 0;
    if (needle_size > size || start > size - needle_size) return NULL;
    for (size_t i = start; i <= size - needle_size; ++i) {
        if (memcmp(data + i, needle, needle_size) == 0) return data + i;
    }
    return NULL;
}

/* PGRN's writer emits canonical JSON. These bounded extractors reject duplicate
 * authoritative keys so a crafted metadata object cannot override a prior value. */
static int pgrn_json_u64(
        const unsigned char * json, size_t size, const char * key, uint64_t * out) {
    char token[96];
    int token_len = snprintf(token, sizeof(token), "\"%s\"", key);
    if (token_len <= 0 || (size_t)token_len >= sizeof(token)) return -1;
    const unsigned char * found = pgrn_find_bytes(json, size, token, (size_t)token_len, NULL);
    if (!found) return 0;
    if (pgrn_find_bytes(json, size, token, (size_t)token_len, found + token_len)) return -1;
    const unsigned char * p = found + token_len;
    const unsigned char * end = json + size;
    while (p < end && isspace(*p)) ++p;
    if (p == end || *p++ != ':') return -1;
    while (p < end && isspace(*p)) ++p;
    if (p == end || !isdigit(*p)) return -1;
    uint64_t value = 0;
    do {
        unsigned digit = (unsigned)(*p++ - '0');
        if (value > (UINT64_MAX - digit) / 10U) return -1;
        value = value * 10U + digit;
    } while (p < end && isdigit(*p));
    while (p < end && isspace(*p)) ++p;
    if (p < end && *p != ',' && *p != '}') return -1;
    *out = value;
    return 1;
}

static int pgrn_json_sha256(
        const unsigned char * json, size_t size, char out[65]) {
    static const char token[] = "\"model_sha256\"";
    const unsigned char * found = pgrn_find_bytes(json, size, token, sizeof(token) - 1, NULL);
    if (!found) return 0;
    if (pgrn_find_bytes(json, size, token, sizeof(token) - 1, found + sizeof(token) - 1)) return -1;
    const unsigned char * p = found + sizeof(token) - 1;
    const unsigned char * end = json + size;
    while (p < end && isspace(*p)) ++p;
    if (p == end || *p++ != ':') return -1;
    while (p < end && isspace(*p)) ++p;
    if (p == end || *p++ != '"' || (size_t)(end - p) < 65 || p[64] != '"') return -1;
    for (size_t i = 0; i < 64; ++i) {
        if (!isxdigit(p[i])) return -1;
        out[i] = (char)tolower(p[i]);
    }
    out[64] = '\0';
    return 1;
}

static int pgrn_json_array_u64(const unsigned char ** cursor, const unsigned char * end, uint64_t * out) {
    const unsigned char * p = *cursor;
    while (p < end && isspace(*p)) ++p;
    if (p == end || !isdigit(*p)) return -1;
    uint64_t value = 0;
    do {
        const unsigned digit = (unsigned)(*p++ - '0');
        if (value > (UINT64_MAX - digit) / 10U) return -1;
        value = value * 10U + digit;
    } while (p < end && isdigit(*p));
    *cursor = p;
    *out = value;
    return 0;
}

static int pgrn_layout_cmp(const void * lhs, const void * rhs) {
    const pgrn_tensor_layout * a = (const pgrn_tensor_layout *) lhs;
    const pgrn_tensor_layout * b = (const pgrn_tensor_layout *) rhs;
    return a->layer < b->layer ? -1 : a->layer > b->layer ? 1 : 0;
}

static int pgrn_json_tensor_directory(pgrn_file * file, const unsigned char * json, size_t size) {
    static const char token[] = "\"tensor_directory\"";
    const unsigned char * found = pgrn_find_bytes(json, size, token, sizeof(token) - 1, NULL);
    if (!found || pgrn_find_bytes(json, size, token, sizeof(token) - 1, found + sizeof(token) - 1)) return -1;
    const unsigned char * p = found + sizeof(token) - 1;
    const unsigned char * end = json + size;
    while (p < end && isspace(*p)) ++p;
    if (p == end || *p++ != ':') return -1;
    while (p < end && isspace(*p)) ++p;
    if (p == end || *p++ != '[') return -1;

    size_t capacity = 0;
    while (1) {
        while (p < end && isspace(*p)) ++p;
        if (p < end && *p == ']') { ++p; break; }
        if (p == end || *p++ != '[') return -1;
        uint64_t fields[7];
        for (size_t i = 0; i < 7; ++i) {
            if (pgrn_json_array_u64(&p, end, &fields[i]) != 0) return -1;
            while (p < end && isspace(*p)) ++p;
            const unsigned char expected = i == 6 ? ']' : ',';
            if (p == end || *p++ != expected) return -1;
        }
        if (fields[0] > UINT16_MAX || fields[1] > UINT32_MAX || fields[2] == 0 ||
                fields[3] > UINT32_MAX || fields[4] == 0 || fields[5] > UINT32_MAX || fields[6] == 0) return -1;
        if (file->layer_count == capacity) {
            size_t next = capacity ? capacity * 2 : 8;
            if (next < capacity || next > SIZE_MAX / sizeof(*file->layouts)) return -1;
            void * grown = realloc(file->layouts, next * sizeof(*file->layouts));
            if (!grown) return -1;
            file->layouts = (pgrn_tensor_layout *) grown;
            capacity = next;
        }
        pgrn_tensor_layout * layout = &file->layouts[file->layer_count++];
        layout->layer = (uint16_t) fields[0];
        for (size_t role = 0; role < 3; ++role) {
            layout->ggml_type[role] = (uint32_t) fields[1 + role * 2];
            layout->nbytes[role] = fields[2 + role * 2];
        }
        while (p < end && isspace(*p)) ++p;
        if (p < end && *p == ',') { ++p; continue; }
        if (p < end && *p == ']') { ++p; break; }
        return -1;
    }
    if (file->layer_count == 0) return -1;
    qsort(file->layouts, file->layer_count, sizeof(*file->layouts), pgrn_layout_cmp);
    for (size_t i = 1; i < file->layer_count; ++i) {
        if (file->layouts[i - 1].layer == file->layouts[i].layer) return -1;
    }
    return 0;
}

static uint32_t pgrn_crc32(const unsigned char * data, size_t size) {
#if defined(PGR_HAVE_ZLIB)
    uLong crc = crc32(0L, Z_NULL, 0);
    while (size) {
        const uInt chunk = size > UINT_MAX ? UINT_MAX : (uInt) size;
        crc = crc32(crc, data, chunk);
        data += chunk;
        size -= chunk;
    }
    return (uint32_t) crc;
#else
    uint32_t table[256];
    for (uint32_t value = 0; value < 256; ++value) {
        uint32_t entry = value;
        for (int bit = 0; bit < 8; ++bit) {
            entry = (entry >> 1) ^ (0xedb88320U & (0U - (entry & 1U)));
        }
        table[value] = entry;
    }
    uint32_t crc = 0xffffffffU;
    for (size_t i = 0; i < size; ++i) {
        crc = table[(crc ^ data[i]) & 0xffU] ^ (crc >> 8);
    }
    return crc ^ 0xffffffffU;
#endif
}

static int pgrn_ref_cmp(const void * lhs, const void * rhs) {
    const pgrn_expert_ref * a = (const pgrn_expert_ref *)lhs;
    const pgrn_expert_ref * b = (const pgrn_expert_ref *)rhs;
    if (a->layer != b->layer) return a->layer < b->layer ? -1 : 1;
    if (a->expert != b->expert) return a->expert < b->expert ? -1 : 1;
    return 0;
}

static int pgrn_decode_directory(
        pgrn_file * file, const unsigned char * raw, uint64_t directory_offset) {
    for (size_t i = 0; i < file->count; ++i) {
        const unsigned char * p = raw + i * PGRN_DIR_RECORD;
        pgrn_expert_ref * ref = &file->refs[i];
        ref->layer = pgrn_u16(p + 0);
        ref->expert = pgrn_u16(p + 2);
        ref->precision = p[4];
        ref->flags = p[5];
        uint32_t heat_bits = pgrn_u32(p + 6);
        memcpy(&ref->heat, &heat_bits, sizeof(heat_bits));
        ref->offset = pgrn_u64(p + 10);
        ref->nbytes = pgrn_u32(p + 18);
        ref->crc32 = pgrn_u32(p + 22);
        if (ref->precision > 3 || ref->nbytes == 0 || ref->offset < PGRN_ALIGN ||
                ref->offset % PGRN_ALIGN != 0 || ref->offset > directory_offset ||
                ref->nbytes > directory_offset - ref->offset) {
            pgrn_set_error(file, "invalid or out-of-bounds expert directory record");
            return -1;
        }
        if (ref->nbytes > file->max_expert_bytes) file->max_expert_bytes = ref->nbytes;
        if (file->total_expert_bytes > UINT64_MAX - ref->nbytes) {
            pgrn_set_error(file, "expert byte total overflow");
            return -1;
        }
        file->total_expert_bytes += ref->nbytes;
    }
    qsort(file->refs, file->count, sizeof(*file->refs), pgrn_ref_cmp);
    for (size_t i = 1; i < file->count; ++i) {
        if (pgrn_ref_cmp(&file->refs[i - 1], &file->refs[i]) == 0) {
            pgrn_set_error(file, "duplicate expert key");
            return -1;
        }
    }
    if (file->layer_count == 0 || file->experts_per_layer == 0 ||
            file->layer_count > SIZE_MAX / file->experts_per_layer ||
            file->layer_count * file->experts_per_layer != file->count) {
        pgrn_set_error(file, "tensor directory geometry does not match expert directory");
        return -1;
    }
    size_t ref_index = 0;
    for (size_t i = 0; i < file->layer_count; ++i) {
        const pgrn_tensor_layout * layout = &file->layouts[i];
        if (layout->nbytes[0] > UINT32_MAX || layout->nbytes[1] > UINT32_MAX || layout->nbytes[2] > UINT32_MAX ||
                layout->nbytes[0] > UINT32_MAX - layout->nbytes[1] ||
                layout->nbytes[0] + layout->nbytes[1] > UINT32_MAX - layout->nbytes[2]) {
            pgrn_set_error(file, "tensor directory byte size overflow");
            return -1;
        }
        const uint32_t record_bytes = (uint32_t) (layout->nbytes[0] + layout->nbytes[1] + layout->nbytes[2]);
        for (uint32_t expert = 0; expert < file->experts_per_layer; ++expert, ++ref_index) {
            const pgrn_expert_ref * ref = &file->refs[ref_index];
            if (ref->layer != layout->layer || ref->expert != expert || ref->nbytes != record_bytes) {
                pgrn_set_error(file, "tensor directory does not match expert records");
                return -1;
            }
        }
    }
    return 0;
}

pgrn_file * pgrn_open(const char * path, const char * expected_sha256) {
    if (!path) return NULL;
    pgrn_file * file = (pgrn_file *)calloc(1, sizeof(*file));
    if (!file) return NULL;
    file->fd = -1;
    file->fd = open(path, O_RDONLY);
    if (file->fd < 0) { pgrn_close(file); return NULL; }
    (void)fcntl(file->fd, F_NOCACHE, 1);
    struct stat st;
    if (fstat(file->fd, &st) != 0 || st.st_size < (off_t)PGRN_ALIGN) { pgrn_close(file); return NULL; }
    file->file_size = (uint64_t)st.st_size;

    unsigned char fixed[PGRN_HEADER_FIXED];
    if (pgrn_read_full(file->fd, fixed, sizeof(fixed), 0) != 0 ||
            memcmp(fixed, "PGRN1\0\0\0", 8) != 0 || pgrn_u32(fixed + 8) != 1) {
        pgrn_close(file); return NULL;
    }
    uint32_t json_len = pgrn_u32(fixed + 12);
    if (json_len == 0 || json_len > PGRN_ALIGN - PGRN_HEADER_FIXED) { pgrn_close(file); return NULL; }
    unsigned char * json = (unsigned char *)malloc((size_t)json_len + 1);
    if (!json) { pgrn_close(file); return NULL; }
    if (pgrn_read_full(file->fd, json, json_len, PGRN_HEADER_FIXED) != 0 || memchr(json, 0, json_len)) {
        free(json); pgrn_close(file); return NULL;
    }
    json[json_len] = 0;

    uint64_t count = 0, directory_offset = 0, experts_per_layer = 0;
    int count_ok = pgrn_json_u64(json, json_len, "expert_count", &count);
    int offset_ok = pgrn_json_u64(json, json_len, "expert_dir_offset", &directory_offset);
    int sha_ok = pgrn_json_sha256(json, json_len, file->model_sha256);
    int experts_ok = pgrn_json_u64(json, json_len, "experts_per_layer", &experts_per_layer);
    int layouts_ok = pgrn_json_tensor_directory(file, json, json_len);
    free(json);
    if (count_ok != 1 || offset_ok != 1 || sha_ok != 1 || experts_ok != 1 || layouts_ok != 0 ||
            count == 0 || count > PGRN_MAX_EXPERTS || experts_per_layer == 0 || experts_per_layer > UINT32_MAX ||
            directory_offset < PGRN_ALIGN || directory_offset % PGRN_ALIGN != 0) {
        pgrn_close(file); return NULL;
    }
    file->experts_per_layer = (uint32_t) experts_per_layer;
    if (expected_sha256 && expected_sha256[0]) {
        if (strlen(expected_sha256) != 64) { pgrn_close(file); return NULL; }
        for (size_t i = 0; i < 64; ++i) {
            if (tolower((unsigned char)expected_sha256[i]) != file->model_sha256[i]) {
                pgrn_close(file); return NULL;
            }
        }
    }
    if (count > SIZE_MAX / PGRN_DIR_RECORD) { pgrn_close(file); return NULL; }
    size_t directory_bytes = (size_t)count * PGRN_DIR_RECORD;
    if (directory_offset > file->file_size || directory_bytes > file->file_size - directory_offset) {
        pgrn_close(file); return NULL;
    }
    unsigned char * raw = (unsigned char *)malloc(directory_bytes);
    file->refs = (pgrn_expert_ref *)calloc((size_t)count, sizeof(*file->refs));
    file->count = (size_t)count;
    if (!raw || !file->refs || pgrn_read_full(file->fd, raw, directory_bytes, directory_offset) != 0 ||
            pgrn_decode_directory(file, raw, directory_offset) != 0) {
        free(raw); pgrn_close(file); return NULL;
    }
    free(raw);
    return file;
}

int pgrn_find(const pgrn_file * file, uint16_t layer, uint16_t expert, pgrn_expert_ref * out) {
    if (!file || !out) return 0;
    size_t lo = 0, hi = file->count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        const pgrn_expert_ref * ref = &file->refs[mid];
        if (ref->layer < layer || (ref->layer == layer && ref->expert < expert)) lo = mid + 1;
        else hi = mid;
    }
    if (lo == file->count || file->refs[lo].layer != layer || file->refs[lo].expert != expert) return 0;
    *out = file->refs[lo];
    return 1;
}

int pgrn_find_layout(const pgrn_file * file, uint16_t layer, pgrn_tensor_layout * out) {
    if (!file || !out) return 0;
    size_t lo = 0, hi = file->layer_count;
    while (lo < hi) {
        const size_t mid = lo + (hi - lo) / 2;
        if (file->layouts[mid].layer < layer) lo = mid + 1;
        else hi = mid;
    }
    if (lo == file->layer_count || file->layouts[lo].layer != layer) return 0;
    *out = file->layouts[lo];
    return 1;
}

int pgrn_read_expert(pgrn_file * file, const pgrn_expert_ref * ref, void * dst, size_t dst_size) {
    if (!file || !ref || !dst || dst_size < ref->nbytes) {
        pgrn_set_error(file, "invalid expert read buffer"); return -1;
    }
    if (ref->offset > file->file_size || ref->nbytes > file->file_size - ref->offset ||
            pgrn_read_full(file->fd, dst, ref->nbytes, ref->offset) != 0) {
        pgrn_set_error(file, "short expert read"); return -1;
    }
    if (pgrn_crc32((const unsigned char *)dst, ref->nbytes) != ref->crc32) {
        pgrn_set_error(file, "expert CRC mismatch"); return -1;
    }
    file->error[0] = '\0';
    return 0;
}

int pgrn_read_expert_mt(const pgrn_file * file, const pgrn_expert_ref * ref,
                        void * dst, size_t dst_size, char * err, size_t err_capacity) {
    if (err && err_capacity) err[0] = '\0';
    if (!file || !ref || !dst || dst_size < ref->nbytes) {
        if (err && err_capacity) snprintf(err, err_capacity, "invalid expert read buffer");
        return -1;
    }
    /* Read-only file state + pread(offset) + a caller-owned error buffer: no shared
     * mutable state is written, so concurrent callers with distinct dst never race. */
    if (ref->offset > file->file_size || ref->nbytes > file->file_size - ref->offset ||
            pgrn_read_full(file->fd, dst, ref->nbytes, ref->offset) != 0) {
        if (err && err_capacity) snprintf(err, err_capacity, "short expert read");
        return -1;
    }
    if (pgrn_crc32((const unsigned char *)dst, ref->nbytes) != ref->crc32) {
        if (err && err_capacity) snprintf(err, err_capacity, "expert CRC mismatch");
        return -1;
    }
    return 0;
}

size_t pgrn_count(const pgrn_file * file) { return file ? file->count : 0; }
size_t pgrn_layer_count(const pgrn_file * file) { return file ? file->layer_count : 0; }
int pgrn_layer_at(const pgrn_file * file, size_t index, uint16_t * layer) {
    if (!file || !layer || index >= file->layer_count) return 0;
    *layer = file->layouts[index].layer;
    return 1;
}
uint32_t pgrn_experts_per_layer(const pgrn_file * file) { return file ? file->experts_per_layer : 0; }
size_t pgrn_max_expert_bytes(const pgrn_file * file) { return file ? file->max_expert_bytes : 0; }
uint64_t pgrn_total_expert_bytes(const pgrn_file * file) { return file ? file->total_expert_bytes : 0; }
const char *pgrn_model_sha256(const pgrn_file * file) { return file ? file->model_sha256 : ""; }
const char *pgrn_error(const pgrn_file * file) { return file ? file->error : "invalid PGRN file"; }

void pgrn_close(pgrn_file * file) {
    if (!file) return;
    if (file->fd >= 0) close(file->fd);
    free(file->refs);
    free(file->layouts);
    free(file);
}
