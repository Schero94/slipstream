#include "peregrine_ane.h"

#import <CoreML/CoreML.h>
#import <Foundation/Foundation.h>
#import <CommonCrypto/CommonDigest.h>

#include <chrono>
#include <cmath>
#include <cctype>
#include <cstdio>
#include <cstring>
#include <limits>
#include <new>
#include <algorithm>
#include <array>
#include <string>
#include <vector>

struct pgr_ane {
    MLModel * model = nil;
    MLMultiArray * input = nil;
    NSString * input_name = nil;
    NSString * output_name = nil;
    std::vector<int32_t> candidates;
    pgr_ane_stats stats = {};
};

static int pgr_ane_fail(char * error, size_t capacity, const char * message) {
    if (error && capacity) std::snprintf(error, capacity, "%s", message ? message : "Core ML adapter error");
    return -1;
}

static bool pgr_ane_sha256_valid(const char * value) {
    if (!value || std::strlen(value) != 64) return false;
    for (size_t i = 0; i < 64; ++i) if (!std::isxdigit(static_cast<unsigned char>(value[i]))) return false;
    return true;
}

static void pgr_ane_hash_u64(CC_SHA256_CTX * ctx, uint64_t value) {
    std::array<uint8_t, 8> encoded{};
    for (size_t i = 0; i < encoded.size(); ++i) encoded[i] = static_cast<uint8_t>(value >> (8 * i));
    CC_SHA256_Update(ctx, encoded.data(), static_cast<CC_LONG>(encoded.size()));
}

int pgr_ane_package_sha256(
        const char * compiled_model_path,
        char output_hex[65],
        char * error,
        size_t error_capacity) {
    if (output_hex) output_hex[0] = '\0';
    if (error && error_capacity) error[0] = '\0';
    if (!compiled_model_path || !output_hex) {
        return pgr_ane_fail(error, error_capacity, "invalid Core ML package hash parameters");
    }
    @autoreleasepool {
        NSFileManager * manager = [NSFileManager defaultManager];
        NSString * root = [NSString stringWithUTF8String:compiled_model_path];
        BOOL directory = NO;
        if (!root || ![manager fileExistsAtPath:root isDirectory:&directory]) {
            return pgr_ane_fail(error, error_capacity, "Core ML package is absent");
        }
        std::vector<std::string> relative_paths;
        if (directory) {
            NSDirectoryEnumerator<NSString *> * entries = [manager enumeratorAtPath:root];
            for (NSString * relative in entries) {
                NSString * full = [root stringByAppendingPathComponent:relative];
                NSDictionary<NSFileAttributeKey, id> * attributes =
                        [manager attributesOfItemAtPath:full error:nil];
                if ([attributes[NSFileType] isEqualToString:NSFileTypeRegular]) {
                    relative_paths.emplace_back(relative.UTF8String);
                } else if (![attributes[NSFileType] isEqualToString:NSFileTypeDirectory]) {
                    return pgr_ane_fail(error, error_capacity, "Core ML package contains a non-regular entry");
                }
            }
        } else {
            relative_paths.emplace_back(".");
        }
        std::sort(relative_paths.begin(), relative_paths.end());

        CC_SHA256_CTX ctx;
        CC_SHA256_Init(&ctx);
        const uint8_t format_tag[] = { 'P', 'G', 'R', 'A', 'N', 'E', '1', 0 };
        CC_SHA256_Update(&ctx, format_tag, static_cast<CC_LONG>(sizeof(format_tag)));
        std::array<uint8_t, 1024 * 1024> buffer{};
        for (const std::string & relative : relative_paths) {
            NSString * full = directory
                    ? [root stringByAppendingPathComponent:[NSString stringWithUTF8String:relative.c_str()]] : root;
            FILE * file = std::fopen(full.fileSystemRepresentation, "rb");
            if (!file) return pgr_ane_fail(error, error_capacity, "cannot read Core ML package file");
            pgr_ane_hash_u64(&ctx, relative.size());
            CC_SHA256_Update(&ctx, relative.data(), static_cast<CC_LONG>(relative.size()));
            uint64_t file_size = 0;
            NSDictionary<NSFileAttributeKey, id> * attributes = [manager attributesOfItemAtPath:full error:nil];
            file_size = [attributes[NSFileSize] unsignedLongLongValue];
            pgr_ane_hash_u64(&ctx, file_size);
            uint64_t consumed = 0;
            while (consumed < file_size) {
                const size_t wanted = static_cast<size_t>(std::min<uint64_t>(buffer.size(), file_size - consumed));
                const size_t got = std::fread(buffer.data(), 1, wanted, file);
                if (got != wanted) {
                    std::fclose(file);
                    return pgr_ane_fail(error, error_capacity, "short read while hashing Core ML package");
                }
                CC_SHA256_Update(&ctx, buffer.data(), static_cast<CC_LONG>(got));
                consumed += got;
            }
            std::fclose(file);
        }
        std::array<uint8_t, CC_SHA256_DIGEST_LENGTH> digest{};
        CC_SHA256_Final(digest.data(), &ctx);
        for (size_t i = 0; i < digest.size(); ++i) std::snprintf(output_hex + 2 * i, 3, "%02x", digest[i]);
        output_hex[64] = '\0';
        return 0;
    }
}

static uint64_t pgr_ane_package_bytes(NSString * path) {
    NSFileManager * manager = [NSFileManager defaultManager];
    BOOL directory = NO;
    if (![manager fileExistsAtPath:path isDirectory:&directory]) return 0;
    if (!directory) {
        NSNumber * size = [manager attributesOfItemAtPath:path error:nil][NSFileSize];
        return size ? size.unsignedLongLongValue : 0;
    }
    uint64_t total = 0;
    NSDirectoryEnumerator<NSString *> * entries = [manager enumeratorAtPath:path];
    for (NSString * relative in entries) {
        NSDictionary<NSFileAttributeKey, id> * attributes =
                [manager attributesOfItemAtPath:[path stringByAppendingPathComponent:relative] error:nil];
        if ([attributes[NSFileType] isEqualToString:NSFileTypeRegular]) {
            const uint64_t size = [attributes[NSFileSize] unsignedLongLongValue];
            if (total > std::numeric_limits<uint64_t>::max() - size) return 0;
            total += size;
        }
    }
    return total;
}

static bool pgr_ane_shape_count(NSArray<NSNumber *> * shape, size_t * count) {
    if (!shape || !count) return false;
    size_t value = 1;
    for (NSNumber * dimension in shape) {
        const long long dim = dimension.longLongValue;
        if (dim <= 0 || static_cast<unsigned long long>(dim) > SIZE_MAX / value) return false;
        value *= static_cast<size_t>(dim);
    }
    *count = value;
    return true;
}

pgr_ane * pgr_ane_new(const pgr_ane_params * params, char * error, size_t error_capacity) {
    if (error && error_capacity) error[0] = '\0';
    if (!params || !params->compiled_model_path || !params->expected_model_sha256 ||
            !params->expected_package_sha256 ||
            !params->architecture || !params->expected_precision || !params->input_name || !params->output_name ||
            !pgr_ane_sha256_valid(params->expected_model_sha256) || params->input_count == 0 ||
            !pgr_ane_sha256_valid(params->expected_package_sha256) ||
            params->candidate_count == 0 || params->allocation_budget_bytes == 0 ||
            params->input_count > SIZE_MAX / sizeof(int32_t) ||
            params->candidate_count > SIZE_MAX / sizeof(int32_t)) {
        pgr_ane_fail(error, error_capacity, "invalid bounded Core ML proposal parameters");
        return nullptr;
    }
    if (@available(macOS 13.0, *)) {
        @autoreleasepool {
            NSString * path = [NSString stringWithUTF8String:params->compiled_model_path];
            char actual_package_sha256[65] = {};
            if (pgr_ane_package_sha256(params->compiled_model_path, actual_package_sha256,
                        error, error_capacity) != 0) {
                return nullptr;
            }
            if (std::strcmp(actual_package_sha256, params->expected_package_sha256) != 0) {
                pgr_ane_fail(error, error_capacity, "Core ML package hash does not match its manifest");
                return nullptr;
            }
            const uint64_t package_bytes = pgr_ane_package_bytes(path);
            const uint64_t array_bytes = (params->input_count + params->candidate_count) * sizeof(int32_t);
            if (package_bytes == 0 || package_bytes > params->allocation_budget_bytes ||
                    array_bytes > params->allocation_budget_bytes - package_bytes) {
                pgr_ane_fail(error, error_capacity, "Core ML package is absent or exceeds its admitted budget");
                return nullptr;
            }
            MLModelConfiguration * configuration = [[MLModelConfiguration alloc] init];
            configuration.computeUnits = MLComputeUnitsCPUAndNeuralEngine;
            NSError * native_error = nil;
            NSURL * url = [NSURL fileURLWithPath:path];
            MLModel * model = [MLModel modelWithContentsOfURL:url configuration:configuration error:&native_error];
            if (!model) {
                pgr_ane_fail(error, error_capacity, native_error.localizedDescription.UTF8String);
                return nullptr;
            }
            NSDictionary * creator = model.modelDescription.metadata[MLModelCreatorDefinedKey];
            NSString * source_sha = [creator isKindOfClass:[NSDictionary class]]
                    ? creator[@"peregrine.source_sha256"] : nil;
            NSString * architecture = [creator isKindOfClass:[NSDictionary class]]
                    ? creator[@"peregrine.architecture"] : nil;
            NSString * precision = [creator isKindOfClass:[NSDictionary class]]
                    ? creator[@"peregrine.precision"] : nil;
            if (![source_sha isEqualToString:[NSString stringWithUTF8String:params->expected_model_sha256]] ||
                    ![architecture isEqualToString:[NSString stringWithUTF8String:params->architecture]] ||
                    ![precision isEqualToString:[NSString stringWithUTF8String:params->expected_precision]]) {
                pgr_ane_fail(error, error_capacity, "Core ML proposal identity does not match the source model");
                return nullptr;
            }
            NSString * input_name = [NSString stringWithUTF8String:params->input_name];
            NSString * output_name = [NSString stringWithUTF8String:params->output_name];
            MLFeatureDescription * input_description = model.modelDescription.inputDescriptionsByName[input_name];
            MLFeatureDescription * output_description = model.modelDescription.outputDescriptionsByName[output_name];
            size_t actual_input = 0;
            size_t actual_output = 0;
            if (!input_description.multiArrayConstraint || !output_description.multiArrayConstraint ||
                    input_description.multiArrayConstraint.dataType != MLMultiArrayDataTypeInt32 ||
                    output_description.multiArrayConstraint.dataType != MLMultiArrayDataTypeInt32 ||
                    !pgr_ane_shape_count(input_description.multiArrayConstraint.shape, &actual_input) ||
                    !pgr_ane_shape_count(output_description.multiArrayConstraint.shape, &actual_output) ||
                    actual_input != params->input_count || actual_output != params->candidate_count) {
                pgr_ane_fail(error, error_capacity, "Core ML proposal tensor shapes or types are incompatible");
                return nullptr;
            }
            MLMultiArray * input = [[MLMultiArray alloc]
                    initWithShape:input_description.multiArrayConstraint.shape
                    dataType:MLMultiArrayDataTypeInt32 error:&native_error];
            if (!input) {
                pgr_ane_fail(error, error_capacity, native_error.localizedDescription.UTF8String);
                return nullptr;
            }
            auto * adapter = new (std::nothrow) pgr_ane;
            if (!adapter) return nullptr;
            adapter->model = model;
            adapter->input = input;
            adapter->input_name = input_name;
            adapter->output_name = output_name;
            adapter->candidates.resize(params->candidate_count);
            adapter->stats.allocated_bytes = package_bytes + array_bytes;
            return adapter;
        }
    }
    pgr_ane_fail(error, error_capacity, "CPU+Neural Engine Core ML execution requires macOS 13 or newer");
    return nullptr;
}

int pgr_ane_propose(
        pgr_ane * adapter,
        const int32_t * input_tokens,
        size_t input_count,
        const int32_t ** candidates,
        size_t * candidate_count,
        char * error,
        size_t error_capacity) {
    if (candidates) *candidates = nullptr;
    if (candidate_count) *candidate_count = 0;
    if (!adapter || !input_tokens || !candidates || !candidate_count ||
            input_count != static_cast<size_t>(adapter->input.count)) {
        return pgr_ane_fail(error, error_capacity, "invalid Core ML proposal input");
    }
    @autoreleasepool {
        std::memcpy(adapter->input.dataPointer, input_tokens, input_count * sizeof(int32_t));
        MLFeatureValue * value = [MLFeatureValue featureValueWithMultiArray:adapter->input];
        NSError * native_error = nil;
        MLDictionaryFeatureProvider * provider = [[MLDictionaryFeatureProvider alloc]
                initWithDictionary:@{adapter->input_name: value} error:&native_error];
        const auto started = std::chrono::steady_clock::now();
        id<MLFeatureProvider> result = provider
                ? [adapter->model predictionFromFeatures:provider error:&native_error] : nil;
        const auto elapsed = std::chrono::duration_cast<std::chrono::microseconds>(
                std::chrono::steady_clock::now() - started).count();
        adapter->stats.calls++;
        adapter->stats.prediction_us += static_cast<uint64_t>(elapsed > 0 ? elapsed : 0);
        MLMultiArray * output = result ? [result featureValueForName:adapter->output_name].multiArrayValue : nil;
        if (!output || output.dataType != MLMultiArrayDataTypeInt32 ||
                static_cast<size_t>(output.count) != adapter->candidates.size()) {
            adapter->stats.failures++;
            return pgr_ane_fail(error, error_capacity,
                    native_error ? native_error.localizedDescription.UTF8String : "Core ML proposal output is invalid");
        }
        std::memcpy(adapter->candidates.data(), output.dataPointer,
                adapter->candidates.size() * sizeof(int32_t));
        adapter->stats.candidates += adapter->candidates.size();
        *candidates = adapter->candidates.data();
        *candidate_count = adapter->candidates.size();
        if (error && error_capacity) error[0] = '\0';
        return 0;
    }
}

void pgr_ane_get_stats(const pgr_ane * adapter, pgr_ane_stats * stats) {
    if (stats) *stats = adapter ? adapter->stats : pgr_ane_stats{};
}

static NSString * pgr_ane_manifest_string(NSDictionary * object, NSString * key) {
    id value = object[key];
    return [value isKindOfClass:[NSString class]] && [value length] > 0 ? value : nil;
}

static NSNumber * pgr_ane_manifest_number(NSDictionary * object, NSString * key) {
    id value = object[key];
    return [value isKindOfClass:[NSNumber class]] ? value : nil;
}

static bool pgr_ane_manifest_keys(NSDictionary * object, NSArray<NSString *> * allowed) {
    if (!object || object.count != allowed.count) return false;
    NSSet * expected = [NSSet setWithArray:allowed];
    return [expected isEqualToSet:[NSSet setWithArray:object.allKeys]];
}

static bool pgr_ane_manifest_integer(NSNumber * value, long long minimum, unsigned long long maximum,
        unsigned long long * result) {
    if (!value || CFGetTypeID((__bridge CFTypeRef) value) == CFBooleanGetTypeID()) return false;
    const double number = value.doubleValue;
    if (!std::isfinite(number) || std::floor(number) != number || number < static_cast<double>(minimum) ||
            number > static_cast<double>(maximum)) return false;
    *result = value.unsignedLongLongValue;
    return true;
}

pgr_ane * pgr_ane_new_from_manifest(
        const pgr_ane_manifest_params * params,
        pgr_ane_info * info,
        char * error,
        size_t error_capacity) {
    if (info) *info = {};
    if (!params || !params->manifest_path || !params->expected_model_sha256 ||
            !params->expected_architecture || !info ||
            !pgr_ane_sha256_valid(params->expected_model_sha256) ||
            params->expected_vocabulary == 0 || params->allocation_budget_bytes == 0) {
        pgr_ane_fail(error, error_capacity, "invalid Core ML manifest parameters");
        return nullptr;
    }
    @autoreleasepool {
        NSString * manifest_path = [[NSString stringWithUTF8String:params->manifest_path] stringByStandardizingPath];
        NSData * data = [NSData dataWithContentsOfFile:manifest_path];
        NSError * native_error = nil;
        id parsed = data ? [NSJSONSerialization JSONObjectWithData:data options:0 error:&native_error] : nil;
        if (![parsed isKindOfClass:[NSDictionary class]]) {
            pgr_ane_fail(error, error_capacity,
                    native_error ? native_error.localizedDescription.UTF8String : "ANE manifest is absent or invalid JSON");
            return nullptr;
        }
        NSDictionary * manifest = parsed;
        NSString * schema = pgr_ane_manifest_string(manifest, @"schema");
        NSString * mode = pgr_ane_manifest_string(manifest, @"mode");
        NSString * package_relative = pgr_ane_manifest_string(manifest, @"compiled_model");
        NSString * package_sha = pgr_ane_manifest_string(manifest, @"package_sha256");
        NSString * source_sha = pgr_ane_manifest_string(manifest, @"source_model_sha256");
        NSString * architecture = pgr_ane_manifest_string(manifest, @"architecture");
        NSDictionary * input = [manifest[@"input"] isKindOfClass:[NSDictionary class]] ? manifest[@"input"] : nil;
        NSDictionary * output = [manifest[@"output"] isKindOfClass:[NSDictionary class]] ? manifest[@"output"] : nil;
        NSDictionary * draft = [manifest[@"draft"] isKindOfClass:[NSDictionary class]] ? manifest[@"draft"] : nil;
        NSString * input_name = pgr_ane_manifest_string(input, @"name");
        NSString * output_name = pgr_ane_manifest_string(output, @"name");
        NSNumber * input_count_number = pgr_ane_manifest_number(input, @"count");
        NSNumber * candidate_count_number = pgr_ane_manifest_number(output, @"count");
        NSNumber * pad_token_number = pgr_ane_manifest_number(input, @"pad_token");
        NSNumber * depth_number = pgr_ane_manifest_number(draft, @"depth");
        NSNumber * width_number = pgr_ane_manifest_number(draft, @"width");
        NSNumber * vocabulary_number = pgr_ane_manifest_number(draft, @"vocabulary");
        NSString * precision = pgr_ane_manifest_string(draft, @"precision");
        unsigned long long input_count = 0;
        unsigned long long candidate_count = 0;
        unsigned long long pad_token_bits = 0;
        unsigned long long depth = 0;
        unsigned long long width = 0;
        unsigned long long vocabulary = 0;
        const bool keys_valid = pgr_ane_manifest_keys(manifest,
                    @[@"schema", @"mode", @"compiled_model", @"package_sha256", @"source_model_sha256",
                      @"architecture", @"input", @"output", @"draft"]) &&
                pgr_ane_manifest_keys(input, @[@"name", @"count", @"pad_token"]) &&
                pgr_ane_manifest_keys(output, @[@"name", @"count"]) &&
                pgr_ane_manifest_keys(draft, @[@"depth", @"width", @"vocabulary", @"precision"]);
        const bool numbers_valid = pgr_ane_manifest_integer(input_count_number, 1, SIZE_MAX, &input_count) &&
                pgr_ane_manifest_integer(candidate_count_number, 1, SIZE_MAX, &candidate_count) &&
                pgr_ane_manifest_integer(pad_token_number, INT32_MIN, INT32_MAX, &pad_token_bits) &&
                pgr_ane_manifest_integer(depth_number, 1, UINT32_MAX, &depth) &&
                pgr_ane_manifest_integer(width_number, 1, UINT32_MAX, &width) &&
                pgr_ane_manifest_integer(vocabulary_number, 1, UINT32_MAX, &vocabulary);
        const long long pad_token = pad_token_number.longLongValue;
        NSString * expected_sha = [NSString stringWithUTF8String:params->expected_model_sha256];
        NSString * expected_arch = [NSString stringWithUTF8String:params->expected_architecture];
        if (!keys_valid || !numbers_valid || ![schema isEqualToString:@"peregrine-ane-v1"] ||
                ![mode isEqualToString:@"one-shot-linear"] ||
                ![package_relative.pathExtension isEqualToString:@"mlmodelc"] || package_relative.isAbsolutePath ||
                package_sha.length != 64 || source_sha.length != 64 ||
                ![source_sha isEqualToString:expected_sha] || ![architecture isEqualToString:expected_arch] ||
                !input_name || !output_name || !precision ||
                depth != candidate_count || width != 1 || vocabulary != params->expected_vocabulary) {
            pgr_ane_fail(error, error_capacity, "ANE manifest identity or tensor contract is incompatible");
            return nullptr;
        }
        NSString * manifest_dir = [manifest_path stringByDeletingLastPathComponent];
        NSString * package_path = [[manifest_dir stringByAppendingPathComponent:package_relative] stringByStandardizingPath];
        NSString * root_prefix = [manifest_dir hasSuffix:@"/"] ? manifest_dir : [manifest_dir stringByAppendingString:@"/"];
        if (![package_path hasPrefix:root_prefix]) {
            pgr_ane_fail(error, error_capacity, "ANE manifest package path escapes its directory");
            return nullptr;
        }
        pgr_ane_params adapter_params{};
        adapter_params.compiled_model_path = package_path.fileSystemRepresentation;
        adapter_params.expected_model_sha256 = params->expected_model_sha256;
        adapter_params.expected_package_sha256 = package_sha.UTF8String;
        adapter_params.architecture = params->expected_architecture;
        adapter_params.expected_precision = precision.UTF8String;
        adapter_params.input_name = input_name.UTF8String;
        adapter_params.output_name = output_name.UTF8String;
        adapter_params.input_count = static_cast<size_t>(input_count);
        adapter_params.candidate_count = static_cast<size_t>(candidate_count);
        adapter_params.allocation_budget_bytes = params->allocation_budget_bytes;
        pgr_ane * adapter = pgr_ane_new(&adapter_params, error, error_capacity);
        if (!adapter) return nullptr;
        info->input_count = adapter_params.input_count;
        info->candidate_count = adapter_params.candidate_count;
        info->pad_token = static_cast<int32_t>(pad_token);
        return adapter;
    }
}

void pgr_ane_free(pgr_ane * adapter) {
    delete adapter;
}
