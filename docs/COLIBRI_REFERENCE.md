# Colibrì Reference Contract

Last verified: 2026-07-17

Upstream: [`JustVugg/colibri`](https://github.com/JustVugg/colibri)  
Local read-only reference: `vendor/colibri`  
Verified upstream refs: `main@72d3d372`, `dev@8b36736`

## Permanent project rule

Colibrì is the first implementation reference for Peregrine. Every milestone task
must begin with an upstream audit and record one status:

- 🟢 merged upstream: adapt with attribution;
- 🟡 open upstream PR: port only after reviewing its tests and current status;
- 🔵 paper or measured recipe: reproduce behind an A/B gate;
- 🔴 absent upstream: new Peregrine work with oracle-first tests.

The audit is not permission to copy model-specific assumptions. Colibrì targets
GLM-5.2 and primarily CPU/CUDA storage tiers; Peregrine's development target is
Qwen3.6-35B-A3B on Apple Metal with a 36 GB unified-memory ceiling. Shape, router,
attention, quantization, and operating-system behavior must be revalidated.

Colibrì is Apache-2.0. Copied or substantially adapted code must retain source
headers, name the upstream commit/PR, and be added to Peregrine's NOTICE record.

## M0 routing

### M0a: expert locality

- 🟢 PR [#176](https://github.com/JustVugg/colibri/pull/176) supplies route-trace,
  cross-layer pair-table, held-out coupling measurement, and optional coupled
  prefetch patterns.
- 🟢 PR [#199](https://github.com/JustVugg/colibri/pull/199) supplies opt-in
  `CACHE_ROUTE` max-rank routing and agreement telemetry.
- The M0a collection baseline must keep cache-aware routing **off**. `CACHE_ROUTE`
  changes selected experts and would contaminate measurement of the model's native
  locality. It becomes a later A/B candidate after the native trace is analyzed.
- Reuse colibrì's trace/pair-table/reporting ideas where their geometry is generic;
  retain Peregrine's validated binary format, session isolation, hashes, and corrupt
  tail checks.

### M0b: sparse-attention recall

- Colibrì's implemented DSA path is useful as measurement and sparse-selection
  reference, but it is GLM-5.2-specific and is not evidence that Quest-style ranking
  works for Qwen3.6.
- Peregrine still requires its own Q/K snapshot oracle and Recall@{32,64,128} at
  64K-128K before any sparse-attention implementation is admitted.

### M0c: disk reality

- 🟢 Start from `vendor/colibri/c/iobench.c`.
- Parameterize the actual Peregrine expert-record size, currently approximately
  1.5-1.6 MB, and run eight-thread random reads.
- On macOS, `F_NOCACHE` prevents new cache population but does not evict pages already
  cached. Use a fresh never-read file for every cold run; never run buffered then
  label the following direct run as disk evidence.

## Later reusable components

- `c/glm.c`: RAM admission, tier/pin/LRU management, batch-union MoE, speculative
  verification, and status metrics. Port mechanisms, not GLM tensor geometry.
- `c/grammar.h` and `c/schema_gbnf.h`: grammar drafts and JSON-schema conversion.
  The filename is `grammar.h`, not `gramma.h`.
- `c/openai_server.py` and `c/coli`: single-model admission, OpenAI-compatible server,
  FIFO behavior, and `plan`/`doctor` interface patterns.
- Converter: resumable shard-at-a-time conversion and immediate source-shard cleanup.
- KV persistence, async readahead, PILOT/coupled prefetch, cache-aware routing, and
  experiment reporting are reference implementations for later gated ports.
- 🟡 Dev-only commits `8bf4cb9` and `7eb2393` keep `coli serve` resident across
  chat sessions and add PID/stop lifecycle. W1 adapts that process-lifecycle
  pattern around llama.cpp; colibrì's GLM protocol and cache geometry are not
  copied.

## Upstream audit procedure

Before starting a new milestone component:

```bash
git -C vendor/colibri fetch --all --prune
git -C vendor/colibri log -1 --oneline origin/main
git -C vendor/colibri log -1 --oneline origin/dev
git -C vendor/colibri log --all --oneline -- <relevant-path>
```

Then record the reviewed upstream commit and PR in the design or results document.
Never modify or build inside the detached reference checkout unless the active task
explicitly requires a disposable experiment.
