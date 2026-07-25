#include "llama-model-loader.h"

#include "ggml-backend.h"
#include "gguf.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <unistd.h>
#include <vector>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

static std::string make_fixture(size_t * expert_bytes, size_t * dense_bytes) {
    gguf_context_ptr gguf(gguf_init_empty());
    gguf_set_val_str(gguf.get(), "general.architecture", "qwen3moe");

    ggml_init_params params = {
        /*.mem_size   =*/ ggml_tensor_overhead() * 8 + 4096,
        /*.mem_buffer =*/ nullptr,
        /*.no_alloc   =*/ false,
    };
    ggml_context_ptr tensors(ggml_init(params));
    if (!tensors) return {};

    const int64_t expert_ne[3] = { 4, 3, 2 };
    const int64_t dense_ne[2] = { 4, 4 };
    const char * names[] = {
        "blk.0.ffn_gate_exps.weight",
        "blk.0.ffn_up_exps.weight",
        "blk.0.ffn_down_exps.weight",
    };
    *expert_bytes = 0;
    for (const char * name : names) {
        ggml_tensor * tensor = ggml_new_tensor(tensors.get(), GGML_TYPE_F32, 3, expert_ne);
        ggml_set_name(tensor, name);
        std::memset(tensor->data, 0x5a, ggml_nbytes(tensor));
        *expert_bytes += ggml_nbytes(tensor);
        gguf_add_tensor(gguf.get(), tensor);
    }
    ggml_tensor * dense = ggml_new_tensor(tensors.get(), GGML_TYPE_F32, 2, dense_ne);
    ggml_set_name(dense, "blk.0.attn_q.weight");
    std::memset(dense->data, 0x33, ggml_nbytes(dense));
    *dense_bytes = ggml_nbytes(dense);
    gguf_add_tensor(gguf.get(), dense);

    char path[] = "/tmp/pgr_loader_XXXXXX";
    const int fd = mkstemp(path);
    if (fd < 0) return {};
    close(fd);
    if (!gguf_write_to_file(gguf.get(), path, false)) {
        unlink(path);
        return {};
    }
    return path;
}

int main() {
    size_t expected_expert_bytes = 0;
    size_t expected_dense_bytes = 0;
    const std::string path = make_fixture(&expected_expert_bytes, &expected_dense_bytes);
    CHECK(!path.empty());

    std::vector<std::string> splits;
    llama_model_loader loader(
        nullptr, nullptr, nullptr, path, splits, nullptr,
        /*use_mmap*/ true, /*use_direct_io*/ false,
        /*check_tensors*/ false, /*no_alloc*/ false,
        /*external_experts*/ true, nullptr, nullptr);
    CHECK(!loader.use_mmap);

    llama_hparams hparams{};
    const LLM_TN tn(LLM_ARCH_QWEN3MOE);
    const int64_t n_expert = 2;
    ggml_tensor * gate = loader.create_tensor(hparams, nullptr, nullptr, nullptr, nullptr,
        tn(LLM_TENSOR_FFN_GATE_EXPS, "weight", 0), {4, 3, n_expert}, 0);
    ggml_tensor * up = loader.create_tensor(hparams, nullptr, nullptr, nullptr, nullptr,
        tn(LLM_TENSOR_FFN_UP_EXPS, "weight", 0), {4, 3, n_expert}, 0);
    ggml_tensor * down = loader.create_tensor(hparams, nullptr, nullptr, nullptr, nullptr,
        tn(LLM_TENSOR_FFN_DOWN_EXPS, "weight", 0), {4, 3, n_expert}, 0);
    CHECK(gate && up && down);
    CHECK(gate->data == nullptr && up->data == nullptr && down->data == nullptr);
    CHECK(gate->buffer == nullptr && up->buffer == nullptr && down->buffer == nullptr);
    CHECK(loader.external_expert_bytes == expected_expert_bytes);
    CHECK(loader.external_expert_mapped_bytes == 0);
    CHECK(loader.external_expert_read_bytes == 0);
    CHECK(loader.is_external_expert(gate->name));

    loader.init_mappings();
    CHECK(loader.mappings.empty());
    CHECK(loader.size_data == expected_dense_bytes);

    ggml_context_ptr external = loader.take_external_expert_ctx();
    CHECK(external != nullptr);
    CHECK(ggml_get_first_tensor(external.get()) != nullptr);
    unlink(path.c_str());
    std::printf("PGR_LOADER_OK expert=%zu dense=%zu mapped=0 read=0\n",
        expected_expert_bytes, expected_dense_bytes);
    return 0;
}
