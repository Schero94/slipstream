# Slipstream 0.3.0

**Canonical product repository:** `Schero94/slipstream`

## Highlights

- **Headless Linux and macOS node:** `slipstream-node` ships with hardened
  systemd, launchd and non-root Docker templates.
- **Direct authenticated QUIC:** `mesh-serve` and `mesh-send-job` use a
  persistent libp2p Ed25519 identity bound to Slipstream's signed Hello and
  X25519 encryption key.
- **Free, explicit community donation:** public capacity is accepted only with
  `--mode community --donate-capacity`; it never uses the demo credit faucet.
- **Safe Mac controls:** local/private/community mode, donation and remote Chat
  are separate controls and default off. Local inference always wins when ready.
- **Fail-closed security:** process-wide bounded replay cache, one-shot
  challenges, encrypted results only, token/frame/concurrency/per-peer limits,
  and identity pinning.
- **Evidence-first vendor queue:** 15 current Colibri, oMLX and llama.cpp PRs are
  SHA-pinned but remain unqualified until deterministic A/B gates pass.

## Privacy boundary

Transport and Slipstream payloads are encrypted, so passive observers cannot
read prompts. The selected inference worker decrypts and sees plaintext. This is
not FHE, blind inference or a trusted enclave. Sensitive and Secret requests stay
local by default; never send secrets or private code to an unknown worker.

## Measured validation

- 183/183 complete Rust workspace tests pass at the released version.
- 83/83 Tauri/Rust tests, 5/5 Mac UI contracts, 3/3 packaged-resource
  checks and 6/6 vendor-harness tests pass.
- Real two-process QUIC community smoke: signed/pinned worker, sealed request,
  5/5 deterministic MockEngine tokens, sealed result; a wrong pin failed before
  inference.
- 4/4 mesh adapter tests cover stable transport identity, Local bind refusal,
  QUIC/Hello identity binding, one-shot challenge and sealed-only result.
- Headless asset, Mac server/privacy, prefer-local Chat and JavaScript syntax
  contracts pass.
- Vendor A/B harness: no vendor candidate is represented as accepted without a
  real model measurement.
- GitHub Pages was exercised in a real browser at 1440x1000 and 390x844: the
  v0.3.0 section is visible, horizontal overflow is zero and the browser console
  has no warnings or errors.
- The packaged app was inspected after mounting the DMG: version 0.3.0, arm64
  app/server/converter, 475 oMLX runtime files, 113 MiB installed bundle and
  88 MiB embedded oMLX runtime. The DMG CRC and ZIP integrity checks pass.
- `/Applications/Slipstream.app` was upgraded to 0.3.0, launched and quit
  cleanly without enabling a server or community donation.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.3.0_aarch64.dmg` | 47,067,086 | `ea93e177220a66e6505f30390cf2b84ae78c7817cda94803773a6294e64082d1` |
| `Slipstream_0.3.0_aarch64.zip` | 45,476,496 | `158358bd7e72834c294a1ae522c6e9e405499ae309f1f79adbd9fbc09655886d` |
| `slipstream-node_0.3.0_macos_aarch64.tar.gz` | 4,341,222 | `e8d9af8cc63b88d6ccbbe1883154e2d3efd1b8645f06200d95cd532ead97bc5a` |

The headless tarball includes the node binary, launchd/systemd/Docker templates
and the community-mesh operations guide. Linux is supported from source and by
the provided non-root Docker/systemd path; this release does not claim a
cross-compiled Linux binary artifact.

## Use the direct mesh

See [`COMMUNITY_MESH_OPERATIONS.md`](COMMUNITY_MESH_OPERATIONS.md) for exact
Linux/Mac/Docker commands and firewall guidance.

## Known boundary

Version 0.3.0 connects to a known reachable QUIC multiaddress. It does not yet
ship a public bootstrap fleet, Kademlia discovery, AutoNAT/DCUtR hole punching,
or relay fallback. NATed hosts need a UDP mapping, VPN/private overlay or a
directly reachable address. The macOS app remains unsigned and unnotarized.
