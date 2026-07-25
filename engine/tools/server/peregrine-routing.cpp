#include "peregrine-routing.h"

#include "ggml.h"

#include <array>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <fcntl.h>
#include <limits>
#include <mutex>
#include <stdexcept>
#include <string>
#include <unistd.h>

namespace {

constexpr uint32_t PGR_VERSION = 1;
constexpr uint32_t PGR_ENDIAN_MARKER = 0x01020304;
constexpr size_t PGR_FLUSH_BYTES = 1024 * 1024;
constexpr std::array<const char *, 6> PGR_ENV_NAMES = {
    "PGR_ROUTING_LOG",
    "PGR_SESSION_UUID",
    "PGR_MODEL_SHA256",
    "PGR_EXPECT_LAYERS",
    "PGR_EXPECT_EXPERTS",
    "PGR_EXPECT_TOP_K",
};

#pragma pack(push, 1)
struct pgr_header_v1 {
    char magic[8];
    uint32_t version;
    uint32_t header_bytes;
    uint32_t record_bytes;
    uint32_t endian_marker;
    uint16_t layer_count;
    uint16_t expert_count;
    uint16_t top_k;
    uint16_t flags;
    uint8_t session_uuid[16];
    uint64_t start_time_ns;
    uint8_t model_sha256[32];
    uint32_t crc32;
    uint8_t reserved[8];
};

struct pgr_record_v1 {
    uint64_t batch_id;
    uint32_t token_pos;
    int32_t token_id;
    uint16_t sequence_id;
    uint8_t phase;
    uint8_t layer;
    uint16_t experts[10];
    uint32_t crc32;
};
#pragma pack(pop)

static_assert(sizeof(pgr_header_v1) == 100);
static_assert(sizeof(pgr_record_v1) == 44);
static_assert(offsetof(pgr_header_v1, crc32) == 88);
static_assert(offsetof(pgr_record_v1, crc32) == 40);

uint32_t pgr_crc32(const void * data, size_t size) {
    uint32_t crc = 0xFFFFFFFFU;
    const auto * bytes = static_cast<const uint8_t *>(data);
    for (size_t i = 0; i < size; ++i) {
        crc ^= bytes[i];
        for (int bit = 0; bit < 8; ++bit) {
            crc = (crc >> 1U) ^ (0xEDB88320U & (0U - (crc & 1U)));
        }
    }
    return ~crc;
}

void pgr_write_all(int fd, const uint8_t * data, size_t size) {
    while (size > 0) {
        const ssize_t written = ::write(fd, data, size);
        if (written < 0) {
            if (errno == EINTR) {
                continue;
            }
            throw std::runtime_error("routing log write failed: " + std::string(std::strerror(errno)));
        }
        if (written == 0) {
            throw std::runtime_error("routing log write returned zero bytes");
        }
        data += written;
        size -= static_cast<size_t>(written);
    }
}

std::string pgr_require_env(const char * name) {
    const char * value = std::getenv(name);
    if (value == nullptr || value[0] == '\0') {
        throw std::invalid_argument(std::string("missing required environment variable: ") + name);
    }
    return value;
}

uint16_t pgr_parse_u16(const char * name) {
    const std::string value = pgr_require_env(name);
    size_t consumed = 0;
    unsigned long parsed = 0;
    try {
        parsed = std::stoul(value, &consumed, 10);
    } catch (const std::exception &) {
        throw std::invalid_argument(std::string("invalid integer in ") + name);
    }
    if (consumed != value.size() || parsed == 0 || parsed > std::numeric_limits<uint16_t>::max()) {
        throw std::invalid_argument(std::string("out-of-range integer in ") + name);
    }
    return static_cast<uint16_t>(parsed);
}

uint8_t pgr_hex_nibble(char value) {
    if (value >= '0' && value <= '9') {
        return static_cast<uint8_t>(value - '0');
    }
    if (value >= 'a' && value <= 'f') {
        return static_cast<uint8_t>(value - 'a' + 10);
    }
    if (value >= 'A' && value <= 'F') {
        return static_cast<uint8_t>(value - 'A' + 10);
    }
    throw std::invalid_argument("invalid hexadecimal digit");
}

template<size_t N>
std::array<uint8_t, N> pgr_decode_hex(const std::string & value) {
    if (value.size() != N * 2) {
        throw std::invalid_argument("hexadecimal value has the wrong length");
    }
    std::array<uint8_t, N> output{};
    for (size_t i = 0; i < N; ++i) {
        output[i] = static_cast<uint8_t>((pgr_hex_nibble(value[2 * i]) << 4U) |
                                         pgr_hex_nibble(value[2 * i + 1]));
    }
    return output;
}

std::array<uint8_t, 16> pgr_decode_uuid(const std::string & value) {
    if (value.size() != 36 || value[8] != '-' || value[13] != '-' ||
            value[18] != '-' || value[23] != '-') {
        throw std::invalid_argument("PGR_SESSION_UUID must use canonical UUID syntax");
    }
    std::string compact;
    compact.reserve(32);
    for (char character : value) {
        if (character != '-') {
            compact.push_back(character);
        }
    }
    return pgr_decode_hex<16>(compact);
}

bool pgr_parse_decimal(const char * start, const char * end, uint32_t & result) {
    if (start == end) {
        return false;
    }
    uint32_t value = 0;
    for (const char * cursor = start; cursor != end; ++cursor) {
        if (*cursor < '0' || *cursor > '9') {
            return false;
        }
        if (value > (std::numeric_limits<uint32_t>::max() - 9U) / 10U) {
            return false;
        }
        value = value * 10U + static_cast<uint32_t>(*cursor - '0');
    }
    result = value;
    return true;
}

bool pgr_tensor_layer(const char * name, uint32_t & layer) {
    const char * actual = std::strstr(name, "ffn_moe_topk-");
    if (actual != nullptr) {
        actual += std::strlen("ffn_moe_topk-");
        return pgr_parse_decimal(actual, actual + std::strlen(actual), layer);
    }

    const char * planned = std::strstr(name, "blk.");
    if (planned == nullptr) {
        return false;
    }
    planned += 4;
    const char * suffix = std::strstr(planned, ".ffn_moe_topk");
    if (suffix == nullptr) {
        return false;
    }
    return pgr_parse_decimal(planned, suffix, layer);
}

} // namespace

struct pgr_routing_logger::impl {
    int fd = -1;
    uint16_t expected_layers = 0;
    uint16_t expected_experts = 0;
    uint16_t expected_top_k = 0;
    uint64_t next_batch_id = 1;
    uint64_t current_batch_id = 0;
    std::vector<pgr_routing_token_meta> tokens;
    std::vector<size_t> captured_per_layer;
    std::vector<uint8_t> transaction;
    std::vector<uint8_t> output;
    std::string fatal;
    bool active = false;
    std::mutex mutex;

    ~impl() {
        try {
            flush();
            if (fd >= 0) {
                ::fsync(fd);
            }
        } catch (...) {
        }
        if (fd >= 0) {
            ::close(fd);
        }
    }

    void flush() {
        if (!output.empty()) {
            pgr_write_all(fd, output.data(), output.size());
            output.clear();
        }
    }

    void fail(const std::string & message) noexcept {
        if (fatal.empty()) {
            fatal = message;
        }
    }
};

pgr_routing_logger::pgr_routing_logger(std::unique_ptr<impl> impl_ptr) : pimpl(std::move(impl_ptr)) {
}

pgr_routing_logger::~pgr_routing_logger() = default;

std::unique_ptr<pgr_routing_logger> pgr_routing_logger::from_environment() {
    const std::string path = pgr_require_env("PGR_ROUTING_LOG");
    const auto session_uuid = pgr_decode_uuid(pgr_require_env("PGR_SESSION_UUID"));
    const auto model_sha256 = pgr_decode_hex<32>(pgr_require_env("PGR_MODEL_SHA256"));
    const uint16_t layers = pgr_parse_u16("PGR_EXPECT_LAYERS");
    const uint16_t experts = pgr_parse_u16("PGR_EXPECT_EXPERTS");
    const uint16_t top_k = pgr_parse_u16("PGR_EXPECT_TOP_K");
    if (layers > 256 || top_k > 10 || top_k > experts) {
        throw std::invalid_argument("routing geometry does not fit format v1");
    }

    const int fd = ::open(path.c_str(), O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd < 0) {
        throw std::runtime_error("cannot create routing log exclusively: " + std::string(std::strerror(errno)));
    }

    auto state = std::make_unique<impl>();
    state->fd = fd;
    state->expected_layers = layers;
    state->expected_experts = experts;
    state->expected_top_k = top_k;
    state->captured_per_layer.resize(layers);

    try {
        pgr_header_v1 header{};
        std::memcpy(header.magic, "PGRRT01\0", sizeof(header.magic));
        header.version = PGR_VERSION;
        header.header_bytes = sizeof(header);
        header.record_bytes = sizeof(pgr_record_v1);
        header.endian_marker = PGR_ENDIAN_MARKER;
        header.layer_count = layers;
        header.expert_count = experts;
        header.top_k = top_k;
        std::memcpy(header.session_uuid, session_uuid.data(), session_uuid.size());
        header.start_time_ns = static_cast<uint64_t>(
            std::chrono::duration_cast<std::chrono::nanoseconds>(
                std::chrono::system_clock::now().time_since_epoch()).count());
        std::memcpy(header.model_sha256, model_sha256.data(), model_sha256.size());
        header.crc32 = pgr_crc32(&header, offsetof(pgr_header_v1, crc32));
        pgr_write_all(fd, reinterpret_cast<const uint8_t *>(&header), sizeof(header));
    } catch (...) {
        ::close(fd);
        state->fd = -1;
        throw;
    }

    return std::unique_ptr<pgr_routing_logger>(new pgr_routing_logger(std::move(state)));
}

void pgr_routing_logger::begin_batch(const std::vector<pgr_routing_token_meta> & tokens) {
    std::lock_guard<std::mutex> lock(pimpl->mutex);
    if (!pimpl->fatal.empty()) {
        throw std::runtime_error(pimpl->fatal);
    }
    if (pimpl->active) {
        throw std::logic_error("routing transaction is already active");
    }
    if (tokens.empty()) {
        throw std::invalid_argument("routing transaction cannot be empty");
    }
    pimpl->tokens = tokens;
    std::fill(pimpl->captured_per_layer.begin(), pimpl->captured_per_layer.end(), 0);
    pimpl->transaction.clear();
    pimpl->transaction.reserve(
        tokens.size() * pimpl->expected_layers * sizeof(pgr_record_v1));
    pimpl->current_batch_id = pimpl->next_batch_id++;
    pimpl->active = true;
}

bool pgr_routing_logger::capture(ggml_tensor * tensor, bool ask) noexcept {
    uint32_t layer = 0;
    if (tensor == nullptr || !pgr_tensor_layer(tensor->name, layer)) {
        return false;
    }
    if (ask) {
        std::lock_guard<std::mutex> lock(pimpl->mutex);
        if (!pimpl->active) {
            return false;
        }
        if (layer >= pimpl->expected_layers) {
            pimpl->fail("routing tensor layer is outside expected geometry");
            return false;
        }
        return true;
    }

    std::lock_guard<std::mutex> lock(pimpl->mutex);
    try {
        if (!pimpl->active) {
            throw std::runtime_error(
                "routing callback for tensor '" + std::string(tensor->name) +
                "' (layer " + std::to_string(layer) + ") arrived outside a transaction");
        }
        if (layer >= pimpl->expected_layers) {
            throw std::runtime_error("routing tensor layer is outside expected geometry");
        }
        if (tensor->type != GGML_TYPE_I32 || tensor->nb[0] != sizeof(int32_t)) {
            throw std::runtime_error(
                "routing tensor layout mismatch: type=" + std::string(ggml_type_name(tensor->type)) +
                " contiguous=" + std::to_string(ggml_is_contiguous(tensor)) +
                " ne=[" + std::to_string(tensor->ne[0]) + "," + std::to_string(tensor->ne[1]) +
                "," + std::to_string(tensor->ne[2]) + "," + std::to_string(tensor->ne[3]) + "]" +
                " nb=[" + std::to_string(tensor->nb[0]) + "," + std::to_string(tensor->nb[1]) +
                "," + std::to_string(tensor->nb[2]) + "," + std::to_string(tensor->nb[3]) + "]");
        }
        if (tensor->ne[0] != pimpl->expected_top_k || tensor->ne[1] <= 0 ||
                tensor->ne[2] != 1 || tensor->ne[3] != 1) {
            throw std::runtime_error("routing tensor shape does not match expected top-k");
        }

        const size_t chunk_tokens = static_cast<size_t>(tensor->ne[1]);
        const size_t selected_row_bytes = pimpl->expected_top_k * sizeof(int32_t);
        if (tensor->nb[1] < selected_row_bytes) {
            throw std::runtime_error("routing tensor row stride is smaller than its top-k payload");
        }
        const size_t token_offset = pimpl->captured_per_layer[layer];
        if (chunk_tokens > pimpl->tokens.size() - token_offset) {
            throw std::runtime_error("routing tensor chunks exceed current decode batch");
        }
        std::vector<int32_t> selected(chunk_tokens * pimpl->expected_top_k);
        ggml_backend_tensor_get_2d(
            tensor,
            selected.data(),
            0,
            selected_row_bytes,
            chunk_tokens,
            tensor->nb[1],
            selected_row_bytes);

        for (size_t token = 0; token < chunk_tokens; ++token) {
            pgr_record_v1 record{};
            record.batch_id = pimpl->current_batch_id;
            const auto & meta = pimpl->tokens[token_offset + token];
            record.token_pos = meta.token_pos;
            record.token_id = meta.token_id;
            record.sequence_id = meta.sequence_id;
            record.phase = static_cast<uint8_t>(meta.phase);
            record.layer = static_cast<uint8_t>(layer);
            std::fill(std::begin(record.experts), std::end(record.experts), UINT16_MAX);
            for (uint16_t rank = 0; rank < pimpl->expected_top_k; ++rank) {
                const int32_t expert = selected[token * pimpl->expected_top_k + rank];
                if (expert < 0 || expert >= pimpl->expected_experts) {
                    throw std::runtime_error("routing tensor contains out-of-range expert id");
                }
                for (uint16_t prior = 0; prior < rank; ++prior) {
                    if (record.experts[prior] == expert) {
                        throw std::runtime_error("routing tensor contains duplicate expert ids");
                    }
                }
                record.experts[rank] = static_cast<uint16_t>(expert);
            }
            record.crc32 = pgr_crc32(&record, offsetof(pgr_record_v1, crc32));
            const auto * bytes = reinterpret_cast<const uint8_t *>(&record);
            pimpl->transaction.insert(
                pimpl->transaction.end(), bytes, bytes + sizeof(record));
        }
        pimpl->captured_per_layer[layer] += chunk_tokens;
        return true;
    } catch (const std::exception & error) {
        pimpl->fail(error.what());
    } catch (...) {
        pimpl->fail("unknown routing callback failure");
    }
    return false;
}

void pgr_routing_logger::commit_batch() {
    std::lock_guard<std::mutex> lock(pimpl->mutex);
    if (!pimpl->active) {
        throw std::logic_error("routing transaction is not active");
    }
    for (size_t layer = 0; layer < pimpl->captured_per_layer.size(); ++layer) {
        if (pimpl->captured_per_layer[layer] != pimpl->tokens.size()) {
            pimpl->fail("routing transaction has incomplete layer coverage");
            break;
        }
    }
    const std::string fatal = pimpl->fatal;
    pimpl->active = false;
    pimpl->tokens.clear();
    if (!fatal.empty()) {
        pimpl->transaction.clear();
        throw std::runtime_error(fatal);
    }
    pimpl->output.insert(
        pimpl->output.end(), pimpl->transaction.begin(), pimpl->transaction.end());
    pimpl->transaction.clear();
    if (pimpl->output.size() >= PGR_FLUSH_BYTES) {
        pimpl->flush();
    }
}

void pgr_routing_logger::abort_batch() noexcept {
    std::lock_guard<std::mutex> lock(pimpl->mutex);
    pimpl->active = false;
    pimpl->tokens.clear();
    pimpl->transaction.clear();
    std::fill(pimpl->captured_per_layer.begin(), pimpl->captured_per_layer.end(), 0);
}

bool pgr_routing_requested() {
    for (const char * name : PGR_ENV_NAMES) {
        const char * value = std::getenv(name);
        if (value != nullptr && value[0] != '\0') {
            return true;
        }
    }
    return false;
}

bool pgr_routing_eval(ggml_tensor * tensor, bool ask, void * user_data) {
    if (user_data == nullptr) {
        return false;
    }
    return static_cast<pgr_routing_logger *>(user_data)->capture(tensor, ask);
}

pgr_routing_logger * pgr_routing_from_callback(
        ggml_backend_sched_eval_callback callback,
        void * user_data) {
    if (callback != pgr_routing_eval) {
        return nullptr;
    }
    return static_cast<pgr_routing_logger *>(user_data);
}
