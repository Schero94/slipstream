# PGRN I/O levers — how to A/B (opt-in, default-off)

Three fetch-bandwidth levers, all **env-gated and default-off** so the shipped
default behaviour is byte-identical. Each is parity-safe by construction (the
default read path is unchanged; the mirror returns identical bytes and every
expert read is CRC-checked). Speedups must be **measured** on a real model —
they are not asserted here.

Built into `src/peregrine_pgrn.c`. Rebuild the engine:
```
cmake --build build-static --target llama-server -j
```

## #1 — Dual-SSD striping (the biggest fetch lever)
Put a **byte-identical copy** of the `.pgrn` on a *second* fast disk, then:
```
PGRN_MIRROR=/Volumes/SecondSSD/model.pgrn ./llama-server --pgrn /fast/model.pgrn ...
```
Reads stripe ~50/50 across the two fds by aligned block, so two SSDs' read
bandwidth sums. A missing / wrong-size mirror is ignored (stays single-disk);
a corrupt mirror is caught per-read by CRC. Costs 2× disk space.

**A/B:** same model, same cache, run once without `PGRN_MIRROR` and once with.
Compare decode tok/s and the SSD-throughput line.
```
python -m bench.m0d.run_native_streaming_ab           # baseline (single disk)
PGRN_MIRROR=/Volumes/SecondSSD/model.pgrn \
  python -m bench.m0d.run_native_streaming_ab         # striped
```
Expected direction: if decode is fetch-bound (it is, ~78–92%) and the two
disks are similar speed, decode tok/s should rise toward ~1.5–2×. Verify.

## #3 — Buffered vs direct reads
Default is `F_NOCACHE` (direct, uncached — best on real NVMe). On a DRAM-less /
non-NVMe external SSD the page cache + readahead can be smoother:
```
PGRN_BUFFERED=1 ./llama-server --pgrn /path/model.pgrn ...
```
**A/B:** run with and without `PGRN_BUFFERED=1` on the *external* SSD case.
Watch memory-health (buffered grows the OS page cache — confirm the gate/ampel
stays green).

## #2 — KV persistence (already in the fork, no new code)
- In-session prompt/KV caching is **default-on** → follow-up agent turns are warm.
- Cross-restart persistence: launch with `--prompt-cache <file>` (writes KV to
  disk; trades SSD writes for a warm reopen). Opt-in — not auto-enabled, to keep
  streaming read-only.

## Parity gate (run before shipping any lever)
```
ctest -R test-peregrine-model-e2e     # asserts streamed == resident (NMSE=0)
```
