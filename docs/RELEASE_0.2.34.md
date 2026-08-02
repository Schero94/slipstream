# Slipstream 0.2.34

**Canonical product repository:** `Schero94/slipstream`

## Highlights

- **One coding contract:** Tools, `tool_choice`, JSON mode, and `json_schema`
  now use the same Slipstream Chat UI and OpenAI-compatible request shape on
  Metal/llama.cpp and MLX/oMLX.
- **Bounded MLX structured requests:** Tools/JSON automatically select the
  internal `contract` profile (capacity 2048, hot 1024, I/O 16). A running
  incompatible MLX profile asks for one safe restart instead of sending a request
  that can exceed the Metal working-set watermark.
- **Grammar-complete runtime:** the standalone MLX bootstrap installs and verifies
  `xgrammar==0.2.3` and `apache-tvm-ffi==0.1.11` without pulling the unused
  roughly 2-GiB PyTorch dependency.
- **Expert-cache correctness:** active-bank experts can no longer be selected as
  LRU victims while that bank is being assembled.
- **Reproducible bundle inputs:** the small launcher/bootstrap/lock/runtime text
  resources are now versioned in the public product repository.

## Live evidence behind the release

| Engine / check | Result |
|---|---|
| llama.cpp Granite SSE | TTFT 0.008 s; usage present |
| llama.cpp Granite JSON Schema | valid, 0.119 s |
| llama.cpp cancellation → recovery | 0.030 s first chunk → 0.001 s recovery |
| oMLX Qwen JSON Schema | valid; 12.238 s wall; 1.9 tok/s server |
| oMLX Qwen tool call | `calculator({"expression":"7*6"})`; 16.475 s; 1.6 tok/s |
| oMLX process after both calls | about 10.0 GiB RSS; no memory-pressure abort |

The bundled Granite model accepted Metal tool requests but did not choose a tool.
That is sufficient for protocol coverage, not a claim about Metal tool-selection
quality. oMLX performed the complete generated tool-call roundtrip.

## Metal Q5 qualification

`Qwen3.6-35B-A3B-UD-Q5_K_XL` is qualified for the exact operating point
cache=10 GiB, headroom=3 GiB, I/O 4, compact slots, draft disabled:

- two consecutive PASS runs;
- 5.834–6.773 tok/s decode across 32/64-token requests;
- 14.494 GiB peak RSS;
- 0 swapouts and identical Thinking hashes.

The immediately preceding run failed the unchanged reclaim gate because 613
swap-ins were already occurring. It remains recorded as a failure; no safety gate
was weakened for this release.

## Validation provenance

The source loop before canonical port completed:

- 22/22 Slipstream UI contract scripts;
- 191 MLX/PGRN tests;
- 83 Tauri/Rust tests;
- 14/14 llama.cpp/PGRN tests including model E2E;
- 810 oMLX grammar/tool/engine tests passing, 3 skipped;
- browser journeys at 1440×1000 and 1024×768 without overflow or console errors.

Canonical repository and release-bundle gate results are appended after the final
v0.2.34 build.

## Artifacts

The GitHub Release publishes both:

- `Slipstream_0.2.34_aarch64.dmg`
- `Slipstream_0.2.34_aarch64.zip`

SHA-256 digests are recorded in the Release body after the final build.

## Build

```bash
cd app/src-tauri
cargo tauri build
```

The release remains unsigned/unnotarized. On first launch use right-click → Open.
