# Verified ANE proposal lane: 35B artifact blocker

Date: 2026-07-22

## Outcome

The bounded native Core ML adapter and its target-verification wiring are implemented, but the Qwen3.6-35B checkpoint cannot honestly enable the proposed ANE one-shot/tree lane yet. Its GGUF contains one sequential MTP/nextn prediction block and no supported Medusa/EAGLE/tree heads. There is also no identity-bound `.mlmodel`, `.mlpackage`, or compiled `.mlmodelc` artifact in the project or model cache.

The existing verified Metal MTP path therefore remains the production speculative decoder. No sequential MTP weights were relabelled as an ANE tree model.

## Native adapter delivered

llama.cpp commits: `a7fb3ac23`, `df65a7493`, `293aec339`, `2677a4232`

- `peregrine_ane.h/.mm` exposes a proposal-only C API.
- Core ML configuration requests `MLComputeUnitsCPUAndNeuralEngine` on macOS 13+.
- Source-model SHA-256 and architecture must match Core ML creator metadata.
- Input and output must be fixed-shape `Int32` multi-arrays with exact declared sizes.
- Compiled package plus fixed input/candidate storage must fit the admitted byte budget before load.
- A canonical SHA-256 over sorted relative paths and regular-file bytes binds the complete compiled package contents.
- Production loading requires a strict `peregrine-ane-v1` manifest. Unknown keys, non-integer sizes, path traversal, package-hash mismatch, source mismatch, architecture mismatch, invalid tensor contracts, and budget overflow fail closed.
- Draft depth, width, target vocabulary and precision are explicit. The current runtime admits only width-1 one-shot-linear output; depth must equal the output tensor count, vocabulary must equal the target GGUF, and precision must match Core ML creator metadata.
- Candidate storage is allocated once and remains bounded.
- Load, identity, type, shape, prediction, or output failures return an error and no candidates.
- `--pgrn-ane-draft MANIFEST` enables a default-off, one-shot-linear `draft-ane` implementation; `--pgrn-ane-budget-mib N` sets its hard byte admission ceiling.
- The adapter has no API capable of committing or accepting a token. It only fills the normal `slot.spec_draft`; the unchanged server target batch evaluates those tokens and `common_sampler_sample_and_accept_n` accepts only the target-model prefix, rolling back at the first mismatch.
- An explicitly requested ANE lane that cannot be admitted now aborts server startup instead of silently degrading.
- `test-peregrine-ane`, `test-peregrine-admission`, the PGRN tests, and `test-arg-parser` pass; `llama-server` links successfully with Core ML and Foundation.

## Real 35B fail-closed startup proof

The exact admitted 35B GGUF/PGRN pair was started with 2 GiB cache, 8 GiB reserved headroom, an explicit ANE manifest and a 256 MiB ANE ceiling. The manifest was bound to source SHA `55983c5a75a1ab969824077b3bb3de4146e82a9234072b48ad4e8f92ad3fe9f1`; its deliberately absent compiled package produced:

```text
failed to initialize speculative decoding context: ANE draft admission failed: Core ML package is absent
explicit ANE draft request failed closed; server startup aborted
exiting due to model loading error
```

No server remained listening, and no swapout occurred. The reproducible negative fixture is `bench/m0d/ane_missing_artifact_manifest.json`.

`bench/m0d/export_ane_tree_draft.py` creates the same canonical package hash and writes the strict manifest atomically, but only after an eligibility audit explicitly proves compatible trained heads. It never fabricates or converts weights.

## Real checkpoint evidence

The native GGUF header audit reports:

```json
{
  "eligible": false,
  "nextn_predict_layers": 1,
  "reason": "sequential MTP/nextn weights are present, but no supported one-shot tree heads",
  "relevant_metadata": {
    "qwen35moe.nextn_predict_layers": 1
  },
  "relevant_tensors": [
    "blk.40.nextn.eh_proj.weight",
    "blk.40.nextn.enorm.weight",
    "blk.40.nextn.hnorm.weight",
    "blk.40.nextn.shared_head_norm.weight"
  ]
}
```

The audit is reproducible and fail-closed:

```sh
.venv/bin/python -m bench.m0d.audit_ane_tree_source \
  /Users/schero/.cache/peregrine/models/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf
```

Exit code `2` means “not eligible”; malformed headers also fail rather than guessing.

## What is required to unblock ANE

1. Train or obtain compatible multi-head one-shot/tree draft weights bound to this exact 35B checkpoint.
2. Export them as a fixed-shape Core ML model with creator metadata containing the source GGUF SHA-256 and `qwen35moe` architecture, then generate the immutable manifest and canonical package hash.
3. Run token-parity tests with the real artifact under acceptance, forced rejection, timeout, and adapter failure.
4. Run repeated paired ANE-vs-MTP 35B tests. ANE is enabled only if it improves end-to-end throughput or energy without violating the same memory/pageout gates.

Until those trained weights exist, claiming a “smart NPU draft” would be technically false. The safe runtime uses Metal-verified MTP and the ANE adapter remains dormant.
