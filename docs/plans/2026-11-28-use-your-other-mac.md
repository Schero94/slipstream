# Plan: Slipstream v0.6 "Use your other Mac"

Goal: a MacBook's coding editor uses the model on a bigger Mac in the same network (Mac Studio /
mini), privately (own, paired devices only), with the editor's address unchanged
(`http://127.0.0.1:8080/v1`). Drafted 2026-11-28 by a planning agent; source of truth for the
v0.6 milestones below.

## 0. Findings that shape the plan

1. Two transports exist: the app's `p2p.rs` uses the older p2p-net TCP runtime; the QUIC mesh
   (`apps/p2p-node/src/mesh.rs`) is CLI-only.
2. The QUIC mesh cannot stream: `request_response` + `BoundedBytesCodec` (one 4 MiB request /
   response), a fresh swarm + handshake per job, `JobRequest {model, system, prompt, max_tokens}`
   — no messages, tools, SSE or cancel. → needs a streaming protocol on a persistent connection.
3. No trust gate: `private` mode accepts any validly signed peer; `--expected-peer-id` pins only
   the client side. → allowlist before any decryption, both sides.
4. mDNS exists unwired (`crates/p2p-net/src/discovery.rs`, feature `mdns`, `_slipstream-p2p._tcp`,
   empty TXT). QUIC is UDP → `_slipstream._udp`.
5. Port 8080 is llama-server (`--slot-save-path` makes `POST /slots/*` file writes live) and the
   app/tray assume 8080 = local model. A raw tunnel would expose file writes on the big Mac.
6. Cancellation already works when the loopback HTTP connection closes (incl. layer-major abort,
   31712b2) → the remote side only needs to close its upstream connection.
7. Details: app tokio lacks io-util, app HTTP via curl, p2p data dir hard-coded, no Info.plist yet,
   Hello clock-skew 60 s, one Ed25519 identity (= libp2p PeerId) covers pinning.

## 1. Discovery (LAN)

- `mdns-sd` in Rust (app + headless node share it); NWBrowser only as fallback if real-Mac tests fail.
- `SERVICE_TYPE = "_slipstream._udp.local."`; `announce_share(&ShareAdvert{fp, proto, pairing_open, port})`
  → `ShareHandle`; `browse_continuous()` → `Found/Lost` events; system hostname.
- Announced only while "Share this Mac" is on: instance `Slipstream-<fp4>`, TXT exactly
  `v=1`, `fp=<16 hex>`, `pair=1` (only during the pairing window). No names, models, RAM, version.
  Friendly name and model info only over the authenticated channel. The client never announces.
- macOS Local Network / firewall prompts cannot be suppressed: trigger only on the user's click;
  Info.plist `NSLocalNetworkUsageDescription` (EN + DE) and `NSBonjourServices`.
- Fallback without multicast: "Not listed? Enter its address" (`/ip4/<ip>/udp/9003/quic-v1`).

## 2. Pairing and trust

- 6-digit one-time code (OsRng) + SPAKE2 (`crates/p2p-crypto/src/pake.rs`), protocol
  `/slipstream/pair/1` (`apps/p2p-node/src/pairing.rs`); both PeerIds + X25519 keys + name +
  version in the key-confirmation transcript (HMAC, constant-time compare). Window 5 min, max 3
  failures, closes after one success. No QR.
- Trust store `apps/p2p-node/src/trust.rs`: `<data_dir>/trusted_macs.json`, 0600, atomic,
  HMAC-protected (HKDF from node secret); `TrustedPeer{peer_id, identity_hex, encryption_node_id,
  name, role, paired_at, last_addrs, last_seen, app_version, may_start_model}`.
  `SLIPSTREAM_P2P_DIR` / `--data-dir` override (two instances on one Mac).
- Gate at `ConnectionEstablished` (disconnect untrusted unless a pairing window is open, then only
  the pair protocol) and again per stream; client dials pinned `/p2p/<id>` only.
- Revoke closes connections immediately + best-effort `Unpair`; "Reset this Mac's identity" (Advanced).

## 3. Request path — recommendation D: allowlisted L7 forwarder over an authenticated QUIC stream

Rejected: extending the job API (loses OpenAI fidelity, cannot stream), raw TCP tunnel (exposes
`/slots` save/restore file writes, `/props` POST, no admission), LAN HTTP with `--host 0.0.0.0
--api-key` (plaintext Wi-Fi, editor reconfig).

New crate `crates/p2p-forward` (hyper 1, libp2p-stream, tokio):
- `proto.rs`: `/slipstream/openai/1`, `/slipstream/status/1`; frames `[u8 type][u32 len][payload]`
  ReqHead/ReqBody/RespHead/RespBody/Error/Cancel.
- `allowlist.rs` (checked on both ends): forward `POST /v1/chat/completions`, `POST /v1/completions`,
  `GET /v1/models`, `GET /health`, `POST /tokenize|/detokenize|/apply-template`; sanitized
  read-only `GET /slots` (no prompt text), `GET /metrics`, `GET /props`; everything else 403
  ("Not available through a shared Mac"). Strip Authorization/Cookie/hop-by-hop headers.
- `client.rs` (MacBook): binds 127.0.0.1:8080; Host allowlist + Origin rejection (DNS rebinding);
  32 MiB body cap streamed; TCP_NODELAY, flush each response frame; watches client disconnect.
- `worker.rs` (big Mac): trust → admission → upstream request to its llama-server → raw byte pipe;
  stream reset/Cancel drops the upstream connection (cancels the task incl. layer-major prefill).
- `status.rs`: `{name, app_version, model_id, model_running, n_ctx, busy, queue_len, on_battery,
  can_start_model}` + `StartModel`/`StopModel`.
- Transport (`crates/p2p-libp2p`): add a stream behaviour (libp2p-stream or custom handler, decided
  in M0), ping, keep-alive 2 s / idle timeout 10 s, connection limits (4 per peer, 32 total),
  `build_paired_swarm`.
- Encryption: QUIC TLS 1.3 with pinned PeerIds + `SealedStream` (per-stream ephemeral X25519 →
  ChaCha20-Poly1305, counter nonces). No plaintext fallback.
- Timeouts: dial 3 s, stream open 2 s, no first-byte timeout (prefill can take minutes; liveness via
  keep-alive), peer gone → SSE error event or 502, total cap 2 h.
- Fidelity: response bytes identical to upstream (SHA-256 of the SSE body, greedy, fixed seed).

## 4. Big-Mac side

- The app itself serves by default (`app/src-tauri/src/remote.rs`, `RemoteState{share, use_}`);
  headless `slipstream-node share|pair|trust` for a Mac mini (forwards to a running llama-server,
  never spawns one).
- "Paired Macs may start and stop the model" (default on): `StartModel` runs the existing
  `start_server` path with the last `ServerConfig` (refactor to `start_server_impl`); `StopModel`
  only for remotely started models with no local request in flight; optional idle auto-stop.
- No new model process; worker admits only when the server is ready (else 503 `model_not_running`).
- `--parallel 1`: local + remote share one slot (and evict each other's prompt cache). Per-peer
  1 in flight + 2 queued, then 503 + Retry-After; "busy with its own work" indicator.
  `--parallel 2 --kv-unified` only after an A/B record.
- Sleep: power assertion (`caffeinate -i -w <pid>` child) while sharing on AC power; during an
  active remote request even on battery (with a warning).

## 5. Edge cases

Asleep (cached addrs → Wake-on-LAN → 20 s retry → clear message), sleeps mid-request (≤ 10 s
detection, SSE error), network change (re-browse, re-dial; no automatic local fallback — offer a
button), version mismatch (clear "update to 0.6" message), both sides running models (confirm
"stop this MacBook's model?"; `start_server` refuses with `remote_active`), port 8080 busy,
no model on the big Mac ("Start it on Mac Studio"), model/context differs (one-click editor
re-patch, base URL unchanged), battery rules, revoke mid-request, two MacBooks on one big Mac.

## 6. UX

- Cluster tab becomes "Your Macs" / "Deine Macs", always visible; the community mesh moves into a
  collapsed "Advanced: community mesh (experimental)" section (still gated).
- Cards: "Use another Mac" (discovered Macs: Pair / Use) and "Share this Mac" (toggle, code,
  paired list with Remove).
- Journey: big Mac 1 click ("Share this Mac" → code); MacBook 3 clicks ("Use another Mac" →
  "Mac Studio · Pair" → code + Enter); daily use 0 clicks (choice persists, re-dial at start).
- Top bar pill modes: Using / Waking / unreachable / busy; big-Mac badge "Shared · 1 request";
  tray line.
- Honest privacy text (EN/DE): only paired Macs connect; prompts travel encrypted inside your
  network and are processed on the big Mac, whose admin could see them; nothing goes to the
  internet or strangers. Full EN/DE string list in the planning notes (`macs.*`, `pill.remote`,
  `btn.stopRemote`, `err.remote_active`).

## 7. Testing on one Mac

Separate processes with their own `SLIPSTREAM_P2P_DIR`/keys/ports; Rust mock llama (SSE delays,
tool-call stream, prompt_progress, disconnect timestamps) or a real llama-server on 18080; a
userspace UDP shaper (delay, jitter, loss, blackhole); SIGSTOP = sleep. Tests:
`loopback_forward.rs` (byte-identical SSE, 403s, Host/Origin), `cancel.rs` (< 250 ms at 5 ms RTT;
real engine logs the layer-major abort), `trust_gate.rs`, `pairing.rs` (wrong code, MITM relay,
replay), `chaos.rs`; `bench/forward/run_forward_ab.sh` (direct vs forwarded, interleaved);
existing OpenAI stream / cross-engine / forced-tool scripts against the forwarder; pcap marker
check (no plaintext); UI mock knobs `__M.remote` + `test_remote_macs_ui.mjs`.

**Needs a real second Mac before release:** macOS Local Network + firewall prompts with the
ad-hoc bundle (incl. after update); mDNS on real Wi-Fi (AP isolation, mesh routers); Wi-Fi
RTT/jitter vs targets; real sleep/wake (WoL, Sleep Proxy); Wi-Fi↔Ethernet roaming and VPN;
64 GB big Mac with simultaneous local + remote use (0 MB swap both); a real Kilo/OpenCode session
(tool calls, stop button).

## 8. Security checklist

Trust gate before any read/decrypt (connection + per stream); SPAKE2 transcript binding, 3
failures / 5 min; pinned dialing only, mDNS untrusted; trust store 0600 + HMAC; immediate revoke;
forwarder on 127.0.0.1 only, Host allowlist, Origin rejected, streamed body cap; method/path
allowlist on both ends (block `POST /slots/*`, `POST /props`; sanitize `/slots`); header
stripping; share listener only while sharing; connection + admission limits; TLS 1.3 + SealedStream,
no downgrade; `NodeMode::Paired` never enables community donation; no prompt content in logs
(note `/tmp/peregrine-control-server.log` permissions); StartModel through RAM admission; mDNS TXT
key test; fuzz frame decoder and allowlist parser; pcap check; docs (`docs/USE_YOUR_OTHER_MAC.md`,
threat model in `COMMUNITY_MESH_OPERATIONS.md`).

## 9. Milestones

| M | Scope | Acceptance | Effort |
|---|---|---|---|
| M0 | Transport spike: stream behaviour, persistent connection, raw pipe, SealedStream prototype | loopback TTFT overhead < 5 ms p95; 512-token decode within 1 %; AEAD < 2 µs/chunk; go/no-go libp2p-stream | 3–4 d |
| M1 | `crates/p2p-forward` + headless `share`/`use` CLI, `NodeMode::Paired`, admission | SSE byte-identical (plain, strict JSON, forced tool); cancel < 250 ms p95 @5 ms RTT; layer-major abort logged; first-token overhead < 50 ms p95 @8 ms RTT; throughput within 2 %; 403s; peer death ≤ 10 s | 6–8 d |
| M2 | Pairing + trust | pairing/trust-gate tests; revoke aborts ≤ 1 s; MITM fails | 5–7 d |
| M3 | Discovery | ≤ 3 s same host; TXT keys exact; re-dial ≤ 10 s | 3–4 d |
| M4 | App integration + UX | pairing ≤ 3 clicks MacBook / 1 big Mac; 0 clicks daily; EN/DE; UI tests green | 8–10 d |
| M5 | Edge-case hardening (WoL, waking, version mismatch, busy, battery, re-patch, chaos) | every §5 row tested or documented; chaos green at 2 % loss / 20 ms jitter | 4–6 d |
| M6 | Real two-Mac qualification + release | TTFT overhead < 50 ms Wi-Fi / < 15 ms Ethernet; throughput within 2 %; 0 MB swap both; offline ≤ 10 s; wake ≤ 20 s; fresh security review | 3–4 d |

Total ~32–43 dev-days; critical path M0 → M1 → M2 → M4 → M6 (M3 parallel to M2, M5 to late M4).
Out of scope for v0.6: always-on proxy with automatic local/remote fallback, NAT traversal
(an overlay like Tailscale works via the manual address), headless model spawning, `--parallel 2`
by default.
