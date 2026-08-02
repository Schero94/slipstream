#!/usr/bin/env bash
# One-click Slipstream MLX runtime (CPython 3.11 + MLX wheels).
#
# Installs into:
#   ~/Library/Application Support/Slipstream/mlx-runtime
# Canonical Python (launchers prefer this):
#   ~/Library/Application Support/Slipstream/mlx-runtime/venv/bin/python
# (Not mlx-runtime/bin/python — that path is only a legacy dual-check fallback.)
#
# Replaces the Gate E dependency on /Applications/oMLX.app for
# resources/omlx-pgrn/run_omlx_pgrn.sh. oMLX.app remains a fallback.
#
# Usage:
#   bootstrap_mlx_runtime.sh              # install (idempotent)
#   bootstrap_mlx_runtime.sh --status     # human status
#   bootstrap_mlx_runtime.sh --json       # machine status for the app UI
#   bootstrap_mlx_runtime.sh --verify     # import smoke only
#
# Override root: SLIPSTREAM_MLX_RUNTIME=/path
set -euo pipefail

# oMLX.app shells often export PYTHONHOME; that breaks system/Homebrew python.
unset PYTHONHOME PYTHONPATH || true

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REQ_LOCK="${SLIPSTREAM_MLX_REQUIREMENTS:-$HERE/requirements-mlx-runtime.lock}"
GRAMMAR_REQ="${SLIPSTREAM_MLX_GRAMMAR_REQUIREMENTS:-$HERE/requirements-mlx-grammar.txt}"
OMLX_FORK_ROOT="${SLIPSTREAM_OMLX_FORK:-$HERE}"
DEFAULT_ROOT="${HOME}/Library/Application Support/Slipstream/mlx-runtime"
ROOT="${SLIPSTREAM_MLX_RUNTIME:-$DEFAULT_ROOT}"
STATUS_JSON="$ROOT/status.json"
READY="$ROOT/READY"
LOG="$ROOT/bootstrap.log"
VENV_DIR="$ROOT/venv"
VENV_PY="$VENV_DIR/bin/python"
STAGE_VENV="$ROOT/venv.next"
STAGE_PY="$STAGE_VENV/bin/python"
STAGE_READY="$ROOT/READY.next"
PREVIOUS_VENV="$ROOT/venv.previous"
PREVIOUS_READY="$ROOT/READY.previous"
LOCK="$ROOT/bootstrap.lock"
OMLX_PY="/Applications/oMLX.app/Contents/Resources/Python/cpython-3.11/bin/python3"

# Prefer a clean interpreter for helpers (never the broken PYTHONHOME shell).
HOST_PY=""
for c in /opt/homebrew/bin/python3.11 /usr/bin/python3 /opt/homebrew/bin/python3 python3; do
  if [[ -x "$c" ]] || command -v "$c" >/dev/null 2>&1; then
    p="$(command -v "$c" 2>/dev/null || echo "$c")"
    if env -u PYTHONHOME -u PYTHONPATH "$p" -c 'print(1)' >/dev/null 2>&1; then
      HOST_PY="$p"
      break
    fi
  fi
done
[[ -n "$HOST_PY" ]] || HOST_PY="/usr/bin/python3"

json_escape() {
  env -u PYTHONHOME -u PYTHONPATH "$HOST_PY" -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1"
}

write_status() {
  local state="$1"
  local detail="${2:-}"
  local pct="${3:-0}"
  mkdir -p "$ROOT"
  cat >"$STATUS_JSON" <<EOF
{
  "state": $(json_escape "$state"),
  "detail": $(json_escape "$detail"),
  "percent": ${pct},
  "root": $(json_escape "$ROOT"),
  "python": $(json_escape "$VENV_PY"),
  "provider": $(json_escape "runtime"),
  "omlx_app": $([[ -x "$OMLX_PY" ]] && echo true || echo false),
  "updated_unix": $(date +%s)
}
EOF
}

core_runtime_ready_for() {
  local py="$1" marker="$2"
  [[ -f "$marker" && -x "$py" ]] || return 1
  "$py" -c "import mlx, mlx_lm, fastapi, uvicorn" >/dev/null 2>&1
}

grammar_runtime_ready_for() {
  local py="$1"
  [[ -x "$py" ]] || return 1
  "$py" -c "import importlib.metadata as m; raise SystemExit(0 if m.version('xgrammar') == '0.2.3' and m.version('apache-tvm-ffi') == '0.1.11' else 1)" >/dev/null 2>&1
}

core_runtime_ready() {
  core_runtime_ready_for "$VENV_PY" "$READY"
}

grammar_runtime_ready() {
  grammar_runtime_ready_for "$VENV_PY"
}

runtime_ready() {
  core_runtime_ready && grammar_runtime_ready
}

emit_json() {
  local omlx_app="false"
  [[ -x "$OMLX_PY" ]] && omlx_app="true"

  if runtime_ready; then
    local mlx_ver detail
    mlx_ver="$("$VENV_PY" -c "import importlib.metadata as m; print(m.version('mlx'))" 2>/dev/null || echo "?")"
    detail="Slipstream mlx-runtime ready (mlx ${mlx_ver}; no oMLX.app required)."
    cat <<EOF
{
  "state": "ready",
  "detail": $(json_escape "$detail"),
  "percent": 100,
  "root": $(json_escape "$ROOT"),
  "python": $(json_escape "$VENV_PY"),
  "provider": "runtime",
  "mlx_version": $(json_escape "$mlx_ver"),
  "omlx_app": ${omlx_app},
  "updated_unix": $(date +%s)
}
EOF
    return 0
  fi

  if core_runtime_ready && ! grammar_runtime_ready; then
    cat <<EOF
{
  "state": "upgrade_required",
  "detail": "MLX runtime is installed; structured-output support needs a small one-time upgrade.",
  "percent": 90,
  "root": $(json_escape "$ROOT"),
  "python": $(json_escape "$VENV_PY"),
  "provider": "runtime",
  "omlx_app": ${omlx_app},
  "updated_unix": $(date +%s)
}
EOF
    return 0
  fi

  if [[ -f "$STATUS_JSON" ]]; then
    # Refresh omlx_app flag; keep other fields from the installer.
    env -u PYTHONHOME -u PYTHONPATH "$HOST_PY" - "$STATUS_JSON" "$omlx_app" <<'PY'
import json, sys, time
path, omlx = sys.argv[1], sys.argv[2] == "true"
with open(path) as f:
    d = json.load(f)
d["omlx_app"] = omlx
d.setdefault("provider", "none" if d.get("state") == "missing" else "runtime")
d["updated_unix"] = int(time.time())
print(json.dumps(d))
PY
    return 0
  fi

  cat <<EOF
{
  "state": "missing",
  "detail": $(json_escape "MLX runtime not installed — one-time download of wheels (~0.5–1 GiB)."),
  "percent": 0,
  "root": $(json_escape "$ROOT"),
  "python": $(json_escape "$VENV_PY"),
  "provider": "none",
  "omlx_app": ${omlx_app},
  "updated_unix": $(date +%s)
}
EOF
}

emit_status() {
  if runtime_ready; then
    local mlx_ver
    mlx_ver="$("$VENV_PY" -c "import importlib.metadata as m; print(m.version('mlx'))" 2>/dev/null || echo "?")"
    echo "ready: $ROOT (mlx $mlx_ver)"
    return 0
  fi
  if core_runtime_ready && ! grammar_runtime_ready; then
    echo "upgrade_required: install structured-output support with: $0 install"
    return 0
  fi
  if [[ -f "$STATUS_JSON" ]]; then
    env -u PYTHONHOME -u PYTHONPATH "$HOST_PY" - "$STATUS_JSON" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print(f"{d.get('state','?')}: {d.get('detail','')}")
PY
    return 0
  fi
  echo "missing: $ROOT — run: $0"
}

find_uv() {
  if [[ -n "${SLIPSTREAM_UV_BIN:-}" && -x "${SLIPSTREAM_UV_BIN}" ]]; then
    echo "$SLIPSTREAM_UV_BIN"
    return 0
  fi
  if [[ -x "$HERE/uv" ]]; then
    echo "$HERE/uv"
    return 0
  fi
  if command -v uv >/dev/null 2>&1; then
    command -v uv
    return 0
  fi
  for c in "$HOME/.local/bin/uv" /opt/homebrew/bin/uv /usr/local/bin/uv; do
    if [[ -x "$c" ]]; then
      echo "$c"
      return 0
    fi
  done
  return 1
}

assert_managed_runtime_path() {
  case "$ROOT" in
    ""|"/"|"."|"$HOME"|"$HOME/"|"/Users"|"/Volumes"|"/tmp"|"/private"|"/private/tmp")
      echo "refusing unsafe MLX runtime root: $ROOT" >&2
      return 1
      ;;
  esac
  [[ "$ROOT" == /* ]] || {
    echo "MLX runtime root must be absolute: $ROOT" >&2
    return 1
  }
  case "$1" in
    "$STAGE_VENV"|"$PREVIOUS_VENV") return 0 ;;
    *) echo "refusing unmanaged runtime path: $1" >&2; return 1 ;;
  esac
}

clear_managed_runtime_dir() {
  local target="$1"
  assert_managed_runtime_path "$target"
  [[ ! -e "$target" ]] || rm -rf -- "$target"
}

clear_staging() {
  clear_managed_runtime_dir "$STAGE_VENV"
  rm -f -- "$STAGE_READY"
}

create_staged_venv() {
  local uv_bin
  mkdir -p "$ROOT"
  clear_staging
  if ! uv_bin="$(find_uv)"; then
    echo "Verified uv binary missing. Slipstream releases bundle uv; development override: SLIPSTREAM_UV_BIN=/path/to/uv" >&2
    write_status "failed" "Verified uv binary missing; reinstall Slipstream or set SLIPSTREAM_UV_BIN." 0
    return 1
  fi
  write_status "installing" "Creating isolated Python 3.11 staging runtime…" 10
  "$uv_bin" python install 3.11 >>"$LOG" 2>&1 || true
  "$uv_bin" venv --python 3.11 "$STAGE_VENV" >>"$LOG" 2>&1
  if [[ ! -x "$STAGE_PY" ]]; then
    write_status "failed" "Staging Python creation failed — see $LOG" 0
    return 1
  fi
}

install_locked_runtime() {
  local uv_bin
  if [[ ! -f "$REQ_LOCK" ]]; then
    write_status "failed" "runtime lock missing: $REQ_LOCK" 0
    echo "missing runtime lock: $REQ_LOCK" >&2
    return 1
  fi
  if ! uv_bin="$(find_uv)"; then
    write_status "failed" "Verified uv binary missing." 0
    return 1
  fi
  write_status "installing" "Installing SHA/commit-locked MLX runtime…" 25
  "$uv_bin" pip install --python "$STAGE_PY" -r "$REQ_LOCK" >>"$LOG" 2>&1
  install_grammar "$STAGE_PY"
  write_status "installing" "Verifying staged runtime…" 90
}

install_grammar() {
  local target_py="$1" uv_bin
  if [[ ! -f "$GRAMMAR_REQ" ]]; then
    write_status "failed" "grammar requirements file missing: $GRAMMAR_REQ" 0
    echo "missing grammar requirements: $GRAMMAR_REQ" >&2
    return 1
  fi
  if ! uv_bin="$(find_uv)"; then
    write_status "failed" "Verified uv binary missing." 0
    return 1
  fi
  write_status "installing" "Installing pinned structured-output support…" 85
  "$uv_bin" pip install --python "$target_py" --no-deps -r "$GRAMMAR_REQ" >>"$LOG" 2>&1
}

verify_runtime() {
  local py="$1"
  if [[ ! -f "$OMLX_FORK_ROOT/omlx/_torch_stub.py" ]]; then
    echo "oMLX torch stub missing: $OMLX_FORK_ROOT/omlx/_torch_stub.py" >&2
    return 1
  fi
  PYTHONPATH="$OMLX_FORK_ROOT" "$py" - <<'PY'
import importlib.metadata as metadata
import sys

if sys.version_info[:2] != (3, 11):
    raise SystemExit(f"expected Python 3.11, got {sys.version.split()[0]}")
from omlx._torch_stub import install as install_torch_stub
install_torch_stub()
for module in ("mlx", "mlx_lm", "fastapi", "uvicorn", "numpy", "xgrammar"):
    __import__(module)
expected = {
    "mlx": "0.32.0",
    "xgrammar": "0.2.3",
    "apache-tvm-ffi": "0.1.11",
}
actual = {name: metadata.version(name) for name in expected}
if actual != expected:
    raise SystemExit(f"runtime version mismatch: expected={expected!r} actual={actual!r}")
PY
}

write_stage_marker() {
  local mlx_ver python_ver
  mlx_ver="$("$STAGE_PY" -c "import importlib.metadata as m; print(m.version('mlx'))")"
  python_ver="$("$STAGE_PY" -c 'import sys; print(sys.version.split()[0])')"
  {
    echo "mlx=$mlx_ver"
    echo "xgrammar=0.2.3"
    echo "python=$python_ver"
    echo "ready_unix=$(date +%s)"
  } >"$STAGE_READY"
}

restore_previous_runtime() {
  [[ ! -e "$VENV_DIR" && -e "$PREVIOUS_VENV" ]] && mv "$PREVIOUS_VENV" "$VENV_DIR"
  [[ ! -e "$READY" && -e "$PREVIOUS_READY" ]] && mv "$PREVIOUS_READY" "$READY"
}

promote_staged_runtime() {
  [[ -x "$STAGE_PY" && -f "$STAGE_READY" ]] || {
    echo "staged runtime is incomplete; refusing promotion" >&2
    return 1
  }
  clear_managed_runtime_dir "$PREVIOUS_VENV"
  rm -f -- "$PREVIOUS_READY"
  [[ ! -e "$VENV_DIR" ]] || mv "$VENV_DIR" "$PREVIOUS_VENV"
  [[ ! -e "$READY" ]] || mv "$READY" "$PREVIOUS_READY"
  if ! mv "$STAGE_VENV" "$VENV_DIR"; then
    restore_previous_runtime
    return 1
  fi
  if ! mv "$STAGE_READY" "$READY"; then
    mv "$VENV_DIR" "$STAGE_VENV"
    restore_previous_runtime
    return 1
  fi
  if ! runtime_ready; then
    mv "$VENV_DIR" "$STAGE_VENV"
    mv "$READY" "$STAGE_READY"
    restore_previous_runtime
    echo "promoted runtime failed active verification; restored previous runtime" >&2
    return 1
  fi
}

verify_stage_and_promote() {
  if ! verify_runtime "$STAGE_PY"; then
    write_status "failed" "Staged runtime verification failed — active runtime unchanged; see $LOG" 0
    return 1
  fi
  write_stage_marker
  if ! promote_staged_runtime; then
    write_status "failed" "Atomic runtime activation failed; previous runtime restored." 0
    return 1
  fi
  local mlx_ver
  mlx_ver="$("$VENV_PY" -c "import importlib.metadata as m; print(m.version('mlx'))")"
  write_status "ready" "Slipstream mlx-runtime ready (mlx $mlx_ver)." 100
  echo "ready: mlx $mlx_ver → $ROOT"
}

cleanup_install() {
  rm -f -- "$LOCK"
  if [[ -e "$STAGE_VENV" || -e "$STAGE_READY" ]]; then
    clear_staging
  fi
}

fail_without_touching_active() {
  write_status "failed" "Runtime installation failed; previous runtime remains active — see $LOG" 0
  return 1
}

do_install() {
  mkdir -p "$ROOT"
  : >"$LOG"
  if [[ -f "$LOCK" ]]; then
    local age now mtime
    now="$(date +%s)"
    mtime="$(stat -f %m "$LOCK" 2>/dev/null || echo 0)"
    age=$(( now - mtime ))
    if [[ "$age" -lt 7200 ]]; then
      echo "bootstrap already running (lock age ${age}s) — see $STATUS_JSON" >&2
      emit_json
      return 0
    fi
  fi
  if runtime_ready; then
    emit_json
    return 0
  fi
  trap cleanup_install EXIT
  echo "$$ $(date +%s)" >"$LOCK"
  write_status "installing" "Starting MLX runtime bootstrap…" 1
  create_staged_venv || fail_without_touching_active
  install_locked_runtime || fail_without_touching_active
  verify_stage_and_promote || fail_without_touching_active
}

if [[ "${SLIPSTREAM_BOOTSTRAP_SOURCE_ONLY:-0}" == "1" ]]; then
  return 0 2>/dev/null || exit 0
fi

cmd="${1:-install}"
case "$cmd" in
  --status|status) emit_status ;;
  --json|json) emit_json ;;
  --verify|verify)
    if runtime_ready; then
      echo "verify ok"
      exit 0
    fi
    echo "verify failed" >&2
    exit 1
    ;;
  install|--install|"")
    do_install
    ;;
  *)
    echo "usage: $0 [--status|--json|--verify|install]" >&2
    exit 2
    ;;
esac
