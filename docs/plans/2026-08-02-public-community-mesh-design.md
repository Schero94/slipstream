# Slipstream Public Community Mesh Design

**Status:** Approved 2026-08-02

## Goal

Slipstream becomes one platform with three deployment surfaces:

1. a headless Linux server that can be installed and operated without a GUI,
2. a Mac experience that turns a Mac mini into an LLM server with a short guided setup, and
3. an initially free, permissionless public inference mesh in which people may donate idle capacity or submit eligible requests.

Traffic is encrypted end to end between requester and selected worker. The worker decrypts the request in order to run inference. Slipstream must state this boundary plainly: transport peers and relays cannot read the prompt, but the selected worker can. Requests classified as Sensitive or Secret stay local by default; only explicitly eligible requests may enter the community mesh.

## Non-goals

- Claiming that a conventional worker can infer over ciphertext without seeing plaintext.
- Sending private code, credentials, files, or secrets to unknown public workers by default.
- Introducing payment, a token, or a globally trusted credit system in the first public version.
- Blindly merging large upstream or vendor pull requests into the product branch.
- Replacing the proven oMLX and llama.cpp/PEREGRINE inference engines with a new inference implementation.

## Selected Architecture

The existing Rust P2P core, crypto envelopes, engine adapters, router, security helpers, and tests remain the application layer. The current direct TCP MVP is hardened first so it is safe and useful on local and manually connected networks. A rust-libp2p transport is then introduced behind the same application protocol for public discovery and NAT traversal.

This incremental architecture is preferable to a rewrite because the current product already has sealed requests and results, engine parity, routing logic, security primitives, and measured inference paths. Each new layer can therefore be validated independently and rolled back without destabilizing local inference.

## Deployment Surfaces

### Shared node daemon

`slipstream-node` is the single service implementation on Linux and macOS. It owns identity, policy, peer connectivity, admission control, routing, engine attachment, health, and metrics. The desktop app is a controller and client of that daemon, not a separate P2P implementation.

The daemon supports three explicit modes:

- `local`: bind only to loopback and never route remotely;
- `private`: accept configured LAN or allowlisted peers;
- `community`: participate in the public mesh and donate capacity under local limits.

Safe defaults are `local` mode, no public donation, no automatic remote routing for Sensitive or Secret prompts, bounded context and output sizes, and no prompt contents in logs.

### Linux

Linux ships as a standalone binary plus a documented configuration file, a hardened systemd unit, and a container image. It does not require a display server or desktop dependencies. A health endpoint, structured status output, and Prometheus-compatible metrics make it operable on servers.

### macOS / Mac mini

The macOS app controls the same daemon through a local-only administration channel. A "Make this Mac an LLM server" flow checks hardware and free storage, selects a supported engine/model, applies conservative memory settings, creates the node identity, enables a launchd service, and optionally enables community donation. Every public-network option explains that the worker sees assigned plaintext prompts.

## Identity and Capability Authentication

The network identity is a persistent Ed25519 libp2p identity and PeerId. Slipstream retains a dedicated X25519 key for application-layer request encryption. A signed identity binding associates the X25519 public key, PeerId, protocol version, and expiry. This prevents an intermediary from substituting an encryption key while forwarding an otherwise plausible capability advertisement.

Hello/capability advertisements are signed and short-lived. Their signed payload includes:

- PeerId and X25519 encryption key,
- supported protocol versions,
- engine/backend and model identifiers,
- hardware and capacity claims,
- donation policy and admission limits,
- issued-at time, expiry, and a fresh nonce.

Signatures establish continuity of a pseudonymous node identity, not real-world trust. Hardware, speed, model, and availability claims remain untrusted until locally observed.

## Transport and Discovery

The public transport uses rust-libp2p with:

- QUIC as the preferred direct transport;
- TCP with Noise and Yamux as a compatibility fallback;
- Identify and Ping for protocol/liveness negotiation;
- Kademlia plus configured bootstrap peers for decentralized discovery;
- mDNS for zero-configuration LAN discovery;
- AutoNAT and DCUtR for reachability detection and hole punching;
- Circuit Relay v2 when a direct path cannot be established;
- connection limits and per-protocol timeouts at the swarm boundary.

Bootstrap and relay services aid discovery and connectivity but do not schedule inference and cannot decrypt sealed application messages. Multiple bootstrap addresses are supported so the design does not depend on a single permanent coordinator.

## Application Protocol

The initial protocol name is `/slipstream/inference/1`. It carries versioned, length-bounded messages. The existing sealed request and sealed result envelopes remain the payload foundation.

Every inference exchange binds these values cryptographically:

- protocol version,
- requester and worker identities,
- job ID,
- request/response direction,
- issue and expiry times,
- request policy class,
- ciphertext and ephemeral encryption key.

Results remain sealed to the requester. Cleartext result frames are removed from product paths once compatibility tests no longer require them. Relays and passive observers see peer metadata, timing, and byte sizes, but not prompt or generated text.

## Privacy Policy

Request routing uses an explicit policy class:

- `LocalOnly`: never leaves the machine;
- `PrivatePeers`: may use explicitly trusted private peers;
- `CommunityEligible`: may use an unknown public worker after a visible user opt-in.

Sensitive and Secret detection is defense in depth, not a promise that classification is perfect. The UI and CLI make the route visible before submission and provide a local-only override. Prompt bodies, system prompts, generated text, encryption material, and credentials are excluded from logs and metrics.

## Free Community Fairness

The first network is free and donation based. Existing ledger code is not used as a permission gate in community mode. Instead each donor sets hard local budgets for concurrent jobs, tokens per job, jobs per peer/window, queue size, memory pressure, operating hours, and optional model allowlists.

Fairness uses bounded queues, per-peer token buckets, cooldowns, timeouts, and locally observed reliability. There is no globally trusted reputation score in the first release because a permissionless identity is cheap to create. Nodes may prefer peers with successful local history but must remain safe when every new identity is hostile.

## Security Controls

Before public WAN participation, the runtime must enforce rather than merely define:

- signed identity/capability binding;
- a process-wide, bounded replay cache shared across connections;
- maximum frame, prompt, context, output token, and queue sizes;
- global and per-peer concurrency/rate limits;
- request expiry and cancellation;
- connection limits, handshake deadlines, idle timeouts, and backpressure;
- model allowlists/digests where a specific model is promised;
- log redaction and stable non-secret error codes;
- safe engine lifecycle guards and memory-pressure admission.

Unknown peers are never allowed to invoke shell commands, select arbitrary local file paths, load arbitrary plugins, or address localhost services beyond the fixed inference adapter.

## Error Handling and Recovery

Protocol errors return sealed, bounded error codes where a peer identity and encryption key are valid. Authentication failures, oversized frames, expired requests, and rate violations are rejected before inference. Malformed or unauthenticated traffic is closed without reflecting large responses.

The daemon persists identity and configuration atomically, tolerates bootstrap outages, reconnects with capped exponential backoff, and remains capable of local inference when the public mesh is unavailable. A node can disable community mode immediately without unloading the local model.

## Vendor Optimization Lane

Upstream/vendor work is handled as a measured lane, separate from network correctness. Candidate changes are cherry-picked or reimplemented in isolated branches and compared against a pinned baseline. No candidate graduates on claimed numbers alone.

Initial high-value candidates from the 2026-08-02 audit are:

- Colibri Metal prefill attention and shared-expert GPU paths;
- oMLX persistent SpecPrefill prefixes and the trailing-window correctness fix;
- llama.cpp disk-backed prompt cache, slot/KV cloning, Apple RDMA RPC, and selected Metal correctness fixes;
- upstream compatibility changes for models and APIs already supported by Slipstream.

Large distributed-compute PRs remain research inputs until their protocol, security, correctness, and maintenance costs are understood.

## Validation Gates

### Security

- key-substitution/MITM tests fail closed;
- a response capture cannot be opened by the worker, relay, or a third party after delivery;
- a captured request replayed over a different connection is rejected;
- oversized, expired, burst, concurrency, and queue attacks remain bounded;
- logs contain no prompt or generated-text payloads.

### Network

- two nodes complete a sealed job over direct QUIC;
- three nodes complete a sealed job through a relay when direct connectivity is disabled;
- bootstrap loss does not break established jobs or local inference;
- LAN mDNS discovery and manually configured private peers continue working.

### Product

- Linux binary and container run without GUI libraries;
- systemd install/start/restart/upgrade smoke tests pass;
- macOS launchd service survives logout/reboot and the app accurately reflects its state;
- server setup can be completed with safe defaults and reverted cleanly.

### Inference quality and performance

- deterministic prompts preserve the expected output/hash or documented tolerance;
- model-specific quality fixtures do not regress;
- tokens/second, time-to-first-token, peak RSS, swap activity, and failure rate are recorded;
- an optimization graduates only when it passes repeat runs against the same model, quantization, context, and thermal/memory conditions.

## Delivery Sequence

1. Harden the existing direct protocol: authenticated identity binding, process-wide replay protection, active admission limits, free-mode semantics, and attack/integration tests.
2. Package the shared headless daemon: configuration, status/health, Linux systemd/container assets, macOS launchd integration, and the Mac server setup flow.
3. Add the libp2p WAN transport with QUIC, discovery, NAT traversal, relay fallback, and protocol compatibility tests.
4. Operate public bootstrap/relay nodes once deployment infrastructure and domains are available; keep them replaceable and unable to decrypt jobs.
5. Run vendor candidates through isolated quality/performance gates and merge only measured wins.
6. Build, install, publish, and document a versioned Slipstream release after all applicable gates pass.

## Rollback

Each phase is feature-gated. Local inference and the current direct/private mode remain available if WAN transport or discovery is disabled. The desktop release does not enable community donation by default. Vendor optimizations remain independently reversible, and protocol versions permit old and new nodes to reject incompatible exchanges cleanly.
