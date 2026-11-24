---
name: engine-sync
description: Sync engine changes made inside the gitignored llama.cpp-slipstream/ tree back into engine/ and patches/slipstream-seams.patch, and verify apply.sh reproduces the tree. Use before every commit that touches engine code, and whenever you finish an engine experiment you want to keep.
---

# Sync engine work back to the repo

`llama.cpp-slipstream/` is reconstructed and gitignored — anything not synced is lost.

```bash
scripts/engine_sync.sh --verify
```

- Files that already live under `engine/`, and new files named `peregrine*`/`pgrn*`,
  are copied to `engine/<same path>`.
- Every other difference from the pinned upstream commit (edits to upstream files,
  other new files) is regenerated into `patches/slipstream-seams.patch`.
- `--verify` rebuilds a fresh tree via `apply.sh` (local clone) and diffs it against the
  working tree; it must print `VERIFY PASS`.

Then review `git diff --stat engine patches` — it should contain only the change you
intend to ship. Rejected experiments must be reverted in the tree *before* syncing
(the patch is regenerated from the whole tree).
