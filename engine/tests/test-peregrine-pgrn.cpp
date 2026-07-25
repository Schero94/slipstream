// Native PGRN reader tests: valid lookup/read plus hostile metadata rejection.
#include "../src/peregrine_pgrn.h"

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <unistd.h>
#include <vector>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

static constexpr size_t ALIGN = 16384;
static constexpr size_t DIR_REC = 26;
static constexpr uint64_t DIR_OFF = 3 * ALIGN;
static const char * SHA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

static uint32_t crc32_bytes(const unsigned char * data, size_t size) {
    uint32_t crc = 0xffffffffU;
    for (size_t i = 0; i < size; ++i) {
        crc ^= data[i];
        for (int bit = 0; bit < 8; ++bit) crc = (crc >> 1) ^ (0xedb88320U & (0U - (crc & 1U)));
    }
    return crc ^ 0xffffffffU;
}

static void put16(unsigned char * p, uint16_t v) { p[0] = v & 0xff; p[1] = v >> 8; }
static void put32(unsigned char * p, uint32_t v) {
    for (int i = 0; i < 4; ++i) p[i] = (unsigned char)(v >> (8 * i));
}
static void put64(unsigned char * p, uint64_t v) {
    for (int i = 0; i < 8; ++i) p[i] = (unsigned char)(v >> (8 * i));
}

static void put_ref(unsigned char * p, uint16_t layer, uint16_t expert,
        uint8_t precision, float heat, uint64_t offset,
        const std::vector<unsigned char> & blob) {
    put16(p + 0, layer); put16(p + 2, expert);
    p[4] = precision; p[5] = 0;
    uint32_t heat_bits = 0; std::memcpy(&heat_bits, &heat, sizeof(heat_bits));
    put32(p + 6, heat_bits); put64(p + 10, offset);
    put32(p + 18, (uint32_t)blob.size());
    put32(p + 22, crc32_bytes(blob.data(), blob.size()));
}

static std::vector<unsigned char> fixture() {
    const std::vector<unsigned char> a = {1, 2, 3, 4, 5};
    const std::vector<unsigned char> b = {9, 8, 7};
    const std::string json = std::string("{\"metadata\":{\"model_sha256\":\"") + SHA +
        "\",\"geometry\":{\"layers_with_experts\":2,\"experts_per_layer\":1},"
        "\"tensor_directory\":[[0,12,1,12,2,12,2],[7,12,1,12,1,12,1]]},\"expert_count\":2,"
        "\"expert_dir_offset\":49152}";
    std::vector<unsigned char> out(DIR_OFF + 2 * DIR_REC, 0);
    std::memcpy(out.data(), "PGRN1\0\0\0", 8);
    put32(out.data() + 8, 1);
    put32(out.data() + 12, (uint32_t)json.size());
    std::memcpy(out.data() + 16, json.data(), json.size());
    std::memcpy(out.data() + ALIGN, a.data(), a.size());
    std::memcpy(out.data() + 2 * ALIGN, b.data(), b.size());
    put_ref(out.data() + DIR_OFF, 0, 0, 1, 0.9f, ALIGN, a);
    put_ref(out.data() + DIR_OFF + DIR_REC, 7, 0, 0, 0.5f, 2 * ALIGN, b);
    return out;
}

static std::string write_temp(const std::vector<unsigned char> & bytes) {
    char path[] = "/tmp/pgrn_native_XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) return {};
    size_t done = 0;
    while (done < bytes.size()) {
        ssize_t n = write(fd, bytes.data() + done, bytes.size() - done);
        if (n <= 0) { close(fd); unlink(path); return {}; }
        done += (size_t)n;
    }
    close(fd);
    return path;
}

static bool rejected(std::vector<unsigned char> bytes) {
    std::string path = write_temp(bytes);
    if (path.empty()) return false;
    pgrn_file * file = pgrn_open(path.c_str(), SHA);
    bool ok = file == nullptr;
    pgrn_close(file);
    unlink(path.c_str());
    return ok;
}

int main() {
    std::vector<unsigned char> bytes = fixture();
    std::string path = write_temp(bytes);
    CHECK(!path.empty());

    pgrn_file * file = pgrn_open(path.c_str(), SHA);
    CHECK(file != nullptr);
    CHECK(pgrn_count(file) == 2);
    CHECK(pgrn_layer_count(file) == 2);
    uint16_t layer_id = UINT16_MAX;
    CHECK(pgrn_layer_at(file, 0, &layer_id) == 1 && layer_id == 0);
    CHECK(pgrn_layer_at(file, 1, &layer_id) == 1 && layer_id == 7);
    CHECK(pgrn_layer_at(file, 2, &layer_id) == 0);
    CHECK(pgrn_experts_per_layer(file) == 1);
    CHECK(std::strcmp(pgrn_model_sha256(file), SHA) == 0);
    pgrn_tensor_layout layout{};
    CHECK(pgrn_find_layout(file, 0, &layout) == 1);
    CHECK(layout.ggml_type[0] == 12 && layout.nbytes[0] == 1 && layout.nbytes[1] == 2 && layout.nbytes[2] == 2);
    pgrn_expert_ref ref{};
    CHECK(pgrn_find(file, 7, 0, &ref) == 1);
    CHECK(ref.offset == 2 * ALIGN && ref.nbytes == 3 && ref.precision == 0);
    unsigned char data[8] = {};
    CHECK(pgrn_read_expert(file, &ref, data, sizeof(data)) == 0);
    CHECK(data[0] == 9 && data[1] == 8 && data[2] == 7);
    CHECK(pgrn_find(file, 7, 1, &ref) == 0);
    pgrn_close(file);

    CHECK(pgrn_open(path.c_str(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb") == nullptr);

    // CRC failures happen at the read boundary, before bytes can become resident.
    bytes[2 * ALIGN] ^= 0xff;
    std::string corrupt = write_temp(bytes);
    CHECK(!corrupt.empty());
    file = pgrn_open(corrupt.c_str(), SHA);
    CHECK(file != nullptr && pgrn_find(file, 7, 0, &ref) == 1);
    CHECK(pgrn_read_expert(file, &ref, data, sizeof(data)) != 0);
    CHECK(std::strstr(pgrn_error(file), "CRC") != nullptr);
    pgrn_close(file);
    unlink(corrupt.c_str());
    unlink(path.c_str());

    bytes = fixture(); bytes[0] = 'X'; CHECK(rejected(bytes));
    bytes = fixture(); put32(bytes.data() + 8, 2); CHECK(rejected(bytes));
    bytes = fixture(); put32(bytes.data() + 12, (uint32_t)ALIGN); CHECK(rejected(bytes));
    bytes = fixture(); put64(bytes.data() + DIR_OFF + 10, ALIGN + 1); CHECK(rejected(bytes));
    bytes = fixture(); bytes[DIR_OFF + 4] = 9; CHECK(rejected(bytes));
    bytes = fixture(); put16(bytes.data() + DIR_OFF + DIR_REC, 0); put16(bytes.data() + DIR_OFF + DIR_REC + 2, 0); CHECK(rejected(bytes));
    bytes = fixture(); bytes.resize(bytes.size() - 1); CHECK(rejected(bytes));

    std::printf("PGRN_NATIVE_OK\n");
    return 0;
}
