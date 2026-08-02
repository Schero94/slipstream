# Slipstream Public Community Mesh Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Deliver one secure Slipstream node for headless Linux and macOS, then expose the same sealed inference protocol over a permissionless, initially free libp2p community mesh.

**Architecture:** Harden the existing direct TCP protocol before adding WAN complexity. Keep the existing X25519 sealed request/result envelopes and engine adapters; add signed identity/capability bindings, process-wide replay/rate admission, explicit local/private/community modes, and headless service assets. Introduce rust-libp2p as a transport/discovery layer behind a versioned application protocol, with QUIC direct paths and relay fallback. The Tauri app controls the same daemon and never enables public donation by default.

**Tech Stack:** Rust 2021, Tokio, Clap, Serde/TOML, X25519 + ChaCha20-Poly1305, Ed25519 identities, rust-libp2p (QUIC, TCP/Noise/Yamux, Identify, Ping, Kademlia, mDNS, AutoNAT, DCUtR, Relay v2), systemd, launchd, Docker, Tauri/vanilla JS, Cargo tests and benchmark scripts.

---

## Task 1: Establish the hardened-node configuration contract

**Files:**
- Create: `apps/p2p-node/src/config.rs`
- Modify: `apps/p2p-node/src/lib.rs`
- Modify: `apps/p2p-node/src/main.rs`
- Modify: `apps/p2p-node/src/runtime.rs`
- Modify: `apps/p2p-node/Cargo.toml`
- Test: `apps/p2p-node/tests/config_contract.rs`

**Step 1: Write the failing tests**

Add tests that parse `local`, `private`, and `community` modes; prove that `local` refuses a non-loopback bind; prove that community donation defaults off; and verify conservative default limits (`4` concurrent jobs, `4096` output tokens, bounded queue/frame sizes).

**Step 2: Run the focused test and confirm failure**

Run: `cargo test -p p2p-node --test config_contract`

Expected: FAIL because `NodeMode` and `NodePolicy` do not exist.

**Step 3: Implement the minimum config types**

Add serde/clap-compatible `NodeMode`, `NodePolicy`, and validation. Wire these values into `NodeConfig` without changing existing safe CLI defaults.

**Step 4: Run focused and workspace tests**

Run: `cargo test -p p2p-node --test config_contract && cargo test --workspace --all-targets`

Expected: PASS.

**Step 5: Commit**

```bash
git add apps/p2p-node
git commit -m "feat(p2p): add explicit node modes and safe policy defaults"
```

## Task 2: Make replay protection process-wide and bounded

**Files:**
- Modify: `crates/p2p-security/src/replay.rs`
- Modify: `apps/p2p-node/src/runtime.rs`
- Test: `crates/p2p-security/src/replay.rs`
- Test: `apps/p2p-node/tests/two_node_job.rs`

**Step 1: Write the failing tests**

Add a cache-capacity test and an integration test that sends the same sealed `job_id` over two separate connections. Assert the second request is rejected and the engine does not run twice.

**Step 2: Confirm the current cross-connection vulnerability**

Run: `cargo test -p p2p-node --test two_node_job replay_across_connections_is_rejected`

Expected: FAIL because each connection owns a fresh `ReplayCache`.

**Step 3: Implement shared bounded state**

Store `Arc<Mutex<ReplayCache>>` on `RunningNode`, share it with every session, add a maximum-entry limit, and evict expired/oldest entries without unbounded allocation. Do not hold the mutex across network or inference awaits.

**Step 4: Validate**

Run: `cargo test -p p2p-security replay && cargo test -p p2p-node --test two_node_job && cargo test --workspace --all-targets`

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/p2p-security/src/replay.rs apps/p2p-node/src/runtime.rs apps/p2p-node/tests/two_node_job.rs
git commit -m "fix(p2p): reject replayed jobs across connections"
```

## Task 3: Enforce admission and free-mode fairness on the hot path

**Files:**
- Create: `apps/p2p-node/src/admission.rs`
- Modify: `apps/p2p-node/src/lib.rs`
- Modify: `apps/p2p-node/src/runtime.rs`
- Modify: `apps/p2p-node/src/config.rs`
- Modify: `crates/p2p-security/src/dos.rs`
- Test: `apps/p2p-node/tests/admission_runtime.rs`

**Step 1: Write the failing attack tests**

Cover max tokens, per-peer burst rate, global concurrency, queue capacity, zero/oversized frames, and isolation between two peer identities. Verify rejections happen before engine inference and return only a bounded sealed error.

**Step 2: Confirm failure**

Run: `cargo test -p p2p-node --test admission_runtime`

Expected: FAIL because `DosLimits` is not connected to the runtime.

**Step 3: Implement admission state and guards**

Create a process-wide admission controller with an atomic/semaphore global job budget and bounded per-peer sliding windows. Validate ciphertext/frame size before decryption and `max_tokens` immediately after decryption. Remove unconditional faucet funding from community/free mode; successful free jobs do not require or settle credits.

**Step 4: Validate bounded behavior**

Run: `cargo test -p p2p-node --test admission_runtime -- --nocapture && cargo test --workspace --all-targets`

Expected: PASS and no prompt text in emitted diagnostics.

**Step 5: Commit**

```bash
git add apps/p2p-node crates/p2p-security/src/dos.rs
git commit -m "feat(p2p): enforce free community admission limits"
```

## Task 4: Authenticate capability advertisements and encryption-key bindings

**Files:**
- Create: `crates/p2p-crypto/src/signing.rs`
- Modify: `crates/p2p-crypto/src/lib.rs`
- Modify: `crates/p2p-crypto/Cargo.toml`
- Modify: `crates/p2p-net/src/message.rs`
- Modify: `apps/p2p-node/src/runtime.rs`
- Modify: `apps/p2p-node/src/main.rs`
- Test: `crates/p2p-crypto/tests/identity_binding.rs`
- Test: `apps/p2p-node/tests/authenticated_hello.rs`

**Step 1: Write the failing cryptographic tests**

Assert that a signed advert verifies, any change to node/encryption key, model, expiry, nonce, or limits fails, expired adverts fail, and an expected peer identity mismatch closes the session. Add challenge-response coverage so possession of the signing key is proven per connection.

**Step 2: Confirm failure**

Run: `cargo test -p p2p-crypto --test identity_binding && cargo test -p p2p-node --test authenticated_hello`

Expected: FAIL because Hello is unsigned.

**Step 3: Implement persistent Ed25519 identity and signed Hello v1**

Use deterministic canonical serialization for the signed payload. Bind Ed25519 identity, X25519 public key, protocol, issue/expiry time, nonce, capability, and policy. Keep secret-key files mode `0600`. Add optional `--expected-peer-id` pinning for direct private connections; public libp2p transport will bind the same identity to its authenticated channel.

**Step 4: Validate adversarial cases**

Run: `cargo test -p p2p-crypto --test identity_binding && cargo test -p p2p-node --test authenticated_hello && cargo test --workspace --all-targets`

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/p2p-crypto crates/p2p-net apps/p2p-node
git commit -m "feat(p2p): authenticate peer capabilities and encryption keys"
```

## Task 5: Remove cleartext result compatibility from product sessions

**Files:**
- Modify: `crates/p2p-net/src/message.rs`
- Modify: `apps/p2p-node/src/runtime.rs`
- Modify: `crates/p2p-net/tests/replay_reject.rs`
- Modify: `crates/p2p-net/tests/two_node.rs`
- Test: `apps/p2p-node/tests/two_node_job.rs`

**Step 1: Add a failing downgrade test**

Have a fake worker send `JobResult` after authenticated Hello and assert the client rejects the downgrade.

**Step 2: Confirm failure**

Run: `cargo test -p p2p-node --test two_node_job cleartext_result_downgrade_is_rejected`

Expected: FAIL because `send_sealed_job` accepts cleartext results.

**Step 3: Remove the product fallback**

Reject the legacy variant in runtime code and keep any frame-shape test isolated to `p2p-net` only. Increment the application protocol compatibility marker.

**Step 4: Validate**

Run: `cargo test --workspace --all-targets`

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/p2p-net apps/p2p-node
git commit -m "fix(p2p): reject cleartext result downgrade"
```

## Task 6: Ship a usable headless daemon contract

**Files:**
- Create: `deploy/slipstream-node.example.toml`
- Create: `deploy/systemd/slipstream-node.service`
- Create: `deploy/launchd/com.slipstream.node.plist`
- Create: `deploy/Dockerfile.node`
- Create: `deploy/README.md`
- Modify: `apps/p2p-node/src/config.rs`
- Modify: `apps/p2p-node/src/main.rs`
- Create: `apps/p2p-node/src/status.rs`
- Test: `apps/p2p-node/tests/headless_smoke.rs`
- Create: `scripts/test_headless_assets.sh`

**Step 1: Write failing config and asset smoke tests**

Test config loading/CLI precedence, `status --json`, loopback health, graceful shutdown, systemd hardening fields, launchd arguments, and a non-root container user.

**Step 2: Confirm failure**

Run: `cargo test -p p2p-node --test headless_smoke && bash scripts/test_headless_assets.sh`

Expected: FAIL because the assets and commands do not exist.

**Step 3: Implement the daemon surface**

Add `--config`, status/health output, signal-aware shutdown, and platform-neutral data-dir handling. Keep administration loopback/local-only. Add production templates with conservative limits and community mode disabled.

**Step 4: Validate Linux-compatible build and assets**

Run: `cargo test -p p2p-node --all-targets && bash scripts/test_headless_assets.sh && cargo check -p p2p-node --features launch`

Expected: PASS.

**Step 5: Commit**

```bash
git add deploy scripts/test_headless_assets.sh apps/p2p-node
git commit -m "feat(node): ship headless Linux and macOS service assets"
```

## Task 7: Add the Mac mini server setup experience

**Files:**
- Modify: `app/dist/index.html`
- Modify: `app/dist/app.js`
- Modify: `app/dist/style.css`
- Modify: `app/src-tauri/src/p2p.rs`
- Modify: `app/src-tauri/src/main.rs`
- Create: `app/scripts/test_server_setup_contract.mjs`

**Step 1: Write the failing UI contract test**

Assert the wizard exposes hardware/model checks, local/private/community modes, donation limits, launch-at-login state, and the exact disclosure that a selected worker sees plaintext prompts. Assert community mode is opt-in and Sensitive/Secret routes are local by default.

**Step 2: Confirm failure**

Run: `node app/scripts/test_server_setup_contract.mjs`

Expected: FAIL because the server wizard does not exist.

**Step 3: Implement the minimum guided flow**

Add a Server panel and Tauri commands that generate/validate configuration and control the local launchd daemon. Reuse current engine and memory checks. Do not spawn a second heavy engine when the existing server is healthy.

**Step 4: Validate UI and Tauri contracts**

Run: `node app/scripts/test_server_setup_contract.mjs && cargo test --manifest-path app/src-tauri/Cargo.toml`

Expected: PASS.

**Step 5: Commit**

```bash
git add app
git commit -m "feat(mac): add guided Mac mini LLM server setup"
```

## Task 8: Introduce the libp2p transport behind the inference protocol

**Files:**
- Create: `crates/p2p-libp2p/Cargo.toml`
- Create: `crates/p2p-libp2p/src/lib.rs`
- Create: `crates/p2p-libp2p/src/behaviour.rs`
- Create: `crates/p2p-libp2p/src/protocol.rs`
- Create: `crates/p2p-libp2p/src/config.rs`
- Modify: `Cargo.toml`
- Modify: `apps/p2p-node/Cargo.toml`
- Modify: `apps/p2p-node/src/runtime.rs`
- Modify: `apps/p2p-node/src/main.rs`
- Test: `crates/p2p-libp2p/tests/direct_quic.rs`

**Step 1: Resolve and pin current official APIs**

Run the project-required Context7 lookup for rust-libp2p immediately before coding. Pin compatible crate features and record the resolved version in `Cargo.lock`.

**Step 2: Write a failing direct-QUIC integration test**

Start two loopback swarms, discover/dial by multiaddr, negotiate `/slipstream/inference/1`, and complete a sealed mock job.

**Step 3: Implement the minimum swarm**

Compose QUIC, TCP/Noise/Yamux fallback, DNS, Identify, Ping, Kademlia, mDNS, Relay client, DCUtR, AutoNAT, and connection limits. Adapt bytes to the existing authenticated sealed protocol; do not duplicate crypto or inference code.

**Step 4: Validate direct transport and compatibility**

Run: `cargo test -p p2p-libp2p --test direct_quic -- --nocapture && cargo test --workspace --all-targets`

Expected: PASS.

**Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/p2p-libp2p apps/p2p-node
git commit -m "feat(mesh): add authenticated libp2p QUIC transport"
```

## Task 9: Add decentralized discovery, NAT traversal, and relay fallback

**Files:**
- Modify: `crates/p2p-libp2p/src/behaviour.rs`
- Modify: `crates/p2p-libp2p/src/config.rs`
- Modify: `apps/p2p-node/src/main.rs`
- Test: `crates/p2p-libp2p/tests/relay_path.rs`
- Test: `crates/p2p-libp2p/tests/discovery.rs`
- Create: `docs/COMMUNITY_MESH_OPERATIONS.md`

**Step 1: Write failing three-node tests**

Prove discovery through bootstrap/Kademlia, force two workers onto a Relay v2 reservation, disable direct dialing, and complete a sealed job through the relay. Assert the relay cannot deserialize plaintext request/result payloads.

**Step 2: Confirm failure**

Run: `cargo test -p p2p-libp2p --test relay_path --test discovery`

Expected: FAIL until discovery/relay event handling is complete.

**Step 3: Implement discovery and relay lifecycle**

Add bootstrap lists, routing-table updates, relay reservations, AutoNAT reachability state, DCUtR upgrades, capped reconnect backoff, and operational status fields.

**Step 4: Validate**

Run: `cargo test -p p2p-libp2p --all-targets && cargo test --workspace --all-targets`

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/p2p-libp2p apps/p2p-node docs/COMMUNITY_MESH_OPERATIONS.md
git commit -m "feat(mesh): add public discovery and relay fallback"
```

## Task 10: Build the vendor A/B qualification harness

**Files:**
- Create: `bench/vendor/run_candidate_ab.py`
- Create: `bench/vendor/candidates.json`
- Create: `bench/vendor/README.md`
- Create: `bench/vendor/test_candidate_ab.py`
- Modify: `bench/RESULTS.md`

**Step 1: Write failing harness tests**

Cover pinned SHAs, clean worktree enforcement, warmups/repeats, deterministic output comparison, TTFT/tok/s/RSS/swap capture, acceptance thresholds, and rejection on missing evidence.

**Step 2: Confirm failure**

Run: `python3 -m unittest bench.vendor.test_candidate_ab`

Expected: FAIL because the harness does not exist.

**Step 3: Implement the isolated runner**

The runner must never mutate the product worktree. It consumes explicit baseline/candidate worktree paths and emits JSON/Markdown artifacts. Seed the manifest with the audited Colibri, oMLX, and llama.cpp candidates, marked `unqualified`.

**Step 4: Validate harness self-tests**

Run: `python3 -m unittest bench.vendor.test_candidate_ab`

Expected: PASS.

**Step 5: Commit**

```bash
git add bench/vendor bench/RESULTS.md
git commit -m "test(vendor): add quality and performance A/B qualification harness"
```

## Task 11: Qualify candidates one at a time

**Files:**
- Modify: `bench/vendor/candidates.json`
- Modify: `bench/RESULTS.md`
- Modify only the engine/vendor files required by a candidate that passes.

**Step 1: Establish fresh baselines**

Run the pinned oMLX and llama.cpp/PEREGRINE benchmark profiles with identical models, quantization, cache, context, prompt, repeat count, and memory reserve.

**Step 2: Test low-conflict correctness fixes first**

Evaluate oMLX trailing-window SpecPrefill correctness and selected llama.cpp Metal fixes. Reject any output/quality regression before performance testing.

**Step 3: Test performance candidates**

Evaluate persistent SpecPrefill, disk-backed prompt cache/KV clone, and Colibri Metal/shared-expert candidates in isolated vendor worktrees.

**Step 4: Record measured decisions**

For every candidate, store SHAs, machine/model conditions, repeat data, confidence/variance, and `accepted` or `rejected` rationale in `bench/RESULTS.md`.

**Step 5: Commit each accepted candidate independently**

```bash
git add <candidate-files> bench/RESULTS.md bench/vendor/candidates.json
git commit -m "perf(<engine>): qualify <candidate>"
```

## Task 12: Release and publication gates

**Files:**
- Modify: `Cargo.toml`
- Modify: `app/src-tauri/Cargo.toml`
- Modify: `app/src-tauri/tauri.conf.json`
- Modify: `docs/index.html`
- Create: `docs/RELEASE_<VERSION>.md`
- Modify: `bench/RESULTS.md`

**Step 1: Run the complete source gates**

Run workspace, Tauri, UI contract, headless asset, direct QUIC, relay, security attack, engine parity, and benchmark acceptance suites. Record exact outputs and skip reasons honestly.

**Step 2: Build and install locally**

Build the signed/packaged macOS app and headless binaries. Install the app locally and run the Mac server wizard smoke test without enabling community donation automatically.

**Step 3: Run a real multi-node smoke**

Complete one local/private sealed job and one isolated community-mode sealed job. Verify worker plaintext disclosure, route policy, encrypted response, metrics, and clean shutdown.

**Step 4: Publish only after all gates pass**

Merge the feature branch, tag the new version, push source/tag, create the GitHub release and assets, and update GitHub Pages/release documentation.

**Step 5: Verify public artifacts**

Verify remote branch/tag/release asset hashes and the live Pages version. Record the final commit, tag, URLs, measured performance/quality evidence, and known limitations.
