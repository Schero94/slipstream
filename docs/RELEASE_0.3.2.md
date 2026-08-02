# Slipstream 0.3.2

**Canonical product repository:** `Schero94/slipstream`

## Highlights

- **Pure native stack:** Slipstream remains llama.cpp/PGRN plus oMLX/PGRN. It
  bundles no Ollama runtime and creates no second model store.
- **Bounded llama prompt cache:** the app launcher now supplies
  `--cache-ram 512`, replacing llama.cpp's 8192 MiB default with a measured
  ceiling that cannot silently consume most of a small Mac's free RAM.
- **Conditional tool-schema warm-up:** when Tools are enabled, llama.cpp warms
  Slipstream's exact `get_current_time` + `calculator` schema after readiness.
  The UI exposes warming, ready and soft-failure states; disabling Tools or
  stopping the server aborts the request.
- **Evidence before rollout:** the optimization is not copied to oMLX because
  its two qualified short-schema requests remained cold. Each engine keeps the
  path its measurements support.

## Measured effect

The product configuration was Qwen3.6-35B-A3B on an external SSD, 10 GiB PGRN
expert cache, 3 GiB RAM reserve, 512 MiB llama prompt cache, MTP and four PGRN
I/O threads.

| Request | TTFT | Cached prompt | Result |
| --- | ---: | ---: | --- |
| hidden one-token schema prime | 23.36 s | 0 / 355 | PASS |
| first visible `calculator(19+23)` | **4.73 s** | **337 / 361** | exact tool + args |
| `get_current_time()` | **2.46 s** | **337 / 355** | exact tool + `{}` |

First visible tool TTFT fell **79.7%** relative to the cold schema prefill. RSS
was 15.18 to 15.42 GiB, free+inactive memory stayed near 6.6 GiB and swap stayed
exactly 1148.12 MiB. The hidden request consumes one completion token in
llama.cpp's accounting; its generated text is discarded and never enters chat
history. A `max_tokens: 0` follow-up was rejected because the OpenAI chat
adapter still returned and accounted one token.

Evidence: `bench/results/app-tool-prime-llamacpp-20260802.json`, SHA-256
`25658bfb45ea1a722ec6a69e86061b1f1b3b0a633402f204677bc6b597610e1b`.

## Validation

- 183/183 complete Rust workspace tests pass.
- 96/96 Tauri/Rust tests pass.
- 39/39 Python runtime, storage and qualification tests pass.
- All six Node UI contracts and the atomic oMLX bootstrap test pass.
- Chromium passed desktop 1440×1000 and mobile 390×844 tool warm-up gates:
  2/2 transitions, exact request contract, no horizontal overflow and no
  console warnings or errors.
- The updated GitHub Pages source passed the same desktop/mobile browser sizes:
  v0.3.2 and the measured TTFT are visible, horizontal overflow is zero, and
  the console has no warnings or errors.
- DMG CRC and ZIP integrity pass. The mounted app contains version 0.3.2, 477
  resource files, all 16 manifest-required components, no Python bytecode cache
  and arm64 app/server/converter/uv binaries.
- `/Applications/Slipstream.app` was upgraded from 0.3.1 to 0.3.2. A real launch
  produced the registered app process; quit released it and ports 8080/8081
  remained free. The prior app is recoverable from the user's Trash.

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.3.2_aarch64.dmg` | 67,288,167 | `c89fa8a76301ba25172d34bf1d1d9ed511805ddb6fc4b75b2621a9adb027cc45` |
| `Slipstream_0.3.2_aarch64.zip` | 65,746,319 | `1a58805ac00957b136eea902ea93068434cdc0391d8a7caa56032ee1f990f388` |
| `slipstream-node_0.3.2_macos_aarch64.tar.gz` | 4,340,656 | `240e167f583df9fdf945ee33d4ddefd3c86e89abaa5198a6a4b60c1be1f0a47b` |

The node archive reports version 0.3.2 and contains the arm64 binary, launchd,
systemd and non-root Docker templates plus the community-mesh operations guide.
Linux remains supported from source and through the documented Docker/systemd
path; this release does not claim a cross-compiled Linux binary or CUDA result.

## Known boundaries

The app is unsigned and unnotarized. Community traffic is encrypted in transit,
but the chosen inference worker decrypts and sees the prompt; sensitive and
secret work stays local by default. Public discovery, NAT traversal and relay
fallback are not yet deployed, so community peers still need a known, reachable
address.
