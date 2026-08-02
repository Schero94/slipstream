# Vendor A/B Qualification

This gate prevents attractive upstream PR claims from entering Slipstream without local quality and performance evidence. It never checks out, rebases, patches, or cleans a vendor tree. You provide two already-built, clean, distinct Git worktrees pinned to the manifest SHAs.

## Evidence contract

The configured command runs once for warmup and at least three measured times in each worktree. Its final stdout line must be one JSON object:

```json
{"output":"deterministic generated text","tok_s":18.7,"ttft_ms":842.1,"peak_rss_mb":23750,"swap_delta_mb":0}
```

The runner hashes and discards `output`; artifacts never contain generated text. It rejects nondeterminism, baseline/candidate output mismatch, missing metrics, dirty worktrees, wrong SHAs, swap activity, or threshold regressions.

## Configure and run one candidate

Every newly audited entry intentionally has an empty `command` and status `unqualified`. Add the exact model-specific benchmark argv only after both isolated worktrees are built. Never weaken output parity merely to admit a speed result.

```bash
python3 bench/vendor/run_candidate_ab.py \
  --candidate omlx-2442-specprefill-trailing-window \
  --baseline-worktree /absolute/vendor/omlx-baseline \
  --candidate-worktree /absolute/vendor/omlx-pr-2442 \
  --output bench/artifacts/vendor/omlx-2442.json \
  --markdown bench/artifacts/vendor/omlx-2442.md \
  --warmups 1 --repeats 5
```

Exit status is `0` for accepted, `2` for measured rejection, and `3` for missing/untrustworthy evidence. A candidate remains `unqualified` until its exact model, quantization, prompt, context, seed, thermal state, memory reserve, and artifact decision are recorded in `bench/RESULTS.md`.
