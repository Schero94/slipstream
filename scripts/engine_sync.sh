#!/usr/bin/env bash
# Sync work done inside the reconstructed tree (llama.cpp-slipstream/) back into the
# repo's source of truth: Slipstream-owned files -> engine/, upstream edits -> the seams
# patch. Then optionally prove the round-trip with a fresh apply.sh.
#
# usage: scripts/engine_sync.sh [--verify]
#   --verify  reconstruct into a temp dir (local clone of the tree's upstream objects)
#             and diff it against the working tree; exits non-zero on any mismatch.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
TREE="$HERE/llama.cpp-slipstream"
PIN=$(sed -n 's/^PIN=//p' "$HERE/apply.sh")
[ -d "$TREE/.git" ] || { echo "no reconstructed tree at $TREE (run ./apply.sh)"; exit 1; }
cd "$TREE"

# Every path that differs from the pinned upstream commit, including untracked files.
changed=$( { git diff --name-only "$PIN"; git ls-files --others --exclude-standard; } \
           | grep -v -E '^(build|build-static|build-[^/]*)/' | sort -u)

owned=()     # -> engine/
upstream=()  # -> patch
for f in $changed; do
  if [ -e "$HERE/engine/$f" ]; then
    owned+=("$f")
  elif ! git cat-file -e "$PIN:$f" 2>/dev/null && [[ "$(basename "$f")" =~ ^(peregrine|pgrn) ]]; then
    owned+=("$f")          # new Slipstream-owned file
  else
    upstream+=("$f")       # edit to an upstream file, or a new non-owned file
  fi
done

for f in "${owned[@]}"; do
  if [ -e "$f" ]; then
    mkdir -p "$HERE/engine/$(dirname "$f")"
    cp "$f" "$HERE/engine/$f"
  else
    rm -f "$HERE/engine/$f"   # deleted in the tree
  fi
done

# Regenerate the patch from the pin to the working tree for upstream paths only.
# New untracked upstream-side files need intent-to-add so `git diff` sees them.
for f in "${upstream[@]}"; do
  if [ -e "$f" ] && ! git ls-files --error-unmatch "$f" >/dev/null 2>&1; then git add -N "$f"; fi
done
if [ ${#upstream[@]} -gt 0 ]; then
  git diff --binary "$PIN" -- "${upstream[@]}" > "$HERE/patches/slipstream-seams.patch"
fi
echo "engine/: ${#owned[@]} owned files synced; patch: ${#upstream[@]} upstream files"

if [ "${1:-}" = "--verify" ]; then
  TMP=$(mktemp -d "${TMPDIR:-/tmp}/slipstream-verify.XXXXXX")
  ( cd "$HERE" && UPSTREAM="$TREE" ./apply.sh "$TMP/tree" >/dev/null )
  if diff -rq -x .git -x 'build*' -x '.claude' -x '__pycache__' "$TMP/tree" "$TREE" ; then
    echo "VERIFY PASS: apply.sh reproduces the working tree exactly"
    rm -rf "$TMP"
  else
    echo "VERIFY FAIL: reconstructed tree differs (kept at $TMP/tree)"; exit 1
  fi
fi
