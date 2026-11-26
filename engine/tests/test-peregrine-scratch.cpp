#include "peregrine_scratch.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <unistd.h>
#include <vector>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

static ggml_tensor * add(ggml_context * ctx, const char * name, int64_t width, int64_t experts) {
    const int64_t ne[3] = { width, 1, experts };
    ggml_tensor * tensor = ggml_new_tensor(ctx, GGML_TYPE_F32, 3, ne);
    ggml_set_name(tensor, name);
    return tensor;
}

// a one-layer, three-expert PGRN fixture (same layout as test-peregrine-stage)
static constexpr size_t ALIGN = 16384, DIR_REC = 26, COUNT = 3, SLICE = 16;
static constexpr uint64_t GiB = 1024ULL * 1024ULL * 1024ULL;
static const char * SHA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
static uint32_t crc32_bytes(const unsigned char * data, size_t size) { uint32_t c = ~0U; for (size_t i=0;i<size;++i){c^=data[i];for(int b=0;b<8;++b)c=(c>>1)^(0xedb88320U&(0U-(c&1U)));} return ~c; }
static void put16(unsigned char * p,uint16_t v){p[0]=v;p[1]=v>>8;} static void put32(unsigned char*p,uint32_t v){for(int i=0;i<4;++i)p[i]=v>>(8*i);} static void put64(unsigned char*p,uint64_t v){for(int i=0;i<8;++i)p[i]=v>>(8*i);}
static std::string fixture() {
    const std::string json = std::string("{\"metadata\":{\"model_sha256\":\"") + SHA +
        "\",\"geometry\":{\"experts_per_layer\":3},"
        "\"tensor_directory\":[[0,0,16,0,16,0,16]]},"
        "\"expert_count\":3,\"expert_dir_offset\":65536}";
    std::vector<unsigned char> data(4*ALIGN + COUNT*DIR_REC, 0); std::memcpy(data.data(), "PGRN1\0\0\0", 8); put32(data.data()+8,1); put32(data.data()+12,(uint32_t)json.size()); std::memcpy(data.data()+16,json.data(),json.size());
    for(size_t e=0;e<COUNT;++e){ unsigned char * rec=data.data()+(e+1)*ALIGN; std::memset(rec,0x10+(int)e,SLICE); std::memset(rec+SLICE,0x20+(int)e,SLICE); std::memset(rec+2*SLICE,0x30+(int)e,SLICE); unsigned char * d=data.data()+4*ALIGN+e*DIR_REC; put16(d,0);put16(d+2,(uint16_t)e);d[4]=1;put64(d+10,(e+1)*ALIGN);put32(d+18,3*SLICE);put32(d+22,crc32_bytes(rec,3*SLICE)); }
    char path[]="/tmp/pgr_scratch_XXXXXX"; int fd=mkstemp(path); if(fd<0)return{}; size_t n=0; while(n<data.size()){ssize_t w=write(fd,data.data()+n,data.size()-n);if(w<=0)break;n+=(size_t)w;} close(fd); return n==data.size()?path:std::string();
}

// layer-major hold: a held layer uploads each expert once per epoch, other layers / hold(-1) restore the old behaviour
static int hold_test() {
    const std::string path = fixture(); CHECK(!path.empty());
    pgr_runtime_params rp{}; rp.pgrn_path=path.c_str(); rp.model_sha256=SHA; rp.clox_k=4;
    rp.admission={36*GiB,30*GiB,1,10*GiB,3*COUNT*SLICE,0,1*GiB,0,8*GiB,2*3*SLICE,0};
    pgr_admission_plan plan{}; char error[192]={};
    pgr_runtime * rt = pgr_runtime_new(&rp,&plan,error,sizeof(error)); CHECK(rt);
    ggml_init_params ip = { ggml_tensor_overhead()*4, nullptr, true };
    ggml_context * ctx = ggml_init(ip); CHECK(ctx);
    add(ctx, "blk.0.ffn_gate_exps.weight", 4, COUNT);
    add(ctx, "blk.0.ffn_up_exps.weight",   4, COUNT);
    add(ctx, "blk.0.ffn_down_exps.weight", 4, COUNT);
    pgr_scratch * scratch = pgr_scratch_new(ctx, ggml_backend_cpu_buffer_type(), error, sizeof(error)); CHECK(scratch);
    pgr_stage_stats st{};
    const int32_t a[] = {2, 0, 2};
    const int32_t b[] = {0, 2, 1, 1};
    // no hold: every call uploads its distinct ids
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    CHECK(pgr_scratch_held_skips(scratch) == 0);
    // hold layer 0: the second stage of the same ids uploads nothing
    pgr_scratch_hold(scratch, 0);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 0 && st.bytes_uploaded == 0);
    CHECK(pgr_scratch_stage(scratch, rt, 0, b, 4, &st, error, sizeof(error)) == 0 && st.experts_copied == 1);
    CHECK(pgr_scratch_held_skips(scratch) == 1 + 3 + 3);   // dup 2 in a; all of a; 0,2 and the dup 1 in b
    std::vector<unsigned char> out(3*SLICE);
    ggml_backend_tensor_get(pgr_scratch_tensor(scratch, 0, PGR_SCRATCH_GATE), out.data(), 0, out.size());
    CHECK(out[0]==0x10 && out[SLICE]==0x11 && out[2*SLICE]==0x12);
    // hold(-1) restores the old behaviour
    pgr_scratch_hold(scratch, -1);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    // a hold on another layer is cleared by staging this layer, which then stages normally
    pgr_scratch_hold(scratch, 5);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    // a new epoch of the same layer forgets what was held
    pgr_scratch_hold(scratch, 0);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    pgr_scratch_hold(scratch, 0);
    CHECK(pgr_scratch_stage(scratch, rt, 0, a, 3, &st, error, sizeof(error)) == 0 && st.experts_copied == 2);
    pgr_scratch_free(scratch); ggml_free(ctx); pgr_runtime_free(rt); unlink(path.c_str());
    std::printf("PGR_SCRATCH_HOLD_OK\n");
    return 0;
}

int main() {
    ggml_init_params params = { ggml_tensor_overhead() * 8, nullptr, true };
    ggml_context * ctx = ggml_init(params);
    CHECK(ctx != nullptr);
    add(ctx, "blk.0.ffn_gate_exps.weight", 4, 3);
    add(ctx, "blk.0.ffn_up_exps.weight",   4, 3);
    add(ctx, "blk.0.ffn_down_exps.weight", 4, 3);
    add(ctx, "blk.7.ffn_gate_exps.weight", 8, 3);
    add(ctx, "blk.7.ffn_up_exps.weight",   6, 3);
    add(ctx, "blk.7.ffn_down_exps.weight", 5, 3);

    char error[192] = {};
    pgr_scratch * scratch = pgr_scratch_new(ctx, ggml_backend_cpu_buffer_type(), error, sizeof(error));
    CHECK(scratch != nullptr);
    CHECK(pgr_scratch_layers(scratch) == 2);
    CHECK(pgr_scratch_bytes(scratch) > 0);
    CHECK(pgr_scratch_tensor(scratch, 0, PGR_SCRATCH_GATE)->buffer == pgr_scratch_buffer(scratch));
    CHECK(pgr_scratch_tensor(scratch, 0, PGR_SCRATCH_GATE)->data ==
          pgr_scratch_tensor(scratch, 7, PGR_SCRATCH_GATE)->data);
    CHECK(pgr_scratch_tensor(scratch, 0, PGR_SCRATCH_UP)->data ==
          pgr_scratch_tensor(scratch, 7, PGR_SCRATCH_UP)->data);
    CHECK(pgr_scratch_tensor(scratch, 0, PGR_SCRATCH_DOWN)->data ==
          pgr_scratch_tensor(scratch, 7, PGR_SCRATCH_DOWN)->data);
    CHECK(pgr_scratch_tensor(scratch, 0, PGR_SCRATCH_GATE)->data !=
          pgr_scratch_tensor(scratch, 0, PGR_SCRATCH_UP)->data);

    pgr_scratch_free(scratch);
    ggml_free(ctx);

    params = { ggml_tensor_overhead() * 3, nullptr, true };
    ctx = ggml_init(params);
    CHECK(ctx != nullptr);
    add(ctx, "blk.0.ffn_gate_exps.weight", 4, 3);
    add(ctx, "blk.0.ffn_up_exps.weight", 4, 3);
    CHECK(pgr_scratch_new(ctx, ggml_backend_cpu_buffer_type(), error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "matching gate/up/down") != nullptr);
    ggml_free(ctx);
    if (hold_test() != 0) return 1;
    std::printf("PGR_SCRATCH_OK\n");
    return 0;
}
