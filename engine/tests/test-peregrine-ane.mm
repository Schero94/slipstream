#include "peregrine_ane.h"

#import <Foundation/Foundation.h>

#include <cstdio>
#include <cstring>
#include <fstream>
#include <string>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { std::printf("FAIL: %s (line %d)\n", #c, __LINE__); return 1; } } while (0)

int main() {
    char error[256] = {};
    CHECK(pgr_ane_new(nullptr, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "invalid") != nullptr);

    pgr_ane_params params{};
    params.compiled_model_path = "/definitely/absent/peregrine.mlmodelc";
    params.expected_model_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    params.expected_package_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    params.architecture = "qwen35moe";
    params.expected_precision = "float16";
    params.input_name = "tokens";
    params.output_name = "candidates";
    params.input_count = 32;
    params.candidate_count = 8;
    params.allocation_budget_bytes = 1024 * 1024;
    CHECK(pgr_ane_new(&params, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "absent") != nullptr);

    params.expected_model_sha256 = "short";
    CHECK(pgr_ane_new(&params, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "invalid") != nullptr);

    char temp_template[] = "/tmp/pgr-ane-hash-XXXXXX";
    char * temp = mkdtemp(temp_template);
    CHECK(temp != nullptr);
    NSString * root = [NSString stringWithUTF8String:temp];
    NSString * sub = [root stringByAppendingPathComponent:@"sub"];
    CHECK([[NSFileManager defaultManager] createDirectoryAtPath:sub
            withIntermediateDirectories:NO attributes:nil error:nil]);
    const std::string z_path = [[root stringByAppendingPathComponent:@"z.bin"] fileSystemRepresentation];
    const std::string a_path = [[sub stringByAppendingPathComponent:@"a.bin"] fileSystemRepresentation];
    std::ofstream(z_path, std::ios::binary) << "zeta";
    std::ofstream(a_path, std::ios::binary) << "alpha";
    char hash_a[65] = {};
    char hash_b[65] = {};
    CHECK(pgr_ane_package_sha256(temp, hash_a, error, sizeof(error)) == 0);
    CHECK(std::strlen(hash_a) == 64);
    CHECK(std::strcmp(hash_a, "cd8b1b2f0eb40d4da6fc42c8e39b3b5ad9cb417f22f4affa1d33b832e848fb62") == 0);
    CHECK(pgr_ane_package_sha256(temp, hash_b, error, sizeof(error)) == 0);
    CHECK(std::strcmp(hash_a, hash_b) == 0);
    std::ofstream(a_path, std::ios::binary | std::ios::app) << "!";
    CHECK(pgr_ane_package_sha256(temp, hash_b, error, sizeof(error)) == 0);
    CHECK(std::strcmp(hash_a, hash_b) != 0);

    const std::string manifest_path = [[root stringByAppendingPathComponent:@"ane.json"] fileSystemRepresentation];
    std::ofstream manifest(manifest_path);
    manifest << "{\"schema\":\"peregrine-ane-v1\",\"mode\":\"one-shot-linear\","
             << "\"compiled_model\":\"missing.mlmodelc\","
             << "\"package_sha256\":\"" << hash_a << "\","
             << "\"source_model_sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\","
             << "\"architecture\":\"qwen35moe\","
             << "\"input\":{\"name\":\"tokens\",\"count\":32,\"pad_token\":0},"
             << "\"output\":{\"name\":\"candidates\",\"count\":8},"
             << "\"draft\":{\"depth\":8,\"width\":1,\"vocabulary\":248320,\"precision\":\"float16\"}}";
    manifest.close();
    pgr_ane_manifest_params manifest_params{};
    manifest_params.manifest_path = manifest_path.c_str();
    manifest_params.expected_model_sha256 =
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    manifest_params.expected_architecture = "qwen35moe";
    manifest_params.expected_vocabulary = 248320;
    manifest_params.allocation_budget_bytes = 1024 * 1024;
    pgr_ane_info info{1, 1, 1};
    CHECK(pgr_ane_new_from_manifest(&manifest_params, &info, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "absent") != nullptr);
    CHECK(info.input_count == 0 && info.candidate_count == 0);
    manifest_params.expected_architecture = "wrong";
    CHECK(pgr_ane_new_from_manifest(&manifest_params, &info, error, sizeof(error)) == nullptr);
    CHECK(std::strstr(error, "incompatible") != nullptr);
    CHECK([[NSFileManager defaultManager] removeItemAtPath:root error:nil]);

    const int32_t * candidates = reinterpret_cast<const int32_t *>(1);
    size_t count = 99;
    CHECK(pgr_ane_propose(nullptr, nullptr, 0, &candidates, &count, error, sizeof(error)) == -1);
    CHECK(candidates == nullptr && count == 0);
    pgr_ane_stats stats{1, 1, 1, 1, 1};
    pgr_ane_get_stats(nullptr, &stats);
    CHECK(stats.calls == 0 && stats.allocated_bytes == 0);

    std::printf("PGR_ANE_FAIL_CLOSED_OK\n");
    return 0;
}
