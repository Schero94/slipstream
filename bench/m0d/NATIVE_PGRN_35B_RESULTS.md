# Native PGRN 35B qualification — 2026-07-22

## Identity and artifact

- Model: `Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf`
- GGUF bytes: 22,853,663,008
- GGUF SHA-256: `55983c5a75a1ab969824077b3bb3de4146e82a9234072b48ad4e8f92ad3fe9f1`
- PGRN bytes: 20,128,754,176
- PGRN: 41 routed layers including embedded MTP, 256 experts per layer,
  10,496 CRC-protected records, 16 KiB alignment
- Final llama.cpp commit: `24b6efd77`

## Correctness and safety

- Exact small-model resident/PGRN parity passed for Qwen3-MoE decoder,
  Qwen3.5-MoE decoder, and embedded Qwen3.5 MTP: NMSE 0 and max absolute error 0.
- Real 35B manifest geometry, quantized tensor types and per-expert byte sizes matched
  the GGUF; model identity was recomputed and checked before PGRN activation.
- Every cold expert read used `pread` + `F_NOCACHE` and verified its stored CRC before
  cache publication.
- Cache memory is fixed at construction and partitioned across routed layers. The
  high-water includes one failure-safe staging record per layer.
- Admission is fail-closed before cache/scratch allocation and re-checks the requested
  live reserve after target and MTP context allocation.
- The final product profile used a 2 GiB cache and 8 GiB live reserve. Two
  consecutive full qualification processes, each with two coding turns, passed the
  fail-closed policy and shut down cleanly. Peak RSS was 6,575.66 and 6,575.61 MiB.
- The unused 72,975,748,384-byte GLM model was deleted with user authorization. The
  admitted 35B GGUF and PGRN were retained.

## Product measurements

Stable profile: cache 2 GiB, headroom 8 GiB, context 512, batch/ubatch 32/32,
Metal offload, no warmup, embedded MTP maximum 4, thinking enabled.

| Run / turn | Prompt | Decode | Thinking chars | MTP accepted | Result |
|---|---:|---:|---:|---:|---|
| A / 64 tokens | 9.75 tok/s | 5.81 tok/s | 270 | 48/55 | pass |
| A / 128 tokens | 11.67 tok/s | 5.55 tok/s | 418 | 96/123 | pass |
| B / 64 tokens | 9.90 tok/s | 5.77 tok/s | 270 | 48/55 | pass |
| B / 128 tokens | 11.61 tok/s | 5.61 tok/s | 418 | 96/123 | pass |

The model-specific PGRN tensor directory derived an exact 2,145,419,264-byte cache,
94,380,032 bytes of per-layer failure-safe staging, and a 2,239,799,296-byte hard
high-water. Native telemetry reported 2,046.03/2,136.04 MiB after decimal rounding.
Both runs had zero swapouts; total cold-run reclaim was 1,867,776 and 2,736,128 bytes
against a 4 MiB limit. System-wide free memory changed 60% -> 60% and 61% -> 60%.
The server exited with code zero after each controlled termination.

Machine-readable final-build evidence:

- `bench/artifacts/m0d/native-pgrn-35b-20260722T034036Z.json`
- `bench/artifacts/m0d/native-pgrn-35b-20260722T034327Z.json`

The resident comparison baseline remains about 32-33 tok/s. Native bounded streaming
therefore meets the safety and functional target but does **not** match resident speed.
The earlier 10 GB observer result (95.58% warm hit rate) is not claimed for this safe
2 GiB profile; its observed cumulative hit rate reached 23.18% over two turns.

## Rejected configurations

- 8 GiB cache + 6 GiB reserve: admission refused; no allocation or fallback occurred.
- 6 GiB cache + 4 GiB reserve: initially reached 10.93 tok/s with MTP, then suffered a
  170-second subsequent prefill residency stall.
- 4 GiB cache + 6 GiB reserve with ubatch 8: 100-200 second Metal residency stalls.
- Increasing physical ubatch to 32 removed prefill stalls, but 4 GiB still produced a
  long second-decode residency stall.
- 1.75 GiB cache + 10 GiB reserve was refused by admission under the then-current live
  desktop state; no fallback or listener was started.
- Several 1-2.25 GiB trials appeared to stall in their second turn. Their server clock
  advanced by 25-275 seconds while client monotonic time and PGRN stage time did not;
  these were macOS idle-sleep artifacts and are invalid performance evidence.
- The runner now starts `caffeinate -im`, verifies it remains active, and fails closed if
  the assertion ends. With that control, 2 GiB + 8 GiB passed twice consecutively and is
  the qualified default for the current 36 GB Mac.

## Automated tests

`ctest --test-dir vendor/llama.cpp/build -R 'test-(arg-parser|peregrine-)'`
passed 11/11 tests: argument parsing, bounded stream, PGRN parser, admission, runtime,
loader, stage, scratch, system memory, SHA-256, and model E2E parity.

`python3 -m unittest tests.m0d.test_native_streaming_ab -v` passed 10/10 tests,
including malformed lifecycle, missing telemetry, forced-kill, swapout, pressure,
throughput, Thinking, MTP, model-derived high-water, and resident-memory refusal gates.
The model E2E CTest additionally proved exact resident/PGRN logits on explicitly
offloaded Metal for Qwen3-MoE, Qwen3.5-MoE, and embedded MTP (`NMSE=0`, max error 0).

## Remaining evidence boundary

A new full resident-vs-PGRN 35B logits run was deliberately not performed: loading the
resident model under the current desktop load would violate the very foreground-memory
contract being qualified. The accepted evidence is the exact small decoder/MTP parity,
the full 35B model-identity/tensor/CRC checks, the existing resident baseline, and the
real 35B native product runs above. This limitation must remain visible in any claim.
