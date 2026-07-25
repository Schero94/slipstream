# Native compact-slot 35B qualification

Date: 2026-07-22

## Decision

**REJECT AS PRODUCTION DEFAULT.** The compact PGRN path is numerically exact and executes real Qwen3.6-35B expert GEMMs directly from pinned backend-arena slots, but both consecutive real-model candidates failed the unchanged 4 MiB reclaim/pageout safety gate. It also did not improve the sustained MTP workload over the qualified bounded-arena baseline.

`--pgrn-compact-slots` therefore remains explicit and default-off. The production 35B profile remains the twice-qualified 2 GiB arena with pure Clox and full-ID staging.

## What passed

- Separate original router IDs and compact slot IDs; router probabilities are unchanged.
- Fixed, generation-bearing slot pins prevent eviction while a graph references an expert.
- Gate/up/down GGML tensors are strided views over the one backend arena; there is no second cache-sized payload allocation.
- COLD reads still use the identity-bound, CRC-checked PGRN loader and publish only after validation.
- Synthetic Qwen3.6 MoE decoder parity: exact logits on CPU and Metal.
- Synthetic Qwen3.6 MTP parity: exact logits on CPU and Metal.
- All 13 PGRN/CLI tests passed before the real-model gate.
- Both real runs completed cleanly with deterministic output hashes, no swap-ins, no swap-outs, and explicit `PGRN compact slot compute = enabled` telemetry.

## Real 35B results

Configuration for both runs:

- llama.cpp commit: `d57a8a2796b9ecb40d504459d03909fef6837fdb`
- Cache / host reserve: 2 GiB / 8 GiB
- Context / batch / ubatch: 512 / 32 / 32
- Verified MTP draft maximum: 4
- HOT reservation: 0; pure Clox
- Compact slots: enabled
- Safety limit: at most 4 MiB combined pageouts plus swap-ins, and zero swap-outs

| Metric | Candidate 1 | Candidate 2 | Qualified arena baseline |
|---|---:|---:|---:|
| Decision | FAIL: pageouts | FAIL: pageouts | PASS twice |
| 64-token decode | 5.6790 tok/s | 5.4882 tok/s | 5.6304 / 5.7931 tok/s |
| 128-token decode | 5.2906 tok/s | 5.1879 tok/s | 5.5617 / 5.2581 tok/s |
| Peak RSS | 6,150,656 KiB | 6,151,664 KiB | 6,022,256 / 6,732,720 KiB |
| Pageouts | 517 pages / 8,470,528 B | 365 pages / 5,980,160 B | 247 / 58 pages |
| Swap-ins / swap-outs | 0 / 0 | 0 / 0 | 0 / 0 |
| Free memory before / after | 54% / 53% | 51% / 51% | 50% / 52%; 55% / 55% |
| Final hit rate | 22.04% | 22.04% | 23.18% |
| Final streamed bytes | 107,461,381,980 B | 107,461,381,980 B | 108,074,987,684 B |

The compact runs were functionally deterministic, but the safety rule is deliberately stricter than “no swap”. Both failures are retained rather than relaxing the threshold after observing the result.

## Artifacts

- `bench/artifacts/m0d/native-pgrn-compact-20260722.json`
  - SHA-256: `5b5237c37b1e59b59a6c390e468698a769e8ac84c2e0407762964809a3b9e2e9`
- `bench/artifacts/m0d/native-pgrn-compact-20260722-r2.json`
  - SHA-256: `171f5471396cc2c3c200f32fc5d98e03b3caee47d4781f553b87e628b6b6a095`

Command:

```sh
.venv/bin/python -m bench.m0d.run_native_streaming_ab \
  --cache-gib 2 \
  --headroom-gib 8 \
  --compact-slots \
  --output <artifact.json>
```

## Follow-up rule

Do not make compact slots the default unless a changed implementation passes two consecutive runs under the same 4 MiB reclaim limit and demonstrates a useful end-to-end throughput or energy improvement over the qualified arena baseline. A relaxed gate alone is not sufficient.
