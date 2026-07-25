#include "peregrine_stage.h"

#include "ggml-backend.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <unistd.h>
#include <vector>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)
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
    char path[]="/tmp/pgr_stage_XXXXXX"; int fd=mkstemp(path); if(fd<0)return{}; size_t n=0; while(n<data.size()){ssize_t w=write(fd,data.data()+n,data.size()-n);if(w<=0)break;n+=(size_t)w;} close(fd); return n==data.size()?path:std::string();
}

int main() {
    const std::string path=fixture(); CHECK(!path.empty()); pgr_runtime_params rp{}; rp.pgrn_path=path.c_str();rp.model_sha256=SHA;rp.clox_k=4;rp.admission={36*GiB,30*GiB,1,10*GiB,3*COUNT*SLICE,0,1*GiB,0,8*GiB,2*3*SLICE,0}; pgr_admission_plan plan{}; char error[192]={}; pgr_runtime * rt=pgr_runtime_new(&rp,&plan,error,sizeof(error)); CHECK(rt);
    ggml_init_params ip={ggml_tensor_overhead()*4,nullptr,true}; ggml_context * ctx=ggml_init(ip); CHECK(ctx); const int64_t ne[3]={4,1,(int64_t)COUNT}; ggml_tensor * gate=ggml_new_tensor(ctx,GGML_TYPE_F32,3,ne); ggml_tensor * up=ggml_new_tensor(ctx,GGML_TYPE_F32,3,ne); ggml_tensor * down=ggml_new_tensor(ctx,GGML_TYPE_F32,3,ne); ggml_backend_buffer_t buf=ggml_backend_alloc_ctx_tensors_from_buft(ctx,ggml_backend_cpu_buffer_type()); CHECK(buf); ggml_backend_buffer_clear(buf,0);
    const int32_t ids[]={2,0,2}; pgr_stage_stats stats{}; CHECK(pgr_stage_selected(rt,0,ids,3,gate,up,down,&stats,error,sizeof(error))==0); CHECK(stats.experts_requested==3&&stats.experts_copied==2&&stats.bytes_uploaded==6*SLICE); std::vector<unsigned char> out(3*SLICE); ggml_backend_tensor_get(gate,out.data(),0,out.size()); CHECK(out[0]==0x10&&out[SLICE]==0&&out[2*SLICE]==0x12); ggml_backend_tensor_get(up,out.data(),0,out.size()); CHECK(out[0]==0x20&&out[SLICE]==0&&out[2*SLICE]==0x22); ggml_backend_tensor_get(down,out.data(),0,out.size()); CHECK(out[0]==0x30&&out[SLICE]==0&&out[2*SLICE]==0x32);
    CHECK(pgr_stage_selected(rt,0,ids,3,nullptr,up,down,&stats,error,sizeof(error))==-1);
    ggml_backend_buffer_free(buf);ggml_free(ctx);pgr_runtime_free(rt);unlink(path.c_str());std::printf("PGR_STAGE_OK copied=%zu bytes=%zu\n",stats.experts_copied,stats.bytes_uploaded);return 0;
}
