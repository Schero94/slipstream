# Slipstream 0.3.1

**Canonical product repository:** `Schero94/slipstream`

## Highlights

- **Pure native stack:** Slipstream remains llama.cpp/PGRN plus oMLX/PGRN. It
  does not bundle Ollama or create a second model store.
- **One measured API contract:** both engines are qualified with the same
  OpenAI-compatible streaming harness for exact chat, strict JSON Schema and a
  specifically selected tool call.
- **Verified runtime capsule:** the app reports every required bundled runtime
  component and refuses to start an incomplete native engine.
- **Storage-aware by default:** internal, external and network model locations
  are identified before placement; free-space reserves prevent unsafe copies.
- **Bounded oMLX prefix cache:** the automatic SSD cache budget now respects
  current free space and a 3 GiB reserve instead of using 10% of total volume
  capacity on a nearly full system disk.
- **Headless Linux stays qualified:** the exact release engine builds without
  Metal or curl and passes all portable PGRN tests.

## Measured engine contract

Every case ran twice at temperature zero, completed its SSE stream through
`[DONE]`, produced a stable semantic hash and left swap unchanged.

| Engine / model location | Plain | Strict JSON | Forced `add(19,23)` |
| --- | ---: | ---: | ---: |
| oMLX/PGRN, Qwen internal SSD | 5.58 tok/s | 4.07 tok/s | 3.21 tok/s |
| llama.cpp/PGRN, Qwen external SSD | 5.84 tok/s | 4.90 tok/s | 6.18 tok/s |

The llama result comes from a clean static arm64 build of exact engine commit
`0c716f30be270d1fb1077a1a0795684e6faeecf8`. It fixes llama.cpp's rejection of
OpenAI's specific-function `tool_choice` object and validates the requested
function against the supplied schemas. The source patch reconstructs cleanly
from the pinned upstream base.

The large llama Qwen result is deliberately labelled external-SSD: only about
10 GiB was free internally, so copying a roughly 22 GiB GGUF would have violated
the reserve. A small internal Granite model proved endpoint, SSE, PGRN and JSON
mechanics but did not follow the exact-chat and tool protocol, so it is not
advertised as tool-compatible. The large oMLX Qwen model is qualified on the
internal SSD.

## Validation

- 183/183 complete Rust workspace tests pass.
- 96/96 Tauri/Rust tests pass.
- All Python runtime/manifest/storage/qualification contracts, all six Node UI
  contracts and the atomic oMLX bootstrap shell test pass.
- Linux engine gate: 14 PGRN objects linked and 12/12 portable tests passed.
- Real engine requests: oMLX 6/6 and llama.cpp 6/6 passed with deterministic
  output and zero swap growth.
- GitHub Pages passed a real-browser gate at 1440×1000 and 390×844: v0.3.1 is
  present, horizontal overflow is zero and the console has no warnings/errors.
- DMG CRC and ZIP integrity pass. The mounted app contains version 0.3.1, 477
  resource files, zero Python bytecode caches and arm64 app/server/converter.
- `/Applications/Slipstream.app` was upgraded from 0.3.0 to 0.3.1. A real
  launch produced an on-screen 1000×800 main window; quit released the process
  and ports 8080/8081 remained free.

## Dependency-audit boundary

The embedded upstream llama server UI reports 21 npm advisories in its complete
development lock tree. The five critical entries are Vitest/browser test tools;
`tar`, `sharp`, Sass/Immutable and PostCSS findings are build-only. SvelteKit's
reported remote-form issues do not apply to the static adapter output. The
direct DOMPurify advisory requires `CUSTOM_ELEMENT_HANDLING`, which this UI does
not enable. No applicable runtime exploit path was found; the raw count is
recorded here so build-only advisories are not mistaken for a clean audit.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.3.1_aarch64.dmg` | 67,287,899 | `4a58ce9770d68d9c2d5071e2ebb1a3d24d830c05c6695afefa3cafd50f3c54a6` |
| `Slipstream_0.3.1_aarch64.zip` | 65,743,482 | `439b818cb5484183b168dc39427b9a54fec363d966cd90adbd614c7f4423ba62` |
| `slipstream-node_0.3.1_macos_aarch64.tar.gz` | 4,340,200 | `b788a1f94f2fa1a47fedd9f5679c65f027c74066cb80e83f3ae6b60049fdad73` |

The node archive contains the arm64 `slipstream-node` binary, launchd/systemd
and non-root Docker templates, plus the community-mesh operations guide. Linux
is supported from source and by the provided Docker/systemd path; this release
does not claim a cross-compiled Linux binary or CUDA throughput.

## Known boundaries

The app remains unsigned and unnotarized. Community traffic is encrypted in
transit, but the chosen inference worker decrypts and sees the prompt; sensitive
and secret work stays local by default. Public discovery, NAT traversal and
relay fallback are not yet deployed, so community peers still need a known,
reachable address.
