#include "peregrine_sha256.h"

#include <cstdio>
#include <cstring>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

int main() {
    char digest[65] = {};
    CHECK(pgr_sha256_bytes("", 0, digest) == 0);
    CHECK(std::strcmp(digest, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855") == 0);
    CHECK(pgr_sha256_bytes("abc", 3, digest) == 0);
    CHECK(std::strcmp(digest, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad") == 0);
    char path[] = "/tmp/pgr_sha_XXXXXX"; int fd = mkstemp(path); CHECK(fd >= 0);
    CHECK(write(fd, "abc", 3) == 3); close(fd);
    CHECK(pgr_sha256_file(path, digest) == 0);
    CHECK(std::strcmp(digest, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad") == 0);
    unlink(path);
    std::printf("PGR_SHA256_OK\n"); return 0;
}
