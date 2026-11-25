# Slipstream 0.4.0 — lookahead prefetch, Turbo, current llama.cpp, a 4-click app

The streamed 35B decodes about a third faster with no change to its output, up to 86 %
faster with an opt-in mode that trades a little fidelity, and the engine now sits on
current upstream llama.cpp. The app gets a live engine cockpit and a first run that takes
four clicks.

## Speed, measured the way the app runs it

Qwen3.6-35B-A3B Q4_K_XL, GGUF and PGRN on the internal NVMe, M3 Pro / 36 GB, llama-server
with the app's flags, greedy. Interleaved A/B, 2 repetitions, 0 MB swap growth.

| Cache | 0.3.5 | 0.4.0 default (lossless) | 0.4.0 + Turbo (opt-in) |
|------:|------:|-------------------------:|-----------------------:|
| 4 GiB | 7.8 tok/s | 10.3 tok/s (+32 %) | 14.5 tok/s (+86 %) |
| 10 GiB | 13.1 tok/s | 17.5 tok/s (+33 %)\* | 19.6 tok/s (+49 %)\* |

\* measured in the server on the previous llama.cpp base; the new base adds another 3–7 %
(18.6 tok/s with lookahead at 10 GiB in llama-completion).

## Changes

### Engine

- **Lookahead prefetch, on by default.** Layer L+1's router is scored on layer L's MoE input
  inside the graph and read at the top-k sync that already exists, so the experts the next
  layer will ask for are already being read from the SSD while this layer computes. It
  predicts 83 % of them. Greedy output is byte-identical with it on or off.
  `PGRN_LOOKAHEAD=-1` turns it off; `PGRN_LOOKAHEAD=N` adds N extra predicted experts.
- **Turbo (opt-in, lossy).** While generating, the router's selection probabilities get a
  small bonus for experts that are resident or being prefetched (Cache-Prior, CECAR and PSR
  on arXiv). Mixing weights stay unmodified and the prompt is always routed exactly. Next
  token matches the unmodified model 93 % of the time; perplexity within noise. `PGRN_STEER=0.02`.
- **Ported to upstream llama.cpp `33aeb17f8586`** (2677 commits newer). Identical greedy output,
  3–7 % faster decode, all engine tests pass. Upstream now ships Laguna; our Laguna changes
  live in the seams patch. Port notes: `docs/PORT_UPSTREAM_33aeb17.md`.
- Fixes: lookahead was inert under speculative decoding (strided top-k view); the server
  aborted on prompt batches whose last layer requested no outputs; grammar-forced drafts
  aborted the server when no draft implementation had drafted.

### App

- **MTP off for streamed models with lookahead.** A multi-token verify step needs the union
  of several tokens' experts from SSD, which costs more than it saves here (28–33 % slower,
  up to 75 % with Turbo). Models that fit in RAM keep MTP.
- **First run in four clicks:** Set up model → Download & prepare (download continues into
  preparation by itself) → Start → Connect your editor.
- **Live engine cockpit:** decode speed, expert cache hit-rate, SSD throughput and free RAM
  with one-minute trends, plus one line that says what limits speed right now.
- Settings follow the Mac's light/dark mode and are grouped into Memory & speed, Turbo,
  Backend, Chat behaviour and Storage overrides, without nested panels. The cache slider shows
  the value Start will use. Lookahead and Turbo are switches with measured, plain-language
  descriptions in English and German. The old predictive prefetch toggle is hidden; it is
  replaced by lookahead.

### Project

- `CLAUDE.md` rules, project skills for build / engine sync / A/B bench,
  `scripts/engine_sync.sh --verify` (proves `apply.sh` reproduces the engine tree),
  `scripts/heavy.sh` (one heavy job at a time), and the `bench/fetchwall` harness.

## What was tried and rejected

All with numbers in `bench/RESULTS.md`: a residual-plus-next-norm predictor input (neutral),
n-gram speculation (−9 % on a rewrite), parallel cold reads and zero-copy slots for small
batches (neutral), grammar-forced drafts without MTP (−7 to −11 %), Turbo strengths of 0.02
and above without the prefetch mark (+12.5 % perplexity).

## Not verified in this release

Laguna 118B on the new llama.cpp base, adaptive draft, the routing logger, the ANE draft,
SPEC_PIN, and `--parallel` above 1.

## Download

No `.dmg` is attached to this release yet. The app bundle also needs the staged MLX runtime
components, which are not part of the repository; it will be added when it is built. Build
from source as described in the README, or keep using the 0.3.4 bundle.

## Verify

```bash
./apply.sh && scripts/engine_sync.sh --verify
cd llama.cpp-slipstream/build-static && ctest -R peregrine      # 14/14
for t in app/scripts/test_*.mjs; do node "$t"; done             # 7/7
bench/fetchwall/run_arm.sh la0 4                                # output e6c6a36552db
```
