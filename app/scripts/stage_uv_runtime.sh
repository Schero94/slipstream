#!/usr/bin/env bash
# Atomically stage the manifest-pinned uv arm64 binary for the macOS bundle.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_TARGET="$SCRIPT_DIR/../src-tauri/resources/omlx-pgrn/uv"
TARGET="${SLIPSTREAM_UV_TARGET:-$DEFAULT_TARGET}"
SOURCE="${SLIPSTREAM_UV_SOURCE:-}"
EXPECTED="uv 0.11.10"

uv_version_ok() {
  local actual
  actual="$("$1" --version)"
  [[ "$actual" == "$EXPECTED" || "$actual" == "$EXPECTED "* ]]
}

if [[ -z "$SOURCE" ]]; then
  SOURCE="$(command -v uv 2>/dev/null || true)"
fi
if [[ -z "$SOURCE" || ! -x "$SOURCE" ]]; then
  echo "uv source missing; set SLIPSTREAM_UV_SOURCE=/path/to/uv" >&2
  exit 1
fi
if ! uv_version_ok "$SOURCE"; then
  echo "uv version mismatch: expected '$EXPECTED', got '$("$SOURCE" --version)'" >&2
  exit 1
fi
if ! file "$SOURCE" | grep -q "Mach-O 64-bit executable arm64"; then
  echo "uv source is not a macOS arm64 executable: $SOURCE" >&2
  exit 1
fi

mkdir -p "$(dirname "$TARGET")"
NEXT="${TARGET}.next"
install -m 0755 "$SOURCE" "$NEXT"
if ! uv_version_ok "$NEXT"; then
  echo "staged uv verification failed" >&2
  exit 1
fi
mv "$NEXT" "$TARGET"
echo "staged $EXPECTED → $TARGET"
