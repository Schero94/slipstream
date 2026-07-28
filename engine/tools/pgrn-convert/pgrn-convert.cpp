// pgrn-convert: extract stacked GGUF MoE expert tensors into a PGRN sidecar.
//
// Native port of bench/m1/convert_gguf_to_pgrn.py. The Python converter is the
// reference implementation; this tool must produce a byte-identical output file
// (same header JSON bytes, same record order/padding, same directory) so the
// parity gate can compare SHA-256 of both outputs.
//
// Progress protocol (--progress jsonl): one JSON line per >=1% step on stdout,
// phases: resume -> sha256 -> write -> verify -> done (or cancelled/error).
//
// Cancellation via SIGINT/SIGTERM keeps the .partial output plus its journal and
// exits 2 with {"phase":"cancelled","resumable":true}; --resume then continues
// from the last committed chunk. With --no-journal the old behaviour applies and
// the .partial is removed instead.

#include "ggml.h"
#include "gguf.h"

#include <algorithm>
#include <cerrno>
#include <cinttypes>
#include <csignal>
#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <atomic>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <strings.h>

#include <fcntl.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <unistd.h>

#include "peregrine_io.h"

#ifdef __APPLE__
// Apple's SHA-256 uses the ARMv8 SHA-2 instructions. Measured on an M3 Pro:
// 3073 MB/s vs 366 MB/s for the portable fallback below, bit-identical digests.
// The hashing phase reads the whole source, so on a 22.8 GB GGUF this turns a
// CPU-bound 87 s into a disk-bound one — and a resume pays it again every time.
#include <CommonCrypto/CommonDigest.h>
#endif

#define PGRN_ALIGN 16384ull

// ---------- small vendored helpers (deterministic, dependency-free) ----------

// zlib-compatible CRC32 (reflected polynomial 0xEDB88320)
static uint32_t crc32_table[256];
static void crc32_init(void) {
    for (uint32_t i = 0; i < 256; ++i) {
        uint32_t c = i;
        for (int k = 0; k < 8; ++k) c = (c & 1) ? 0xEDB88320u ^ (c >> 1) : c >> 1;
        crc32_table[i] = c;
    }
}
static uint32_t crc32_update(uint32_t crc, const uint8_t * p, size_t n) {
    crc ^= 0xFFFFFFFFu;
    for (size_t i = 0; i < n; ++i) crc = crc32_table[(crc ^ p[i]) & 0xFF] ^ (crc >> 8);
    return crc ^ 0xFFFFFFFFu;
}

// SHA-256 (FIPS 180-4), minimal streaming implementation — the portable
// fallback, and the reference for what the accelerated path must reproduce.
struct sha256_ctx { uint32_t h[8]; uint64_t len; uint8_t buf[64]; size_t buflen; };
static const uint32_t sha256_k[64] = {
    0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
    0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
    0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
    0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
    0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
    0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
    0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
    0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2,
};
static inline uint32_t rotr32(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }
static void sha256_init(sha256_ctx * c) {
    static const uint32_t h0[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
    memcpy(c->h, h0, sizeof(h0)); c->len = 0; c->buflen = 0;
}
static void sha256_block(sha256_ctx * c, const uint8_t * p) {
    uint32_t w[64];
    for (int i = 0; i < 16; ++i)
        w[i] = ((uint32_t)p[4*i] << 24) | ((uint32_t)p[4*i+1] << 16) | ((uint32_t)p[4*i+2] << 8) | p[4*i+3];
    for (int i = 16; i < 64; ++i) {
        uint32_t s0 = rotr32(w[i-15],7) ^ rotr32(w[i-15],18) ^ (w[i-15] >> 3);
        uint32_t s1 = rotr32(w[i-2],17) ^ rotr32(w[i-2],19) ^ (w[i-2] >> 10);
        w[i] = w[i-16] + s0 + w[i-7] + s1;
    }
    uint32_t a=c->h[0],b=c->h[1],d=c->h[3],e=c->h[4],f=c->h[5],g=c->h[6],h=c->h[7],cc=c->h[2];
    for (int i = 0; i < 64; ++i) {
        uint32_t S1 = rotr32(e,6) ^ rotr32(e,11) ^ rotr32(e,25);
        uint32_t ch = (e & f) ^ (~e & g);
        uint32_t t1 = h + S1 + ch + sha256_k[i] + w[i];
        uint32_t S0 = rotr32(a,2) ^ rotr32(a,13) ^ rotr32(a,22);
        uint32_t mj = (a & b) ^ (a & cc) ^ (b & cc);
        uint32_t t2 = S0 + mj;
        h=g; g=f; f=e; e=d+t1; d=cc; cc=b; b=a; a=t1+t2;
    }
    c->h[0]+=a; c->h[1]+=b; c->h[2]+=cc; c->h[3]+=d; c->h[4]+=e; c->h[5]+=f; c->h[6]+=g; c->h[7]+=h;
}
static void sha256_update(sha256_ctx * c, const void * data, size_t n) {
    const uint8_t * p = (const uint8_t *)data;
    c->len += n;
    if (c->buflen) {
        while (n && c->buflen < 64) { c->buf[c->buflen++] = *p++; --n; }
        if (c->buflen == 64) { sha256_block(c, c->buf); c->buflen = 0; }
    }
    while (n >= 64) { sha256_block(c, p); p += 64; n -= 64; }
    while (n--) c->buf[c->buflen++] = *p++;
}
static std::string sha256_final_hex(sha256_ctx * c) {
    uint64_t bits = c->len * 8;
    uint8_t pad = 0x80;
    sha256_update(c, &pad, 1);
    uint8_t zero = 0;
    while (c->buflen != 56) sha256_update(c, &zero, 1);
    uint8_t lenb[8];
    for (int i = 0; i < 8; ++i) lenb[i] = (uint8_t)(bits >> (56 - 8*i));
    sha256_update(c, lenb, 8);
    char hex[65];
    for (int i = 0; i < 8; ++i) snprintf(hex + 8*i, 9, "%08x", c->h[i]);
    return std::string(hex, 64);
}

// One SHA-256 stream, hardware-accelerated where available. Same algorithm, so
// the digest — and therefore the PGRN header and the parity gate — is unchanged.
struct sha256_stream {
#ifdef __APPLE__
    CC_SHA256_CTX cc;
    sha256_stream() { CC_SHA256_Init(&cc); }
    void update(const void * data, size_t n) {
        const uint8_t * p = (const uint8_t *)data;
        while (n) {                                  // CC_LONG is 32-bit
            const CC_LONG take = (CC_LONG)std::min<size_t>(n, 1u << 30);
            CC_SHA256_Update(&cc, p, take);
            p += take; n -= take;
        }
    }
    std::string final_hex() {
        uint8_t d[CC_SHA256_DIGEST_LENGTH];
        CC_SHA256_Final(d, &cc);
        char hex[2 * CC_SHA256_DIGEST_LENGTH + 1];
        for (int i = 0; i < CC_SHA256_DIGEST_LENGTH; ++i) snprintf(hex + 2*i, 3, "%02x", d[i]);
        return std::string(hex, 2 * CC_SHA256_DIGEST_LENGTH);
    }
#else
    sha256_ctx ctx;
    sha256_stream() { sha256_init(&ctx); }
    void update(const void * data, size_t n) { sha256_update(&ctx, data, n); }
    std::string final_hex() { return sha256_final_hex(&ctx); }
#endif
};

// ---------- progress + errors ----------

static bool g_progress = false;
static std::atomic<bool> g_cancel{false};
static void on_signal(int) { g_cancel.store(true); }

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

struct phase_reporter {
    const char * phase;
    uint64_t total;
    double t0;
    uint64_t last_emit = 0;
    explicit phase_reporter(const char * ph, uint64_t total_bytes) : phase(ph), total(total_bytes), t0(now_s()) {}
    void emit(uint64_t done, const char * extra_key = nullptr, uint64_t extra_val = 0, uint64_t extra_total = 0) {
        if (!g_progress) return;
        // emit at >=1% steps (and always at completion)
        if (done < total && done - last_emit < total / 100 + 1) return;
        last_emit = done;
        double dt = now_s() - t0;
        double mbs = dt > 0.0 ? (double)done / dt / 1e6 : 0.0;
        double eta = (mbs > 0.0 && total > done) ? (double)(total - done) / (mbs * 1e6) : 0.0;
        if (extra_key) {
            printf("{\"phase\":\"%s\",\"%s\":%" PRIu64 ",\"%s_total\":%" PRIu64
                   ",\"done_bytes\":%" PRIu64 ",\"total_bytes\":%" PRIu64 ",\"mb_s\":%.0f,\"eta_s\":%.0f}\n",
                   phase, extra_key, extra_val, extra_key, extra_total, done, total, mbs, eta);
        } else {
            printf("{\"phase\":\"%s\",\"done_bytes\":%" PRIu64 ",\"total_bytes\":%" PRIu64
                   ",\"mb_s\":%.0f,\"eta_s\":%.0f}\n", phase, done, total, mbs, eta);
        }
        fflush(stdout);
    }
};

static int fail(const char * fmt, ...) {
    char msg[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(msg, sizeof(msg), fmt, ap);
    va_end(ap);
    if (g_progress) {
        printf("{\"phase\":\"error\",\"message\":\"%s\"}\n", msg);
        fflush(stdout);
    }
    fprintf(stderr, "pgrn-convert: %s\n", msg);
    return 1;
}

// ---------- GGUF expert layout ----------

struct role_t { uint32_t ggml_type; uint64_t nbytes_total; uint64_t rel_offset; int shard; };
struct layer_t { role_t roles[3]; bool have[3] = {false,false,false}; };

// One mapped GGUF file. Sharded models (llama.cpp gguf-split) carry their expert
// tensors spread over shards; each tensor lives wholly inside one shard.
struct shard_t {
    std::string path;
    struct gguf_context * gctx = nullptr;
    struct ggml_context * ctx_meta = nullptr;
    uint64_t data_offset = 0;
    uint64_t size = 0;
    int fd = -1;
};

static int read_int_kv(struct gguf_context * g, const char * key, int fallback) {
    const int64_t kid = gguf_find_key(g, key);
    if (kid < 0) return fallback;
    switch (gguf_get_kv_type(g, kid)) {
        case GGUF_TYPE_UINT16: return (int) gguf_get_val_u16(g, kid);
        case GGUF_TYPE_INT16:  return (int) gguf_get_val_i16(g, kid);
        case GGUF_TYPE_UINT32: return (int) gguf_get_val_u32(g, kid);
        case GGUF_TYPE_INT32:  return (int) gguf_get_val_i32(g, kid);
        default: return fallback;
    }
}
static const char * ROLE_NAMES[3] = {"gate", "up", "down"};

static bool parse_expert_name(const char * name, int * layer, int * role) {
    // blk.<N>.ffn_(gate|up|down)_exps.weight
    if (strncmp(name, "blk.", 4) != 0) return false;
    char * end = nullptr;
    long ln = strtol(name + 4, &end, 10);
    if (end == name + 4 || ln < 0 || *end != '.') return false;
    static const char * pat[3] = {"ffn_gate_exps.weight", "ffn_up_exps.weight", "ffn_down_exps.weight"};
    for (int r = 0; r < 3; ++r) {
        if (strcmp(end + 1, pat[r]) == 0) { *layer = (int)ln; *role = r; return true; }
    }
    return false;
}

static int pread_full(int fd, void * dst, size_t size, uint64_t off) {
    uint8_t * out = (uint8_t *)dst;
    size_t done = 0;
    while (done < size) {
        ssize_t n = pread(fd, out + done, size - done, (off_t)(off + done));
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        done += (size_t)n;
    }
    return 0;
}

static int write_full(int fd, const void * src, size_t size) {
    const uint8_t * in = (const uint8_t *)src;
    size_t done = 0;
    while (done < size) {
        ssize_t n = write(fd, in + done, size - done);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) return -1;
        done += (size_t)n;
    }
    return 0;
}

// 26-byte little-endian directory record: HHBBfQII
static void pack_dir_record(uint8_t * p, uint16_t layer, uint16_t expert, uint8_t precision,
                            uint8_t flags, float heat, uint64_t offset, uint32_t nbytes, uint32_t crc) {
    p[0] = (uint8_t)layer; p[1] = (uint8_t)(layer >> 8);
    p[2] = (uint8_t)expert; p[3] = (uint8_t)(expert >> 8);
    p[4] = precision; p[5] = flags;
    uint32_t hb; memcpy(&hb, &heat, 4);
    p[6] = (uint8_t)hb; p[7] = (uint8_t)(hb >> 8); p[8] = (uint8_t)(hb >> 16); p[9] = (uint8_t)(hb >> 24);
    for (int i = 0; i < 8; ++i) p[10 + i] = (uint8_t)(offset >> (8 * i));
    for (int i = 0; i < 4; ++i) p[18 + i] = (uint8_t)(nbytes >> (8 * i));
    for (int i = 0; i < 4; ++i) p[22 + i] = (uint8_t)(crc >> (8 * i));
}

static uint64_t align_up(uint64_t v) { return (v + PGRN_ALIGN - 1) / PGRN_ALIGN * PGRN_ALIGN; }

// ---------- resume journal ----------
//
// A .partial output carries no directory (that is written last), so on its own it
// cannot say how much of it is valid. The journal sidecar supplies exactly that:
// a header pinning the source and geometry, followed by the 26-byte directory
// records of every committed chunk in write order.
//
// Ordering guarantee: the payload chunk is fsynced BEFORE its records are
// appended and the journal is fsynced after. The journal can therefore only lag
// behind the data, never lead it — so its record count is always a boundary that
// the file has actually reached, and resume replays at most one chunk.
//
// Because the record offsets follow deterministically from the GGUF geometry, the
// prefix of a cancelled run is bit-identical to what an uninterrupted run would
// have written. The stored CRCs let resume prove that instead of assuming it.

#define PGRNJ_MAGIC         "PGRNJRN1"
#define PGRNJ_HEADER_BYTES  120
#define PGRN_DIR_RECORD     26

struct journal_header {
    char     source_sha[65] = {0};   // 64 hex chars + NUL
    uint64_t source_size    = 0;
    uint64_t expert_count   = 0;
    uint64_t n_layers       = 0;
    uint64_t padded_payload = 0;
    uint32_t geometry_crc   = 0;     // CRC32 over the per-layer record sizes, in layer order
};

static void put_u64(uint8_t * p, uint64_t v) { for (int i = 0; i < 8; ++i) p[i] = (uint8_t)(v >> (8 * i)); }
static void put_u32(uint8_t * p, uint32_t v) { for (int i = 0; i < 4; ++i) p[i] = (uint8_t)(v >> (8 * i)); }
static uint64_t get_u64(const uint8_t * p) {
    uint64_t v = 0;
    for (int i = 0; i < 8; ++i) v |= (uint64_t)p[i] << (8 * i);
    return v;
}
static uint32_t get_u32(const uint8_t * p) {
    uint32_t v = 0;
    for (int i = 0; i < 4; ++i) v |= (uint32_t)p[i] << (8 * i);
    return v;
}

static void journal_pack_header(uint8_t * blk, const journal_header & h) {
    memset(blk, 0, PGRNJ_HEADER_BYTES);
    memcpy(blk, PGRNJ_MAGIC, 8);
    put_u32(blk + 8, 1);                       // version
    memcpy(blk + 16, h.source_sha, 64);
    put_u64(blk + 80,  h.source_size);
    put_u64(blk + 88,  h.expert_count);
    put_u64(blk + 96,  h.n_layers);
    put_u64(blk + 104, h.padded_payload);
    put_u32(blk + 112, h.geometry_crc);
    put_u32(blk + 116, crc32_update(0, blk, 116));
}

// Returns nullptr on success, else a human-readable reason why this journal does
// not describe the conversion we are about to run.
static const char * journal_check_header(const uint8_t * blk, const journal_header & want) {
    if (memcmp(blk, PGRNJ_MAGIC, 8) != 0)              return "not a pgrn-convert journal";
    if (get_u32(blk + 8) != 1)                          return "unsupported journal version";
    if (get_u32(blk + 116) != crc32_update(0, blk, 116)) return "journal header is corrupt";
    if (memcmp(blk + 16, want.source_sha, 64) != 0)     return "journal belongs to a different source (SHA-256 differs)";
    if (get_u64(blk + 80)  != want.source_size)         return "source size differs from the journal";
    if (get_u64(blk + 88)  != want.expert_count)        return "expert count differs from the journal";
    if (get_u64(blk + 96)  != want.n_layers)            return "layer count differs from the journal";
    if (get_u64(blk + 104) != want.padded_payload)      return "payload size differs from the journal";
    if (get_u32(blk + 112) != want.geometry_crc)        return "record geometry differs from the journal";
    return nullptr;
}

// Padded payload bytes covered by the first `records` records, walking layers in
// write order. This is what the .partial must be truncated to on resume.
static uint64_t prefix_payload_bytes(const std::map<int, uint64_t> & record_bytes,
                                     uint64_t expert_count, uint64_t records) {
    uint64_t bytes = 0, seen = 0;
    for (const auto & kv : record_bytes) {
        if (seen >= records) break;
        const uint64_t take = std::min<uint64_t>(expert_count, records - seen);
        bytes += align_up(kv.second) * take;
        seen  += take;
    }
    return bytes;
}

// ---------- CRC sweep ----------
//
// Two phases re-read written records and check them against their stored CRC:
// `verify` over the whole file at the end, and `resume` over the prefix a
// cancelled run left behind. Both are the same operation, and each record is
// independent of every other — so it parallelises over the io threads. Measured
// on an M3 Pro, one thread sustains ~340 MB/s while four reach the ~1.2 GB/s the
// write phase already gets out of the same device.

struct sweep_result {
    uint64_t bad_record = 0;    // lowest record whose bytes disagree, else `count`
    bool     io_error   = false;
    bool     cancelled  = false;
};

// Checks records [0, count) of `records` against the payload in `fd`. Reports the
// *lowest* disagreeing record, not merely the first one noticed, so the outcome
// does not depend on thread scheduling: `verify` names a reproducible record in
// its error, and `resume` gets the exact point from which everything must be
// rewritten.
static sweep_result crc_sweep(int fd, const uint8_t * records, uint64_t count,
                              int nthreads, phase_reporter & rep) {
    sweep_result res;
    res.bad_record = count;
    if (count == 0) return res;

    std::atomic<uint64_t> next{0};
    std::atomic<uint64_t> bad{count};
    std::atomic<uint64_t> done{0};
    std::atomic<bool>     io_err{false};
    std::mutex            emit_mu;

    auto worker = [&]() {
        std::vector<uint8_t> buf;
        for (;;) {
            const uint64_t i = next.fetch_add(1);
            // Records at or above a known mismatch are pointless: both callers
            // discard everything from that point on. Lower ones were already
            // handed out (fetch_add is monotonic), so the minimum is still found.
            if (i >= count || i >= bad.load() || io_err.load() || g_cancel.load()) return;

            const uint8_t * p = records + (size_t)(i * PGRN_DIR_RECORD);
            const uint64_t  off    = get_u64(p + 10);
            const uint32_t  nbytes = get_u32(p + 18);
            const uint32_t  want   = get_u32(p + 22);

            buf.resize(nbytes);
            if (pread_full(fd, buf.data(), nbytes, off) != 0) { io_err.store(true); return; }
            if (crc32_update(0, buf.data(), nbytes) != want) {
                uint64_t cur = bad.load();
                while (i < cur && !bad.compare_exchange_weak(cur, i)) { }
                return;
            }

            done.fetch_add(align_up(nbytes));
            // Emit the running total, not this thread's snapshot of it, so
            // progress stays monotonic however the threads interleave.
            std::lock_guard<std::mutex> lk(emit_mu);
            rep.emit(done.load());
        }
    };

    std::vector<std::thread> pool;
    const int n = (int)std::min<uint64_t>((uint64_t)std::max(nthreads, 1), count);
    pool.reserve(n);
    for (int t = 0; t < n; ++t) pool.emplace_back(worker);
    for (auto & th : pool) th.join();

    res.bad_record = bad.load();
    res.io_error   = io_err.load();
    res.cancelled  = g_cancel.load();
    return res;
}

int main(int argc, char ** argv) {
    const char * input = nullptr;
    const char * output = nullptr;
    const char * expect_sha = nullptr;
    double min_free_gb = 16.0;
    int io_threads = (int)std::thread::hardware_concurrency() / 2;
    if (io_threads < 1) io_threads = 1;
    if (io_threads > 16) io_threads = 16;
    bool dry_run = false;
    bool verify = true;
    bool resume = false;
    bool journal_enabled = true;

    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--input")   && i + 1 < argc) input  = argv[++i];
        else if (!strcmp(argv[i], "--output") && i + 1 < argc) output = argv[++i];
        else if (!strcmp(argv[i], "--expect-sha256") && i + 1 < argc) expect_sha = argv[++i];
        else if (!strcmp(argv[i], "--min-free-gb") && i + 1 < argc) min_free_gb = atof(argv[++i]);
        else if (!strcmp(argv[i], "--io-threads") && i + 1 < argc) io_threads = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--progress") && i + 1 < argc) g_progress = !strcmp(argv[++i], "jsonl");
        else if (!strcmp(argv[i], "--dry-run")) dry_run = true;
        else if (!strcmp(argv[i], "--no-verify")) verify = false;
        else if (!strcmp(argv[i], "--resume")) resume = true;
        else if (!strcmp(argv[i], "--no-journal")) journal_enabled = false;
        else if (!strcmp(argv[i], "--help")) {
            printf("usage: %s --input model.gguf --output model.pgrn [--expect-sha256 HEX]\n"
                   "          [--min-free-gb N=16] [--io-threads N] [--progress jsonl]\n"
                   "          [--dry-run] [--no-verify] [--resume] [--no-journal]\n"
                   "\n"
                   "  --resume      continue an interrupted conversion from its .partial +\n"
                   "                .partial.journal, re-checking the CRC of everything\n"
                   "                already written before appending\n"
                   "  --no-journal  do not keep a resume journal; a cancelled run then\n"
                   "                deletes its .partial as before\n"
                   "\n"
                   "exit codes: 0 done, 1 error, 2 cancelled (resumable)\n", argv[0]);
            return 0;
        } else {
            return fail("unknown or incomplete argument: %s", argv[i]);
        }
    }
    if (!input) return fail("--input is required");
    if (!output && !dry_run) return fail("--output is required unless --dry-run");
    if (resume && !journal_enabled) return fail("--resume needs the journal — drop --no-journal");
    if (io_threads < 1) io_threads = 1;

    crc32_init();
    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);

    // --- read GGUF metadata + tensor directory (no tensor payload) ---
    // Sharded models: pass the first shard ("-00001-of-NNNNN.gguf"); the
    // remaining shards are derived, opened, and scanned for expert tensors.
    std::vector<shard_t> shards;
    {
        shard_t first;
        first.path = input;
        struct gguf_init_params gparams = { /*.no_alloc =*/ true, /*.ctx =*/ &first.ctx_meta };
        first.gctx = gguf_init_from_file(input, gparams);
        if (!first.gctx) return fail("failed to read GGUF: %s", input);
        shards.push_back(first);

        const int split_count = read_int_kv(first.gctx, "split.count", 1);
        if (split_count > 1) {
            if (read_int_kv(first.gctx, "split.no", 0) != 0) {
                return fail("sharded model: pass the FIRST shard (-00001-of-%05d.gguf)", split_count);
            }
            // input must end in "-%05d-of-%05d.gguf" (20 chars)
            const std::string base(input);
            if (base.size() < 20) return fail("sharded model has an unexpected filename: %s", input);
            const std::string tail = base.substr(base.size() - 20);
            int no = 0, of = 0;
            if (sscanf(tail.c_str(), "-%5d-of-%5d.gguf", &no, &of) != 2 || of != split_count) {
                return fail("sharded model filename does not match '-NNNNN-of-NNNNN.gguf': %s", input);
            }
            const std::string prefix = base.substr(0, base.size() - 20);
            for (int s = 1; s < split_count; ++s) {
                char name[4096];
                snprintf(name, sizeof(name), "%s-%05d-of-%05d.gguf", prefix.c_str(), s + 1, split_count);
                shard_t sh;
                sh.path = name;
                struct gguf_init_params sp = { /*.no_alloc =*/ true, /*.ctx =*/ &sh.ctx_meta };
                sh.gctx = gguf_init_from_file(name, sp);
                if (!sh.gctx) return fail("missing shard %d/%d: %s", s + 1, split_count, name);
                shards.push_back(sh);
            }
        }
    }
    const int n_shards = (int) shards.size();
    uint64_t source_size = 0;
    for (auto & sh : shards) {
        sh.data_offset = gguf_get_data_offset(sh.gctx);
        struct stat st;
        if (stat(sh.path.c_str(), &st) != 0) return fail("stat failed: %s", sh.path.c_str());
        sh.size = (uint64_t) st.st_size;
        source_size += sh.size;
    }

    uint64_t expert_count = 0;
    {
        struct gguf_context * g = shards[0].gctx;
        const int n_kv = (int)gguf_get_n_kv(g);
        for (int i = 0; i < n_kv; ++i) {
            const char * key = gguf_get_key(g, i);
            const size_t klen = strlen(key);
            static const char * suffix = ".expert_count";
            const size_t slen = strlen(suffix);
            if (klen > slen && strcmp(key + klen - slen, suffix) == 0) {
                switch (gguf_get_kv_type(g, i)) {
                    case GGUF_TYPE_UINT32: expert_count = gguf_get_val_u32(g, i); break;
                    case GGUF_TYPE_INT32:  expert_count = (uint64_t)gguf_get_val_i32(g, i); break;
                    case GGUF_TYPE_UINT64: expert_count = gguf_get_val_u64(g, i); break;
                    default: break;
                }
                if (expert_count) break;
            }
        }
    }
    if (!expert_count) return fail("GGUF metadata has no authoritative expert_count");

    std::map<int, layer_t> by_layer;
    for (int s = 0; s < n_shards; ++s) {
        struct gguf_context * g = shards[s].gctx;
        const int n_tensors = (int)gguf_get_n_tensors(g);
        for (int i = 0; i < n_tensors; ++i) {
            const char * name = gguf_get_tensor_name(g, i);
            int layer = -1, role = -1;
            if (!parse_expert_name(name, &layer, &role)) continue;
            struct ggml_tensor * t = ggml_get_tensor(shards[s].ctx_meta, name);
            if (!t) return fail("tensor %s missing from ggml context", name);
            const int nd = ggml_n_dims(t);
            if (nd < 1 || (uint64_t)t->ne[nd - 1] != expert_count) {
                return fail("tensor %s is not stacked by expert_count=%" PRIu64, name, expert_count);
            }
            const uint32_t type = (uint32_t)t->type;
            // F32, F16, Q4_K, Q5_K, Q6_K — byte-preserving, never transcoded
            if (!(type == 0 || type == 1 || type == 12 || type == 13 || type == 14)) {
                return fail("unsupported expert GGML type %u in %s", type, name);
            }
            const uint64_t nbytes = (uint64_t)ggml_nbytes(t);
            if (nbytes % expert_count) return fail("tensor %s not evenly stacked", name);
            layer_t & L = by_layer[layer];
            if (L.have[role]) return fail("duplicate expert tensor %s across shards", name);
            L.roles[role] = { type, nbytes, gguf_get_tensor_offset(g, i), s };
            L.have[role] = true;
        }
    }
    if (by_layer.empty()) return fail("GGUF contains no supported stacked expert tensors");
    for (auto & kv : by_layer) {
        if (!kv.second.have[0] || !kv.second.have[1] || !kv.second.have[2]) {
            return fail("layer %d does not have gate/up/down expert tensors", kv.first);
        }
    }

    const uint64_t n_layers = by_layer.size();
    const uint64_t total_experts = n_layers * expert_count;

    // per-layer record bytes (gate+up+down per expert) + padded payload estimate
    std::map<int, uint64_t> record_bytes;
    uint64_t padded_payload = 0;
    for (auto & kv : by_layer) {
        uint64_t rec = 0;
        for (int r = 0; r < 3; ++r) rec += kv.second.roles[r].nbytes_total / expert_count;
        record_bytes[kv.first] = rec;
        padded_payload += align_up(rec) * expert_count;
    }
    const uint64_t estimated_output = PGRN_ALIGN + padded_payload + total_experts * PGRN_DIR_RECORD;

    if (dry_run) {
        printf("{\"source\":\"%s\",\"source_size\":%" PRIu64 ",\"source_shards\":%d,\"layers_with_experts\":%" PRIu64
               ",\"experts_per_layer\":%" PRIu64 ",\"expert_count\":%" PRIu64
               ",\"estimated_output_bytes\":%" PRIu64 "}\n",
               input, source_size, n_shards, n_layers, expert_count, total_experts, estimated_output);
        for (auto & sh : shards) { gguf_free(sh.gctx); ggml_free(sh.ctx_meta); }
        return 0;
    }

    const std::string partial      = std::string(output) + ".partial";
    const std::string journal_path  = partial + ".journal";

    // --- disk admission (fail-closed before the first write) ---
    {
        std::string dir(output);
        size_t slash = dir.find_last_of('/');
        dir = slash == std::string::npos ? "." : dir.substr(0, slash);
        struct statvfs vfs;
        if (statvfs(dir.c_str(), &vfs) != 0) return fail("statvfs failed for %s", dir.c_str());
        const uint64_t free_bytes = (uint64_t)vfs.f_bavail * vfs.f_frsize;
        const uint64_t reserve = (uint64_t)(min_free_gb * 1073741824.0);
        // Bytes already on disk from an interrupted run don't have to be found twice.
        uint64_t already = 0;
        struct stat pst;
        if (resume && stat(partial.c_str(), &pst) == 0) already = (uint64_t)pst.st_size;
        const uint64_t needed = estimated_output > already ? estimated_output - already : 0;
        if (free_bytes < needed + reserve) {
            return fail("PGRN disk admission refused: free=%" PRIu64 ", still needed=%" PRIu64 ", reserve=%" PRIu64,
                        free_bytes, needed, reserve);
        }
    }

    // --- source fds (uncached: don't balloon the page cache with the model) ---
    for (auto & sh : shards) {
        sh.fd = open(sh.path.c_str(), O_RDONLY);
        if (sh.fd < 0) return fail("open failed: %s", sh.path.c_str());
        pgr_io_hint_sequential(sh.fd);
    }
    auto close_shards = [&](void) {
        for (auto & sh : shards) { if (sh.fd >= 0) { close(sh.fd); sh.fd = -1; } }
    };

    // A cancel this early has written nothing, but an earlier run's .partial may
    // still be lying next to us — report it as resumable instead of "gone".
    auto cancel_untouched = [&](void) -> int {
        close_shards();
        struct stat jst, pst;
        const bool resumable = stat(journal_path.c_str(), &jst) == 0 && stat(partial.c_str(), &pst) == 0;
        if (!resumable) return fail("cancelled");
        if (g_progress) {
            printf("{\"phase\":\"cancelled\",\"resumable\":true,\"records_done\":%" PRIu64
                   ",\"records_total\":%" PRIu64 ",\"partial\":\"%s\"}\n",
                   (uint64_t)(((uint64_t)jst.st_size - PGRNJ_HEADER_BYTES) / PGRN_DIR_RECORD),
                   total_experts, partial.c_str());
            fflush(stdout);
        } else {
            fprintf(stderr, "pgrn-convert: cancelled — %s is intact, resume with --resume\n", partial.c_str());
        }
        return 2;
    };

    // --- phase: sha256 over the whole source, shards in order (header identity) ---
    std::string source_sha;
    {
        phase_reporter rep("sha256", source_size);
        sha256_stream sc;
        const size_t CHUNK = 8 * 1024 * 1024;
        std::vector<uint8_t> buf(CHUNK);
        uint64_t done_total = 0;
        for (auto & sh : shards) {
            uint64_t off = 0;
            while (off < sh.size) {
                if (g_cancel.load()) return cancel_untouched();
                const size_t n = (size_t)std::min<uint64_t>(CHUNK, sh.size - off);
                if (pread_full(sh.fd, buf.data(), n, off) != 0) { close_shards(); return fail("short read during sha256: %s", sh.path.c_str()); }
                sc.update(buf.data(), n);
                off += n;
                done_total += n;
                rep.emit(done_total);
            }
        }
        source_sha = sc.final_hex();
    }
    if (expect_sha && strcasecmp(expect_sha, source_sha.c_str()) != 0) {
        close_shards();
        return fail("source SHA-256 mismatch: expected %s, got %s", expect_sha, source_sha.c_str());
    }

    // --- header JSON — byte-exact replica of the Python converter's layout ---
    std::string meta;
    meta.reserve(8192);
    char tmp[256];
    meta += "{\"model_sha256\":\""; meta += source_sha; meta += "\",";
    snprintf(tmp, sizeof(tmp), "\"source_size\":%" PRIu64 ",", source_size); meta += tmp;
    meta += "\"source_format\":\"GGUF\",";
    // Sharded sources record the shard count; the single-file layout stays
    // byte-identical to the Python reference converter (parity gate).
    if (n_shards > 1) {
        snprintf(tmp, sizeof(tmp), "\"source_shards\":%d,", n_shards); meta += tmp;
    }
    snprintf(tmp, sizeof(tmp), "\"geometry\":{\"layers_with_experts\":%" PRIu64 ",\"experts_per_layer\":%" PRIu64 "},",
             n_layers, expert_count); meta += tmp;
    meta += "\"record_layout_by_layer\":{";
    {
        bool first_layer = true;
        for (auto & kv : by_layer) {
            if (!first_layer) meta += ",";
            first_layer = false;
            snprintf(tmp, sizeof(tmp), "\"%d\":{", kv.first); meta += tmp;
            uint64_t cursor = 0;
            for (int r = 0; r < 3; ++r) {
                const uint64_t sz = kv.second.roles[r].nbytes_total / expert_count;
                snprintf(tmp, sizeof(tmp), "%s\"%s\":{\"offset\":%" PRIu64 ",\"nbytes\":%" PRIu64 ",\"ggml_type\":%u}",
                         r ? "," : "", ROLE_NAMES[r], cursor, sz, kv.second.roles[r].ggml_type);
                meta += tmp;
                cursor += sz;
            }
            meta += "}";
        }
    }
    meta += "},\"tensor_directory\":[";
    {
        bool first = true;
        for (auto & kv : by_layer) {
            if (!first) meta += ",";
            first = false;
            snprintf(tmp, sizeof(tmp), "[%d,%u,%" PRIu64 ",%u,%" PRIu64 ",%u,%" PRIu64 "]",
                     kv.first,
                     kv.second.roles[0].ggml_type, kv.second.roles[0].nbytes_total / expert_count,
                     kv.second.roles[1].ggml_type, kv.second.roles[1].nbytes_total / expert_count,
                     kv.second.roles[2].ggml_type, kv.second.roles[2].nbytes_total / expert_count);
            meta += tmp;
        }
    }
    meta += "]}";

    // --- open .partial output (fresh, or continue a journalled one) ---
    std::vector<uint8_t> dir_records(total_experts * PGRN_DIR_RECORD);

    journal_header jh;
    memcpy(jh.source_sha, source_sha.c_str(), 64);
    jh.source_size    = source_size;
    jh.expert_count   = expert_count;
    jh.n_layers       = n_layers;
    jh.padded_payload = padded_payload;
    {
        // Fingerprint the geometry so a resume cannot mix records of two layouts.
        std::vector<uint8_t> geo(record_bytes.size() * 8);
        size_t i = 0;
        for (const auto & kv : record_bytes) { put_u64(geo.data() + i, kv.second); i += 8; }
        jh.geometry_crc = crc32_update(0, geo.data(), geo.size());
    }

    bool partial_exists = false;
    {
        struct stat ost;
        if (stat(output, &ost) == 0) { close_shards(); return fail("output already exists: %s", output); }
        partial_exists = stat(partial.c_str(), &ost) == 0;
        if (partial_exists && !resume) {
            close_shards();
            return fail("partial output already exists: %s (pass --resume to continue it)", partial.c_str());
        }
    }

    uint64_t resume_records = 0;   // records already committed and CRC-checked
    int journal_fd = -1;
    int out_fd = -1;

    auto abort_partial = [&](void) {
        if (out_fd >= 0) close(out_fd);
        if (journal_fd >= 0) close(journal_fd);
        close_shards();
        unlink(partial.c_str());
        unlink(journal_path.c_str());
    };

    // Cancellation keeps the work: the .partial stays put together with its
    // journal, so --resume can pick it up at the last committed chunk.
    auto cancel_exit = [&](uint64_t records_done) -> int {
        if (journal_fd < 0) { abort_partial(); return fail("cancelled"); }
        (void)fsync(out_fd);
        (void)fsync(journal_fd);
        close(out_fd);   out_fd = -1;
        close(journal_fd); journal_fd = -1;
        close_shards();
        if (g_progress) {
            printf("{\"phase\":\"cancelled\",\"resumable\":true,\"records_done\":%" PRIu64
                   ",\"records_total\":%" PRIu64 ",\"partial\":\"%s\"}\n",
                   records_done, total_experts, partial.c_str());
            fflush(stdout);
        } else {
            fprintf(stderr, "pgrn-convert: cancelled at %" PRIu64 "/%" PRIu64 " experts — resume with --resume\n",
                    records_done, total_experts);
        }
        return 2;
    };

    if (partial_exists) {
        // Resume: the journal decides how much of the .partial is trustworthy.
        journal_fd = open(journal_path.c_str(), O_RDWR);
        if (journal_fd < 0) {
            close_shards();
            return fail("cannot resume without its journal: %s", journal_path.c_str());
        }
        uint8_t hblk[PGRNJ_HEADER_BYTES];
        struct stat jst;
        if (fstat(journal_fd, &jst) != 0 || (uint64_t)jst.st_size < PGRNJ_HEADER_BYTES ||
            pread_full(journal_fd, hblk, PGRNJ_HEADER_BYTES, 0) != 0) {
            close(journal_fd); close_shards();
            return fail("journal is truncated: %s", journal_path.c_str());
        }
        if (const char * why = journal_check_header(hblk, jh)) {
            close(journal_fd); close_shards();
            return fail("cannot resume: %s", why);
        }
        resume_records = ((uint64_t)jst.st_size - PGRNJ_HEADER_BYTES) / PGRN_DIR_RECORD;
        if (resume_records > total_experts) resume_records = total_experts;

        if (resume_records) {
            if (pread_full(journal_fd, dir_records.data(), (size_t)(resume_records * PGRN_DIR_RECORD),
                           PGRNJ_HEADER_BYTES) != 0) {
                close(journal_fd); close_shards();
                return fail("cannot read journal records: %s", journal_path.c_str());
            }
        }

        out_fd = open(partial.c_str(), O_RDWR);
        if (out_fd < 0) { close(journal_fd); close_shards(); return fail("cannot open %s", partial.c_str()); }
        pgr_io_hint_sequential(out_fd);

        // The payload is fsynced before its records are journalled, so the file
        // must already cover every journalled record. If it somehow does not,
        // walk the anchor back instead of trusting the journal.
        struct stat pst;
        if (fstat(out_fd, &pst) != 0) { close(out_fd); close(journal_fd); close_shards(); return fail("stat failed: %s", partial.c_str()); }
        while (resume_records &&
               (uint64_t)pst.st_size < PGRN_ALIGN + prefix_payload_bytes(record_bytes, expert_count, resume_records)) {
            --resume_records;
        }

        const uint64_t prefix = prefix_payload_bytes(record_bytes, expert_count, resume_records);
        if (ftruncate(out_fd, (off_t)(PGRN_ALIGN + prefix)) != 0) {
            close(out_fd); close(journal_fd); close_shards();
            return fail("cannot truncate %s to the last committed record", partial.c_str());
        }
        if (ftruncate(journal_fd, (off_t)(PGRNJ_HEADER_BYTES + resume_records * PGRN_DIR_RECORD)) != 0) {
            close(out_fd); close(journal_fd); close_shards();
            return fail("cannot truncate the journal to its last whole record");
        }
        if (lseek(out_fd, (off_t)(PGRN_ALIGN + prefix), SEEK_SET) < 0 ||
            lseek(journal_fd, 0, SEEK_END) < 0) {
            close(out_fd); close(journal_fd); close_shards();
            return fail("cannot seek to the resume point");
        }

        // Re-check every byte we are about to keep against its journalled CRC.
        // Without this, resume would inherit silent corruption as "done".
        {
            phase_reporter rep("resume", prefix ? prefix : 1);
            const sweep_result sw = crc_sweep(out_fd, dir_records.data(), resume_records, io_threads, rep);
            if (sw.io_error) {
                close(out_fd); close(journal_fd); close_shards();
                return fail("resume read failed");
            }
            // Roll back before reacting to a signal: otherwise the cancel report
            // would claim more valid records than the file actually has.
            if (sw.bad_record < resume_records) {
                // Everything from the first mismatch on is suspect; drop it and redo it.
                resume_records = sw.bad_record;
                const uint64_t good = prefix_payload_bytes(record_bytes, expert_count, resume_records);
                if (ftruncate(out_fd, (off_t)(PGRN_ALIGN + good)) != 0 ||
                    ftruncate(journal_fd, (off_t)(PGRNJ_HEADER_BYTES + resume_records * PGRN_DIR_RECORD)) != 0 ||
                    lseek(out_fd, (off_t)(PGRN_ALIGN + good), SEEK_SET) < 0 ||
                    lseek(journal_fd, 0, SEEK_END) < 0) {
                    close(out_fd); close(journal_fd); close_shards();
                    return fail("cannot roll back to the last intact record");
                }
            }
            if (sw.cancelled) return cancel_exit(resume_records);
            rep.emit(prefix ? prefix : 1);
        }
    } else {
        out_fd = open(partial.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
        if (out_fd < 0) { close_shards(); return fail("cannot create %s", partial.c_str()); }
        pgr_io_hint_sequential(out_fd);

        if (journal_enabled) {
            unlink(journal_path.c_str());   // a journal without its .partial is stale
            journal_fd = open(journal_path.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0644);
            if (journal_fd < 0) { abort_partial(); return fail("cannot create %s", journal_path.c_str()); }
            uint8_t hblk[PGRNJ_HEADER_BYTES];
            journal_pack_header(hblk, jh);
            if (write_full(journal_fd, hblk, sizeof(hblk)) != 0 || fsync(journal_fd) != 0) {
                abort_partial(); return fail("cannot write the journal header");
            }
        }

        static std::vector<uint8_t> zeros(PGRN_ALIGN, 0);
        if (write_full(out_fd, zeros.data(), PGRN_ALIGN) != 0) {
            abort_partial(); return fail("write failed (header block)");
        }
    }

    // --- phase: write experts (parallel reads, strictly ordered writes) ---
    uint64_t payload_done = prefix_payload_bytes(record_bytes, expert_count, resume_records);
    uint64_t cursor = PGRN_ALIGN + payload_done;
    uint64_t dir_index = resume_records;
    {
        phase_reporter rep("write", padded_payload);
        rep.emit(payload_done);
        uint64_t layer_first = 0;   // global index of this layer's first record
        for (auto & kv : by_layer) {
            const int layer = kv.first;
            const uint64_t rec = record_bytes[layer];
            const uint64_t rec_padded = align_up(rec);
            const uint64_t layer_end = layer_first + expert_count;
            if (layer_end <= resume_records) { layer_first = layer_end; continue; }   // fully written
            const uint64_t start_expert = resume_records > layer_first ? resume_records - layer_first : 0;
            layer_first = layer_end;
            // chunk so the in-flight buffer stays <= ~256 MiB
            uint64_t chunk = 268435456ull / rec_padded;
            if (chunk < 1) chunk = 1;
            if (chunk > expert_count) chunk = expert_count;
            std::vector<uint8_t> buf((size_t)(chunk * rec_padded), 0);

            for (uint64_t base = start_expert; base < expert_count; base += chunk) {
                if (g_cancel.load()) return cancel_exit(dir_index);
                const uint64_t count = std::min<uint64_t>(chunk, expert_count - base);
                // parallel reads of [base, base+count) into buf slots (zero-padded slots)
                std::atomic<uint64_t> next{0};
                std::atomic<bool> read_err{false};
                auto worker = [&]() {
                    for (;;) {
                        const uint64_t k = next.fetch_add(1);
                        if (k >= count || read_err.load() || g_cancel.load()) return;
                        uint8_t * slot = buf.data() + (size_t)(k * rec_padded);
                        uint64_t slot_off = 0;
                        for (int r = 0; r < 3; ++r) {
                            const role_t & role = kv.second.roles[r];
                            const shard_t & sh = shards[role.shard];
                            const uint64_t per = role.nbytes_total / expert_count;
                            const uint64_t abs = sh.data_offset + role.rel_offset + (base + k) * per;
                            if (pread_full(sh.fd, slot + slot_off, (size_t)per, abs) != 0) { read_err.store(true); return; }
                            slot_off += per;
                        }
                        // zero the padding tail of this slot (buffer is reused across chunks)
                        memset(slot + rec, 0, (size_t)(rec_padded - rec));
                    }
                };
                std::vector<std::thread> pool;
                const int nthreads = (int)std::min<uint64_t>((uint64_t)io_threads, count);
                pool.reserve(nthreads);
                for (int t = 0; t < nthreads; ++t) pool.emplace_back(worker);
                for (auto & th : pool) th.join();
                if (read_err.load()) { abort_partial(); return fail("short read from GGUF payload"); }
                if (g_cancel.load()) return cancel_exit(dir_index);

                // ordered write + directory records + CRC
                const uint64_t chunk_first = dir_index;
                for (uint64_t k = 0; k < count; ++k) {
                    const uint8_t * slot = buf.data() + (size_t)(k * rec_padded);
                    const uint32_t crc = crc32_update(0, slot, (size_t)rec);
                    pack_dir_record(dir_records.data() + (size_t)(dir_index * PGRN_DIR_RECORD),
                                    (uint16_t)layer, (uint16_t)(base + k), 1, 0, 0.0f,
                                    cursor, (uint32_t)rec, crc);
                    ++dir_index;
                    cursor += rec_padded;
                }
                if (write_full(out_fd, buf.data(), (size_t)(count * rec_padded)) != 0) {
                    abort_partial(); return fail("write failed (expert payload)");
                }
                // Commit order matters: the payload must be durable *before* its
                // records enter the journal, otherwise a resume would trust bytes
                // that never reached the disk.
                if (journal_fd >= 0) {
                    if (fsync(out_fd) != 0) { abort_partial(); return fail("fsync failed (expert payload)"); }
                    if (write_full(journal_fd, dir_records.data() + (size_t)(chunk_first * PGRN_DIR_RECORD),
                                   (size_t)(count * PGRN_DIR_RECORD)) != 0 ||
                        fsync(journal_fd) != 0) {
                        abort_partial(); return fail("write failed (journal records)");
                    }
                }
                payload_done += count * rec_padded;
                rep.emit(payload_done, "expert", dir_index, total_experts);
            }
        }
    }
    const uint64_t expert_dir_offset = cursor;
    if (write_full(out_fd, dir_records.data(), dir_records.size()) != 0) { abort_partial(); return fail("write failed (directory)"); }

    // --- header: magic + version + json_len + json (rest of block is already zeros) ---
    {
        std::string header = "{\"metadata\":";
        header += meta;
        char tail[128];
        snprintf(tail, sizeof(tail), ",\"expert_count\":%" PRIu64 ",\"expert_dir_offset\":%" PRIu64 "}",
                 total_experts, expert_dir_offset);
        header += tail;
        if (8 + 8 + header.size() > PGRN_ALIGN) { abort_partial(); return fail("header metadata exceeds the 16 KiB header block"); }
        uint8_t fixed[16] = { 'P','G','R','N','1',0,0,0, 1,0,0,0, 0,0,0,0 };
        const uint32_t jlen = (uint32_t)header.size();
        fixed[12] = (uint8_t)jlen; fixed[13] = (uint8_t)(jlen >> 8); fixed[14] = (uint8_t)(jlen >> 16); fixed[15] = (uint8_t)(jlen >> 24);
        if (pwrite(out_fd, fixed, 16, 0) != 16 ||
            pwrite(out_fd, header.data(), header.size(), 16) != (ssize_t)header.size()) {
            abort_partial(); return fail("write failed (header)");
        }
    }
    if (fsync(out_fd) != 0) { abort_partial(); return fail("fsync failed"); }

    // --- phase: verify (CRC sweep over every record via the directory) ---
    if (verify) {
        phase_reporter rep("verify", padded_payload);
        const sweep_result sw = crc_sweep(out_fd, dir_records.data(), total_experts, io_threads, rep);
        if (sw.io_error) { abort_partial(); return fail("verify read failed"); }
        // A mismatch here means the finished file is wrong, which outranks a
        // signal: report it instead of exiting as merely "resumable".
        if (sw.bad_record < total_experts) {
            abort_partial();
            return fail("verify CRC mismatch at record %" PRIu64, sw.bad_record);
        }
        if (sw.cancelled) return cancel_exit(dir_index);
        rep.emit(padded_payload);
    }

    close_shards();
    close(out_fd);
    if (journal_fd >= 0) close(journal_fd);
    if (rename(partial.c_str(), output) != 0) {
        unlink(partial.c_str());
        unlink(journal_path.c_str());
        return fail("rename to final output failed");
    }
    unlink(journal_path.c_str());   // the .pgrn is complete; nothing left to resume

    if (g_progress) {
        printf("{\"phase\":\"done\",\"output\":\"%s\",\"output_bytes\":%" PRIu64
               ",\"experts\":%" PRIu64 ",\"sha256_source\":\"%s\"}\n",
               output, expert_dir_offset + total_experts * 26, total_experts, source_sha.c_str());
        fflush(stdout);
    } else {
        printf("wrote %s (%" PRIu64 " experts, %" PRIu64 " bytes)\n",
               output, total_experts, expert_dir_offset + total_experts * 26);
    }

    for (auto & sh : shards) { gguf_free(sh.gctx); ggml_free(sh.ctx_meta); }
    return 0;
}
