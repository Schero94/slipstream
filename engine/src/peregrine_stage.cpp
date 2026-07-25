#include "peregrine_stage.h"

#include "ggml-backend.h"

#include <cstdio>
#include <cstring>

static int pgr_stage_fail(char * error, size_t capacity, const char * message) {
    if (error && capacity) std::snprintf(error, capacity, "%s", message);
    return -1;
}

static int pgr_stage_slice(const ggml_tensor * tensor, size_t * slice_bytes) {
    if (!tensor || tensor->ne[2] <= 0 || !tensor->buffer || !tensor->data) return -1;
    const size_t total = ggml_nbytes(tensor);
    const size_t experts = static_cast<size_t>(tensor->ne[2]);
    if (total == 0 || total % experts != 0) return -1;
    *slice_bytes = total / experts;
    return 0;
}

int pgr_stage_selected(
        pgr_runtime * runtime,
        uint16_t layer,
        const int32_t * expert_ids,
        size_t expert_id_count,
        ggml_tensor * gate,
        ggml_tensor * up,
        ggml_tensor * down,
        pgr_stage_stats * stats,
        char * error,
        size_t error_capacity) {
    if (stats) std::memset(stats, 0, sizeof(*stats));
    if (error && error_capacity) error[0] = '\0';
    if (!runtime || (!expert_ids && expert_id_count != 0) || !gate || !up || !down || !stats) {
        return pgr_stage_fail(error, error_capacity, "invalid expert staging arguments");
    }
    if (gate->ne[2] != up->ne[2] || gate->ne[2] != down->ne[2] || gate->ne[2] > UINT16_MAX) {
        return pgr_stage_fail(error, error_capacity, "expert tensor counts differ or exceed PGRN keys");
    }
    size_t gate_bytes = 0, up_bytes = 0, down_bytes = 0;
    if (pgr_stage_slice(gate, &gate_bytes) != 0 ||
            pgr_stage_slice(up, &up_bytes) != 0 ||
            pgr_stage_slice(down, &down_bytes) != 0 ||
            gate_bytes > SIZE_MAX - up_bytes || gate_bytes + up_bytes > SIZE_MAX - down_bytes) {
        return pgr_stage_fail(error, error_capacity, "invalid expert tensor slice geometry");
    }
    const size_t record_bytes = gate_bytes + up_bytes + down_bytes;
    stats->experts_requested = expert_id_count;

    // Start every stage call with a clean epoch for this layer's stream. The batch path
    // pins the experts it returns; without this reset a later serial call (a prefill
    // ubatch whose distinct experts exceed the partition) would inherit those pins and
    // find no evictable slot. Serial-only use (io_width == 1) never pins, so this is a
    // no-op there — but it makes mixing the batch and serial paths always safe.
    pgr_runtime_batch_begin(runtime, layer);

    // Deduplicate the selected ids (a repeated expert is uploaded once), preserving
    // first-seen order.
    enum { PGR_STAGE_MAX = 256 };
    uint16_t unique[PGR_STAGE_MAX];
    int unique_n = 0;
    for (size_t i = 0; i < expert_id_count; ++i) {
        const int32_t id = expert_ids[i];
        if (id < 0 || id >= gate->ne[2]) {
            return pgr_stage_fail(error, error_capacity, "selected expert id is outside tensor bounds");
        }
        bool duplicate = false;
        for (int u = 0; u < unique_n; ++u) {
            if (unique[u] == id) { duplicate = true; break; }
        }
        if (duplicate) continue;
        if (unique_n >= PGR_STAGE_MAX) {
            return pgr_stage_fail(error, error_capacity, "selected expert count exceeds staging maximum");
        }
        unique[unique_n++] = static_cast<uint16_t>(id);
    }
    if (unique_n == 0) return 0;

    // Serial upload of one resident record into its expert slot on the three tensors.
    // The Metal backend is single-writer, so uploads always run on this thread.
    auto upload = [&](uint16_t id, const unsigned char * bytes) {
        const int64_t upload_start = ggml_time_us();
        ggml_backend_tensor_set(gate, bytes, static_cast<size_t>(id) * gate_bytes, gate_bytes);
        ggml_backend_tensor_set(up, bytes + gate_bytes, static_cast<size_t>(id) * up_bytes, up_bytes);
        ggml_backend_tensor_set(down, bytes + gate_bytes + up_bytes, static_cast<size_t>(id) * down_bytes, down_bytes);
        stats->upload_us += (uint64_t) (ggml_time_us() - upload_start);
        stats->experts_copied++;
        stats->bytes_uploaded += record_bytes;
    };

    // Fetch policy. io_width == 1 keeps the qualified serial path exactly. For io_width > 1
    // the experts are fetched in parallel windows and uploaded in input order — the bytes
    // and order are identical to serial, so the scratch (and thus the logits) are unchanged.
    // A decode batch (fits the partition) is one window; a prefill ubatch (more distinct
    // experts than the partition) is chunked so a window never over-pins the cache. This is
    // Phase 2: parallel prefill fetch (no GEMM wave — the scratch already holds every slot).
    const size_t layer_cap = pgr_runtime_layer_capacity(runtime, layer);
    const int io_width = pgr_runtime_io_width(runtime);
    bool staged = false;

    if (io_width > 1 && layer_cap > 0) {
        int window = (static_cast<size_t>(unique_n) <= layer_cap)
                ? unique_n
                : (io_width < static_cast<int>(layer_cap) ? io_width : static_cast<int>(layer_cap));
        if (window < 1) window = 1;
        const void * recs[PGR_STAGE_MAX];
        size_t loaded[PGR_STAGE_MAX];
        staged = true;
        for (int base = 0; base < unique_n && staged; base += window) {
            const int count = (unique_n - base < window) ? (unique_n - base) : window;
            pgr_runtime_batch_begin(runtime, layer);   // release the previous window's pins
            const int64_t fetch_start = ggml_time_us();
            const pgr_stream_status status = pgr_runtime_get_many(
                    runtime, layer, unique + base, count, recs, loaded, nullptr);
            stats->fetch_us += (uint64_t) (ggml_time_us() - fetch_start);
            if (status != PGR_STREAM_OK) { staged = false; break; }
            for (int u = 0; u < count; ++u) {
                if (loaded[u] != record_bytes) { staged = false; break; }
                upload(unique[base + u], static_cast<const unsigned char *>(recs[u]));
            }
        }
        if (!staged) pgr_runtime_batch_begin(runtime, layer);  // clear pins before serial retry
    }

    if (!staged) {
        // Serial path: the qualified io_width==1 default, and the always-correct fallback if
        // a parallel window hit an anomaly (re-uploads are idempotent — same id-indexed slot).
        for (int u = 0; u < unique_n; ++u) {
            const void * record = nullptr;
            size_t loaded = 0;
            int hit = 0;
            const int64_t fetch_start = ggml_time_us();
            const pgr_stream_status status = pgr_runtime_get(
                    runtime, layer, unique[u], &record, &loaded, &hit);
            stats->fetch_us += (uint64_t) (ggml_time_us() - fetch_start);
            if (status != PGR_STREAM_OK) {
                return pgr_stage_fail(error, error_capacity, pgr_runtime_error(runtime));
            }
            if (loaded != record_bytes) {
                return pgr_stage_fail(error, error_capacity, "PGRN record does not match GGUF expert slices");
            }
            upload(unique[u], static_cast<const unsigned char *>(record));
        }
    }
    return 0;
}
