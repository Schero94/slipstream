#include "peregrine_sha256.h"
#include "peregrine_io.h"

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>

#if defined(__APPLE__)
#include <CommonCrypto/CommonDigest.h>
#endif

typedef struct {
    uint32_t state[8];
    uint64_t total;
    unsigned char block[64];
    size_t used;
} pgr_sha256_ctx;

static const uint32_t pgr_sha256_k[64] = {
    0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
    0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
    0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
    0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
    0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
    0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
    0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
    0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2,
};

static uint32_t pgr_rotr(uint32_t x, unsigned n) { return (x >> n) | (x << (32U - n)); }
static uint32_t pgr_be32(const unsigned char * p) {
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | p[3];
}

static void pgr_sha256_transform(pgr_sha256_ctx * ctx, const unsigned char data[64]) {
    uint32_t w[64];
    for (int i = 0; i < 16; ++i) w[i] = pgr_be32(data + 4*i);
    for (int i = 16; i < 64; ++i) {
        uint32_t s0 = pgr_rotr(w[i-15],7) ^ pgr_rotr(w[i-15],18) ^ (w[i-15] >> 3);
        uint32_t s1 = pgr_rotr(w[i-2],17) ^ pgr_rotr(w[i-2],19) ^ (w[i-2] >> 10);
        w[i] = w[i-16] + s0 + w[i-7] + s1;
    }
    uint32_t a=ctx->state[0],b=ctx->state[1],c=ctx->state[2],d=ctx->state[3];
    uint32_t e=ctx->state[4],f=ctx->state[5],g=ctx->state[6],h=ctx->state[7];
    for (int i = 0; i < 64; ++i) {
        uint32_t s1=pgr_rotr(e,6)^pgr_rotr(e,11)^pgr_rotr(e,25);
        uint32_t ch=(e&f)^((~e)&g);
        uint32_t t1=h+s1+ch+pgr_sha256_k[i]+w[i];
        uint32_t s0=pgr_rotr(a,2)^pgr_rotr(a,13)^pgr_rotr(a,22);
        uint32_t maj=(a&b)^(a&c)^(b&c);
        uint32_t t2=s0+maj;
        h=g;g=f;f=e;e=d+t1;d=c;c=b;b=a;a=t1+t2;
    }
    ctx->state[0]+=a;ctx->state[1]+=b;ctx->state[2]+=c;ctx->state[3]+=d;
    ctx->state[4]+=e;ctx->state[5]+=f;ctx->state[6]+=g;ctx->state[7]+=h;
}

static void pgr_sha256_init(pgr_sha256_ctx * ctx) {
    static const uint32_t initial[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
    memset(ctx, 0, sizeof(*ctx)); memcpy(ctx->state, initial, sizeof(initial));
}

static int pgr_sha256_update(pgr_sha256_ctx * ctx, const unsigned char * data, size_t size) {
    if (size > UINT64_MAX - ctx->total) return -1;
    ctx->total += size;
    while (size) {
        size_t take = 64 - ctx->used; if (take > size) take = size;
        memcpy(ctx->block + ctx->used, data, take); ctx->used += take; data += take; size -= take;
        if (ctx->used == 64) { pgr_sha256_transform(ctx, ctx->block); ctx->used = 0; }
    }
    return 0;
}

static void pgr_sha256_final(pgr_sha256_ctx * ctx, char out[65]) {
    const uint64_t bits = ctx->total * 8U;
    ctx->block[ctx->used++] = 0x80;
    if (ctx->used > 56) { while (ctx->used < 64) ctx->block[ctx->used++] = 0; pgr_sha256_transform(ctx, ctx->block); ctx->used = 0; }
    while (ctx->used < 56) ctx->block[ctx->used++] = 0;
    for (int i = 7; i >= 0; --i) ctx->block[ctx->used++] = (unsigned char)(bits >> (8*i));
    pgr_sha256_transform(ctx, ctx->block);
    static const char hex[] = "0123456789abcdef";
    for (int i = 0; i < 8; ++i) for (int j = 0; j < 4; ++j) {
        unsigned char byte = (unsigned char)(ctx->state[i] >> (24 - 8*j));
        out[2*(4*i+j)] = hex[byte >> 4]; out[2*(4*i+j)+1] = hex[byte & 15];
    }
    out[64] = '\0';
}

#if defined(__APPLE__)
static void pgr_sha256_digest_hex(const unsigned char digest[CC_SHA256_DIGEST_LENGTH], char out[65]) {
    static const char hex[] = "0123456789abcdef";
    for (int i = 0; i < CC_SHA256_DIGEST_LENGTH; ++i) {
        out[2*i] = hex[digest[i] >> 4];
        out[2*i + 1] = hex[digest[i] & 15];
    }
    out[64] = '\0';
}
#endif

int pgr_sha256_bytes(const void * data, size_t size, char out_hex[65]) {
    if ((!data && size) || !out_hex || size > UINT64_MAX / 8U) return -1;
#if defined(__APPLE__)
    CC_SHA256_CTX ctx;
    unsigned char digest[CC_SHA256_DIGEST_LENGTH];
    if (CC_SHA256_Init(&ctx) != 1) return -1;
    const unsigned char * cursor = (const unsigned char *) data;
    while (size) {
        const CC_LONG chunk = size > UINT32_MAX ? UINT32_MAX : (CC_LONG) size;
        if (CC_SHA256_Update(&ctx, cursor, chunk) != 1) return -1;
        cursor += chunk;
        size -= chunk;
    }
    if (CC_SHA256_Final(digest, &ctx) != 1) return -1;
    pgr_sha256_digest_hex(digest, out_hex);
#else
    pgr_sha256_ctx ctx; pgr_sha256_init(&ctx);
    if (pgr_sha256_update(&ctx, (const unsigned char *)data, size) != 0) return -1;
    pgr_sha256_final(&ctx, out_hex);
#endif
    return 0;
}

int pgr_sha256_file(const char * path, char out_hex[65]) {
    if (!path || !out_hex) return -1;
    int fd = open(path, O_RDONLY); if (fd < 0) return -1;
    pgr_io_hint_sequential(fd);   /* one pass front to back, then discard */
#if defined(__APPLE__)
    CC_SHA256_CTX ctx;
    if (CC_SHA256_Init(&ctx) != 1) { close(fd); return -1; }
#else
    pgr_sha256_ctx ctx; pgr_sha256_init(&ctx);
#endif
    unsigned char buffer[65536];
    for (;;) {
        ssize_t got = read(fd, buffer, sizeof(buffer));
        if (got < 0 && errno == EINTR) continue;
        if (got < 0) { close(fd); return -1; }
#if defined(__APPLE__)
        if (got > 0 && CC_SHA256_Update(&ctx, buffer, (CC_LONG) got) != 1) { close(fd); return -1; }
#else
        if (got > 0 && pgr_sha256_update(&ctx, buffer, (size_t)got) != 0) { close(fd); return -1; }
#endif
        if (got == 0) break;
    }
    close(fd);
#if defined(__APPLE__)
    unsigned char digest[CC_SHA256_DIGEST_LENGTH];
    if (CC_SHA256_Final(digest, &ctx) != 1) return -1;
    pgr_sha256_digest_hex(digest, out_hex);
#else
    if (ctx.total > UINT64_MAX / 8U) return -1;
    pgr_sha256_final(&ctx, out_hex);
#endif
    return 0;
}
