#pragma once

#include "ggml-backend.h"
#include "llama.h"

#include <cstdint>
#include <memory>
#include <vector>

enum class pgr_routing_phase : uint8_t {
    prompt = 0,
    decode = 1,
};

struct pgr_routing_token_meta {
    uint32_t token_pos;
    int32_t token_id;
    uint16_t sequence_id;
    pgr_routing_phase phase;
};

class pgr_routing_logger {
public:
    static std::unique_ptr<pgr_routing_logger> from_environment();
    ~pgr_routing_logger();

    void begin_batch(const std::vector<pgr_routing_token_meta> & tokens);
    bool capture(ggml_tensor * tensor, bool ask) noexcept;
    void commit_batch();
    void abort_batch() noexcept;

private:
    struct impl;
    explicit pgr_routing_logger(std::unique_ptr<impl> impl);
    std::unique_ptr<impl> pimpl;
};

bool pgr_routing_requested();
bool pgr_routing_eval(ggml_tensor * tensor, bool ask, void * user_data);
pgr_routing_logger * pgr_routing_from_callback(
    ggml_backend_sched_eval_callback callback,
    void * user_data);
