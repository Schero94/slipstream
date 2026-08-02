#!/usr/bin/env bash
# Apply small Slipstream API overlays after staging the pinned full oMLX fork.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET="${SLIPSTREAM_OMLX_TARGET:-$REPO_ROOT/app/src-tauri/resources/omlx-pgrn}"
PATCH_FILE="$REPO_ROOT/patches/omlx/0001-enforce-openai-tool-choice.patch"
SERVER="$TARGET/omlx/server.py"
HELPER="$TARGET/omlx/api/forced_tool_choice.py"

[[ -f "$SERVER" ]] || { echo "staged oMLX server missing: $SERVER" >&2; exit 1; }
[[ -f "$HELPER" ]] || { echo "Slipstream tool-choice overlay missing: $HELPER" >&2; exit 1; }
[[ -f "$PATCH_FILE" ]] || { echo "oMLX patch missing: $PATCH_FILE" >&2; exit 1; }

if ! grep -q "enforce_tool_choice" "$SERVER"; then
  (cd "$TARGET" && /usr/bin/patch --batch --forward -p1 <"$PATCH_FILE")
fi
python3 -m py_compile "$SERVER" "$HELPER"
echo "oMLX overlays: ready"
