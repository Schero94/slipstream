# Slipstream 0.3.3

**Canonical product repository:** `Schero94/slipstream`

## Highlights

- **Complete macOS bundle seal:** the app and its resources are now ad-hoc
  signed as one bundle. The mounted DMG passes
  `codesign --verify --deep --strict` and satisfies its designated requirement.
- **Honest trust boundary:** ad-hoc signing detects bundle damage but is not an
  Apple Developer ID signature and is not notarization. Gatekeeper may still
  require right-click → Open on first launch.
- **Warm, bounded native llama path:** v0.3.2's 512 MiB prompt-cache ceiling and
  conditional tool-schema warm-up remain included. The qualified first visible
  tool TTFT is 4.73 s versus 23.36 s cold, with no swap growth.
- **No runtime expansion:** Slipstream remains llama.cpp/PGRN + oMLX/PGRN. It
  bundles no Ollama runtime and creates no second model store.

## Why this patch exists

The previous app launched successfully, but strict whole-bundle verification
found only Rust's linker-signed Mach-O and no resource seal. Official Tauri 2
guidance specifies `bundle.macOS.signingIdentity: "-"` for ad-hoc signing. A
release-contract test now pins this configuration so an unsealed app cannot
quietly return.

The rebuilt app contains `Contents/_CodeSignature/CodeResources`, passes strict
deep verification both before packaging and after mounting the DMG, and passes
a real installed launch/quit smoke. `spctl` still rejects it because no Apple
Developer identity or notarization ticket is claimed.

## Validation

- 183/183 complete Rust workspace tests pass.
- 96/96 Tauri/Rust tests pass.
- 40/40 Python runtime, storage, signing and qualification tests pass.
- All six Node UI contracts and the atomic oMLX bootstrap test pass.
- The v0.3.3 Pages source passes Chromium at 1440×1000 and 390×844 with the
  seal boundary and measured TTFT visible, zero overflow and no console errors.
- DMG CRC, mounted-app deep signature verification and ZIP integrity pass.
- The mounted app contains version 0.3.3, 477 resource files, all 16 required
  runtime components, no Python bytecode cache and no Ollama payload.
- App, llama server, PGRN converter, pinned uv and headless node are arm64.
- `/Applications/Slipstream.app` was upgraded from 0.3.2 to 0.3.3. Its deep
  signature verifies, a real launch/quit passed and ports 8080/8081 remained
  free. Both prior app versions remain recoverable from the user's Trash.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.3.3_aarch64.dmg` | 67,320,475 | `88e511a5dc83f1b64b6ebc9f7685376c145c3e66f74b3e57cd1b6cf936f490ef` |
| `Slipstream_0.3.3_aarch64.zip` | 65,758,148 | `cdb9b07c9d2c6663f1c762b0932c215838fe3aff26d24ce81deeecc172da6a6f` |
| `slipstream-node_0.3.3_macos_aarch64.tar.gz` | 4,341,131 | `6cc2e21332ed04218f7d2c86c7063884fee662f01c74a0ca3152388ed3d2bbf9` |

The node archive reports version 0.3.3 and contains the arm64 binary, launchd,
systemd and non-root Docker templates plus the community-mesh operations guide.
Linux remains supported from source and through the documented Docker/systemd
path; this release does not claim a cross-compiled Linux binary or CUDA result.

## Known boundaries

The app is ad-hoc signed but not Developer ID signed or notarized. Community
traffic is encrypted in transit, but the chosen inference worker decrypts and
sees the prompt; sensitive and secret work stays local by default. Public
discovery, NAT traversal and relay fallback are not yet deployed, so community
peers still need a known, reachable address.
