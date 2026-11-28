# Slipstream Community Mesh Operations

Slipstream's direct QUIC mesh is usable between reachable machines now. It provides a persistent Ed25519 transport identity, a signed challenge/response binding to the X25519 encryption key, bounded request frames, process-wide replay protection, and sealed request/result payloads.

## Privacy boundary

Traffic is encrypted, but the selected worker decrypts and sees the plaintext prompt during inference. The worker must see the prompt because this release does not claim FHE, blind inference, or a trusted execution environment. Do not send credentials, private source code, medical/legal data, or other secrets to an unknown worker. Sensitive and Secret requests stay local by default in the app.

## Start a private worker

On a reachable Linux server or Mac mini:

```bash
cargo build --locked --release --bin slipstream-node
./target/release/slipstream-node mesh-serve \
  --listen /ip4/0.0.0.0/udp/9003/quic-v1 \
  --mode private \
  --key node.key \
  --models slipstream \
  --engine auto
```

Allow inbound UDP 9003 only from intended peers. The worker prints a complete multiaddress such as `/ip4/192.0.2.10/udp/9003/quic-v1/p2p/12D3...`, a 64-character signing `identity`, and an encryption key ID.

## Send a pinned sealed job

```bash
./target/release/slipstream-node mesh-send-job \
  --peer /ip4/192.0.2.10/udp/9003/quic-v1/p2p/12D3... \
  --expected-peer-id WORKER_SIGNING_IDENTITY \
  --key client.key \
  --model slipstream \
  --max-tokens 128 \
  --prompt "Explain this public, non-sensitive text"
```

Keep `--expected-peer-id` for private peers: it prevents a valid but unexpected worker identity from receiving the prompt. The first run creates key files with owner-only permissions.

## Free community donation

Community work is initially free and does not use the demo credit faucet or settlement path. A worker accepts community jobs only when both flags are present:

```bash
./target/release/slipstream-node mesh-serve \
  --listen /ip4/0.0.0.0/udp/9003/quic-v1 \
  --mode community \
  --donate-capacity \
  --key node.key \
  --models slipstream \
  --engine auto
```

Donation remains bounded by concurrency, token, frame, per-peer rate, queue, and replay limits. Omitting `--donate-capacity` advertises no public capacity.

## Current network boundary

This guide covers the **community mesh** (`mesh-serve` / `mesh-send-job`). It supports direct QUIC connections to a known, reachable multiaddress. It does not yet deploy a public bootstrap fleet, Kademlia discovery, NAT hole punching, or relay fallback. Machines behind inbound NAT need a UDP port mapping, VPN/private overlay, or a directly reachable host until those validation gates are complete. Relays, once enabled, will forward ciphertext and cannot decrypt Slipstream's sealed payloads; the selected inference worker still can.

**"Your Macs" (v0.6, experimental) is a different, private path** and never touches the community mesh: one Mac uses the model of another Mac **you own**, in the same local network. See [`USE_YOUR_OTHER_MAC.md`](USE_YOUR_OTHER_MAC.md). Its boundary in short:

- Only Macs paired with a 6-digit code connect (SPAKE2 pairing, pinned identities, HMAC-protected trust store). Unpaired peers are disconnected before any request is read. `NodeMode::Paired` never donates capacity to the community mesh.
- Discovery is local only (mDNS `_slipstream._udp`, TXT records carry only a version, a fingerprint and whether a pairing code is open — no names, models or RAM). A manual address works too, e.g. over your own VPN; there is no NAT traversal and nothing is relayed through a server.
- The editor talks to `127.0.0.1:8080` on its own Mac; the forwarder requires this install's local API key and refuses requests from web pages (every browser origin except the app's own window). Only an allowlist of OpenAI endpoints is forwarded (`POST /slots/*` file writes, `POST /props` and everything else are refused on both Macs).
- Traffic between the Macs is encrypted (QUIC TLS 1.3 with pinned identities plus an authenticated per-stream key exchange). The sharing Mac decrypts and processes the prompt, so whoever administers that Mac could see it — the same as for any worker.
- Tested on one Mac with two instances only. Not yet tested between two real Macs: the macOS Local Network permission prompt and firewall with the shipped app, real Wi-Fi (incl. access-point isolation), sleep/wake and Wake-on-LAN, Wi-Fi/Ethernet roaming.
