#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOOTSTRAP="$SCRIPT_DIR/../src-tauri/resources/omlx-pgrn/bootstrap_mlx_runtime.sh"
TEST_ROOT="$(mktemp -d -t slipstream-mlx-atomic.XXXXXX)"

export SLIPSTREAM_MLX_RUNTIME="$TEST_ROOT"
export SLIPSTREAM_BOOTSTRAP_SOURCE_ONLY=1
source "$BOOTSTRAP"

make_fake_runtime() {
  local directory="$1" generation="$2"
  mkdir -p "$directory/bin"
  printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$directory/bin/python"
  chmod +x "$directory/bin/python"
  printf '%s\n' "$generation" >"$directory/generation"
}

make_fake_runtime "$VENV_DIR" old
printf '%s\n' old-ready >"$READY"
make_fake_runtime "$STAGE_VENV" new
printf '%s\n' new-ready >"$STAGE_READY"

promote_staged_runtime

[[ "$(<"$VENV_DIR/generation")" == "new" ]]
[[ "$(<"$PREVIOUS_VENV/generation")" == "old" ]]
[[ "$(<"$READY")" == "new-ready" ]]
[[ "$(<"$PREVIOUS_READY")" == "old-ready" ]]

if promote_staged_runtime 2>/dev/null; then
  echo "promotion unexpectedly accepted a missing staging runtime" >&2
  exit 1
fi
[[ "$(<"$VENV_DIR/generation")" == "new" ]]
[[ "$(<"$READY")" == "new-ready" ]]

echo "atomic bootstrap promotion: ok"
