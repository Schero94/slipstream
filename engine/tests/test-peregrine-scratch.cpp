#include "peregrine_scratch.h"

#include <cstdio>
#include <cstring>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

static ggml_tensor * add(ggml_context * ctx, const char * name, int64_t width, int64_t experts) {
    const int64_t ne[3] = { width, 1, experts };
    ggml_tensor * tensor = ggml_new_tensor(ctx, GGML_TYPE_F32, 3, ne);
    ggml_set_name(tensor, name);
    return tensor;
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
    std::printf("PGR_SCRATCH_OK\n");
    return 0;
}
