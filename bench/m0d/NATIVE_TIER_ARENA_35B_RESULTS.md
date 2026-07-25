# Native PGRN 35B bounded-arena qualification

Date: 2026-07-22

## Decision

**PASS** for the first production increment: the real Qwen3.6-35B-A3B PGRN model completed two consecutive fail-closed runs with one fixed 2 GiB backend cache arena, fixed per-layer staging, an 8 GiB admission headroom, and no swap traffic.

This result qualifies the bounded HOT/WARM/COLD storage foundation. It does not claim that a hard HOT reservation improves locality: the separate real-trace replay rejected that policy, so the safe production default remains `HOT=0` and pure Clox admission until an adaptive policy beats the baseline. It also does not qualify an ANE proposal model; ANE remains a later, always-verified speculative lane.

## Exact configuration

- Model: `Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf`
- PGRN sidecar: `Qwen3.6-35B-A3B-UD-Q4_K_XL.pgrn`
- llama.cpp commit: `752c361ec4d6d05bfc523cf44471c782c813c087`
- Server SHA-256: `2997a9c241c910296a153538a67e8a03a2cb8975edc96343e02bd8a3009b7b21`
- Cache request: 2 GiB
- Reserved host headroom: 8 GiB
- Context: 512
- Batch / ubatch: 32 / 32
- MTP draft maximum: 4
- PGRN layers: 41
- Slot size: 2,301,952 bytes
- Cache capacity: 932 records
- Fixed cache allocation: 2,145,419,264 bytes
- Fixed staging allocation: 94,380,032 bytes
- Computed hard high-water bound: 2,239,799,296 bytes

## Consecutive real-model runs

| Metric | Run 1 | Run 2 |
|---|---:|---:|
| Decision | PASS | PASS |
| 64-token decode | 5.6304 tok/s | 5.7931 tok/s |
| 128-token decode | 5.5617 tok/s | 5.2581 tok/s |
| Peak process RSS | 6,022,256 KiB | 6,732,720 KiB |
| Measured maximum PGRN high-water | 2,239,804,539 B | 2,239,804,539 B |
| Maximum reclaim scratch | 4,194,304 B | 4,194,304 B |
| Free memory before / after | 50% / 52% | 55% / 55% |
| Swap-ins / swap-outs delta | 0 / 0 | 0 / 0 |
| Clean shutdown | yes | yes |
| Server exit | 0 | 0 |

Both runs produced identical deterministic response hashes and identical final routing counts:

- 13,769 cache hits / 45,634 misses, 23.18% cumulative hit rate
- 56,289 expert requests
- 108,074,987,684 bytes streamed from PGRN
- MTP: 96 accepted / 123 generated, 78.049% acceptance

The high-water observation exceeds the computed bound by 5,243 bytes because human-readable telemetry reports decimal MiB rounded to two digits and is parsed back to bytes. The runner explicitly allows one telemetry rounding unit; the allocation itself remains fixed.

## Reproducibility artifacts

- `bench/artifacts/m0d/native-pgrn-tier-arena-20260722.json`
  - SHA-256: `048fb1bd999ee0be702bbe8907093116f16dd509b8224e026b05989470895c92`
- `bench/artifacts/m0d/native-pgrn-tier-arena-20260722-r2.json`
  - SHA-256: `926d7f623490713de2030db67983d50f30a1b4617a029f8d89f7f934b3c783bc`

Command:

```sh
.venv/bin/python -m bench.m0d.run_native_streaming_ab \
  --cache-gib 2 \
  --headroom-gib 8 \
  --output <artifact.json>
```

## Safety contract now demonstrated

1. The loader admits the run only with the configured 8 GiB host reserve.
2. Expert cache storage is one fixed backend allocation, divided into disjoint per-layer slices.
3. Each layer has only one fixed-record staging allocation for cold PGRN reads.
4. Cache and staging high-water are known before generation and do not grow with token count.
5. COLD records are read on demand from the 16-KiB-aligned, CRC-checked PGRN sidecar.
6. A failed read is never published into HOT or WARM state.
7. The two real-model qualification runs caused no swap-ins or swap-outs.

## Remaining gates

- Replace the original-expert-ID compute scratch path with compact slot-ID remapping without changing logits.
- Re-run numerical parity and sustained 35B tests after that graph change.
- Export or train a compatible one-shot/tree proposal artifact before enabling Core ML CPU+Neural Engine proposals.
- Compare verified ANE proposals against the already-working verified MTP path; retain ANE only if end-to-end throughput or energy improves.
