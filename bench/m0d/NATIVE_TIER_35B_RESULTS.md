# Native HOT/WARM/COLD policy replay — 2026-07-22

## Contract

- Model identity: `55983c5a75a1ab969824077b3bb3de4146e82a9234072b48ad4e8f92ad3fe9f1`
- Geometry: 40 routed layers, 256 experts, top-8
- Input: `bench/artifacts/m0d-live-observe/live-observe-routing.bin`
- Input SHA-256: `b6e75de7d771d39cddcf58a517ef2c68f38a97bf85d96a4f6f6243ba5b1b0d66`
- Counted decode accesses: 397,440
- Cache budget: 2 GiB total, 1,213 expert slots at 1,769,472 bytes/expert
- Native policy commit: `61ad840c13b8e7c87e4586098ffab8bb1a111a4d`
- Native driver SHA-256: `f1bd75f65c12ee5e7702194e1784b40ac3d88f74b08e73977ef971d8ae4cbe3d`
- Immutable JSON SHA-256: `fef74c04b327a688e4ab2f980a5ba5950dabc7ffac011679aef26f49b4256574`

Command:

```sh
.venv/bin/python -m bench.m0d.native_tier_replay \
  --cache-gib 2 \
  --output artifacts/native-tier-replay.json
```

## Result

| HOT quota | hits | misses | hit rate | promotions | demotions |
|---:|---:|---:|---:|---:|---:|
| 0% | 236,411 | 161,029 | **59.483444%** | 0 | 0 |
| 10% | 232,190 | 165,250 | 58.421397% | 100,726 | 100,635 |
| 20% | 231,354 | 166,086 | 58.211051% | 131,854 | 131,722 |
| 25% | 231,339 | 166,101 | 58.207277% | 133,567 | 133,431 |
| 33% | 231,326 | 166,114 | 58.204006% | 133,864 | 133,726 |

Decision: **KEEP_PURE_CLOX** for the qualified 2 GiB profile. Every nonzero hard
HOT reservation regressed the real trace by 1.06–1.28 percentage points and caused
high promotion/demotion churn. The native HOT/WARM state machine remains available
and bounded, but production keeps `--pgrn-hot-percent 0` until the compact single-arena
Metal path changes the memory/copy cost and passes a fresh equal-budget replay.

This is an admission decision, not a correctness failure. Temperature never changes
router IDs, expert bytes, logits, or tokens.
