# Native Runtime Capsule Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make bundled llama.cpp/PGRN and oMLX/PGRN verifiable, storage-aware, safely launchable, and measurably equivalent under one Slipstream runtime contract.

**Architecture:** Add a versioned runtime manifest and a Rust preflight module used by the Tauri app and release tests. Add storage classification and reserve admission before large disk operations, expose concrete runtime/device status to the UI, then run one common real-model streaming/stop/reclaim qualification harness against both engines.

**Tech Stack:** Rust 2021, Tauri 2, serde/serde_json, existing shell/Python resource scripts, Node static UI contracts, llama.cpp/PGRN, oMLX/MLX/PGRN, curl SSE probes.

---

### Task 1: Versioned runtime manifest

**Files:**
- Create: `app/src-tauri/resources/runtime-manifest.json`
- Create: `app/scripts/test_runtime_manifest.py`
- Modify: `app/src-tauri/tauri.conf.json`

**Step 1: Write the failing test**

Require schema version 1, product engine `llama.cpp-pgrn`, an explicit absence
of Ollama, required `llama-server` and `pgrn-convert` components, macOS-only
oMLX metadata, exact MLX package pins, and relative resource paths without
parent traversal.

**Step 2: Run the test to verify it fails**

Run: `python3 app/scripts/test_runtime_manifest.py`
Expected: FAIL because `runtime-manifest.json` does not exist.

**Step 3: Write the minimal manifest**

Use this stable shape:

```json
{
  "schema": 1,
  "product_engine": "llama.cpp-pgrn",
  "ollama": false,
  "components": {
    "llama_server": {"path": "llama-server", "required": true},
    "pgrn_convert": {"path": "pgrn-convert", "required": true},
    "omlx_launcher": {"path": "omlx-pgrn/run_omlx_pgrn.sh", "platform": "macos-arm64"},
    "omlx_fork": {"path": "omlx-pgrn/omlx", "platform": "macos-arm64"},
    "pgrn_host": {"path": "omlx-pgrn/libpgrn_host.dylib", "platform": "macos-arm64"}
  },
  "mlx_packages": {"mlx": "0.32.0", "mlx-lm": "0.31.1", "omlx": "0.5.3"}
}
```

Add `resources/runtime-manifest.json` to Tauri resources explicitly.

**Step 4: Run the test to verify it passes**

Run: `python3 app/scripts/test_runtime_manifest.py`
Expected: PASS.

**Step 5: Commit**

```bash
git add app/src-tauri/resources/runtime-manifest.json app/scripts/test_runtime_manifest.py app/src-tauri/tauri.conf.json
git commit -m "feat(runtime): add native component manifest"
```

### Task 2: Fail-closed Rust runtime preflight

**Files:**
- Create: `app/src-tauri/src/runtime.rs`
- Modify: `app/src-tauri/src/main.rs`
- Test: inline `#[cfg(test)]` tests in `app/src-tauri/src/runtime.rs`

**Step 1: Write the failing tests**

Test that preflight:

- rejects an absent required component;
- rejects `../` resource traversal;
- distinguishes an optional platform-mismatched component from a required
  missing component;
- requires regular executable files for server/converter/launcher;
- reports a manifest parse error without panicking.

**Step 2: Run the tests to verify they fail**

Run: `cargo test --manifest-path app/src-tauri/Cargo.toml runtime::tests -- --nocapture`
Expected: compile failure because module/functions do not exist.

**Step 3: Implement the preflight**

Define serializable `RuntimeReport` and `ComponentReport` values. Parse only
schema 1, join paths beneath an explicit resource root, canonicalize the root
and existing component, verify it remains below the root, and return an overall
`ready` only when all applicable required components pass. Never execute a
component during this structural check.

Expose a Tauri command:

```rust
#[tauri::command]
fn runtime_preflight(app: tauri::AppHandle) -> Result<runtime::RuntimeReport, String> {
    let root = app.path().resource_dir().map_err(|e| e.to_string())?.join("resources");
    runtime::preflight(&root)
}
```

Register it in `generate_handler!`.

**Step 4: Run tests**

Run: `cargo test --manifest-path app/src-tauri/Cargo.toml runtime::tests -- --nocapture`
Expected: all new tests PASS.

**Step 5: Commit**

```bash
git add app/src-tauri/src/runtime.rs app/src-tauri/src/main.rs
git commit -m "feat(runtime): verify bundled engines before start"
```

### Task 3: Storage device classification and reserve gate

**Files:**
- Create: `app/src-tauri/src/storage.rs`
- Modify: `app/src-tauri/src/main.rs`
- Test: inline tests in `app/src-tauri/src/storage.rs`

**Step 1: Write failing tests**

Use a command-runner seam and fixtures to cover:

- real path versus symlink path;
- internal SSD, external SSD, and unknown classification;
- available bytes and required reserve;
- refusal when `new_file_bytes + reserve_bytes > available_bytes`;
- PGRN policy warning when a streamed path is external;
- no claim of internal storage when inspection fails.

**Step 2: Verify failure**

Run: `cargo test --manifest-path app/src-tauri/Cargo.toml storage::tests -- --nocapture`
Expected: compile failure.

**Step 3: Implement**

On macOS resolve the existing path or nearest existing ancestor, obtain volume
availability with `statvfs`, and query `diskutil info -plist` for internal and
solid-state properties. Keep the platform seam honest on Linux via `/proc/self/mountinfo`
plus `statvfs`; an unknown rotational property remains `unknown`.

Expose `inspect_storage(path, role, planned_bytes, reserve_bytes)` to the UI.
`role=pgrn` sets `placement_ok=false` for a known external device but remains a
warning; `admitted=false` is reserved for insufficient disk headroom or invalid
paths.

**Step 4: Run tests**

Run: `cargo test --manifest-path app/src-tauri/Cargo.toml storage::tests -- --nocapture`
Expected: PASS.

**Step 5: Commit**

```bash
git add app/src-tauri/src/storage.rs app/src-tauri/src/main.rs
git commit -m "feat(storage): gate model placement by device and reserve"
```

### Task 4: Runtime and placement UI

**Files:**
- Modify: `app/dist/index.html`
- Modify: `app/dist/app.js`
- Modify: `app/dist/style.css`
- Create: `app/scripts/test_native_runtime_ui.mjs`

**Step 1: Write the failing static contract**

Require a runtime card with llama server, converter, oMLX/PGRN, runtime version,
model device, PGRN device, internal/external badge, free disk, and explicit text
that Ollama is neither required nor bundled. Require calls to
`runtime_preflight` and `inspect_storage`; do not infer internal storage from a
path prefix.

**Step 2: Verify failure**

Run: `node app/scripts/test_native_runtime_ui.mjs`
Expected: FAIL for missing UI elements.

**Step 3: Implement the UI**

Render concrete component states and resolved device facts. Block Start when
runtime `ready=false` or storage `admitted=false`. Warn, but do not silently
move data, when PGRN is external. Keep existing backend selection and local
server preference unchanged.

**Step 4: Run contracts and syntax checks**

Run: `node --check app/dist/app.js && node app/scripts/test_native_runtime_ui.mjs && node app/scripts/test_cross_engine_contract.mjs`
Expected: all PASS.

**Step 5: Commit**

```bash
git add app/dist/index.html app/dist/app.js app/dist/style.css app/scripts/test_native_runtime_ui.mjs
git commit -m "feat(ui): show verified runtime and SSD placement"
```

### Task 5: Hash-locked oMLX runtime repair

**Files:**
- Create: `app/src-tauri/resources/omlx-pgrn/requirements-mlx-runtime.lock`
- Modify: `app/src-tauri/resources/omlx-pgrn/bootstrap_mlx_runtime.sh`
- Modify: `app/scripts/test_omlx_contract_resources.py`

**Step 1: Write failing tests**

Require exact package versions, hashes for downloaded wheels, a staging
directory, import verification before rename, atomic `READY` publication, and
preservation of the last verified runtime when repair fails.

**Step 2: Verify failure**

Run: `python3 app/scripts/test_omlx_contract_resources.py`
Expected: FAIL for the missing lock and atomic-repair contract.

**Step 3: Implement minimal safe repair**

Install to `mlx-runtime.next`, verify imports and exact versions, write a
machine-readable manifest, then rename the prior verified directory to a
recoverable backup and atomically promote `.next`. On failure, delete only the
staging directory and leave the current runtime untouched. Avoid shell
pipelines that download and execute unpinned installers on the release path.

**Step 4: Verify tests and local runtime**

Run: `python3 app/scripts/test_omlx_contract_resources.py`
Run: `/Applications/Slipstream.app/Contents/Resources/resources/omlx-pgrn/bootstrap_mlx_runtime.sh --verify`
Expected: PASS; installed MLX 0.32.0 imports successfully.

**Step 5: Commit**

```bash
git add app/src-tauri/resources/omlx-pgrn/requirements-mlx-runtime.lock app/src-tauri/resources/omlx-pgrn/bootstrap_mlx_runtime.sh app/scripts/test_omlx_contract_resources.py
git commit -m "fix(omlx): make runtime repair atomic and reproducible"
```

### Task 6: Common real-engine streaming qualification harness

**Files:**
- Create: `bench/runtime/run_engine_qualification.py`
- Create: `bench/runtime/test_engine_qualification.py`
- Create: `bench/runtime/README.md`

**Step 1: Write failing harness tests**

Test SSE parsing, TTFT/chunk timing, deterministic output hashing, baseline
capture, owned-PID stop, port-release detection, RSS/swap deltas, reclaim, and
fail-closed JSON verdicts with a mock HTTP fixture.

**Step 2: Verify failure**

Run: `python3 -m unittest bench.runtime.test_engine_qualification -v`
Expected: FAIL because harness does not exist.

**Step 3: Implement harness**

Accept an explicit launch command array, health URL, chat URL, model id, output
JSON, warm-up count (minimum one), repeat count (minimum three), memory reserve,
and performance gates. Never use `pkill`; record and terminate only the spawned
process group. Store generated text only as SHA-256 and length.

**Step 4: Run self-tests**

Run: `python3 -m unittest bench.runtime.test_engine_qualification -v`
Expected: PASS.

**Step 5: Commit**

```bash
git add bench/runtime
git commit -m "test(runtime): add common streaming stability gate"
```

### Task 7: Real internal-SSD qualification

**Files:**
- Modify: `bench/RESULTS.md`
- Generated/ignored: `bench/artifacts/runtime/*.json`

**Step 1: Establish safe machine state**

Verify no owned server runs, ports 8080/8090 are free, free+inactive memory meets
the selected profile, swap counters are captured, and the internal disk reserve
is not crossed. Do not copy a model.

**Step 2: Qualify oMLX/PGRN**

Use `/Users/schero/Modelle/mlx/Qwen3.6-35B-A3B-4bit` with its existing internal
`experts.pgrn`, profile `balanced`, residency `touch`, concurrency 1, one warm-up
and three deterministic streamed chats. Require zero swapout growth, valid
incremental chunks, output stability, successful Stop, port release, and memory
reclaim.

**Step 3: Qualify llama.cpp/PGRN**

Resolve the current qualified GGUF/PGRN pair without moving it. If the PGRN is
external, record the placement as a performance warning rather than claiming an
internal run. Run the same protocol with the currently admitted cache/headroom
point.

**Step 4: Compare and record**

Append exact paths/device classifications, hashes, TTFT, prefill/decode, chunk
timing, RSS/high-water, swap/pageout deltas, and Stop/reclaim verdicts to
`bench/RESULTS.md`. Do not promote a backend on mocks or a single request.

**Step 5: Commit passing evidence**

```bash
git add bench/RESULTS.md
git commit -m "bench(runtime): qualify native engines on internal SSD"
```

### Task 8: Full regression, package and next-loop selection

**Files:**
- Modify only if required by a failing contract.

**Step 1: Run regression suites**

Run root workspace tests, Tauri tests, Python resource tests, Node contracts,
headless assets, runtime harness self-tests, and `git diff --check`.

**Step 2: Build release package**

Stage only audited runtime resources, build the arm64 app/DMG, mount it, verify
manifest/component hashes and architecture, then install and launch-test using a
recoverable backup.

**Step 3: Validate UI in a real browser**

Check desktop and mobile widths, runtime/storage facts, zero horizontal
overflow, and zero console errors.

**Step 4: Publish only when green**

Fast-forward `main`, tag the next version, upload hash-verified artifacts,
verify GitHub Pages, and retain honest Linux/macOS capability boundaries.

**Step 5: Select the next measured bottleneck**

Choose the largest validated contributor among TTFT, PGRN misses, conversion,
prefill, decode, RSS, or UI setup friction. Create the next small A/B task; do
not enable an unqualified vendor PR or experimental switch.
