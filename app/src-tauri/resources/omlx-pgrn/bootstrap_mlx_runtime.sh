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

HERE="$(cd "$(dirname "$0")" && pwd)"
REQ="${SLIPSTREAM_MLX_REQUIREMENTS:-$HERE/requirements-mlx-runtime.txt}"
GRAMMAR_REQ="${SLIPSTREAM_MLX_GRAMMAR_REQUIREMENTS:-$HERE/requirements-mlx-grammar.txt}"
DEFAULT_ROOT="${HOME}/Library/Application Support/Slipstream/mlx-runtime"
ROOT="${SLIPSTREAM_MLX_RUNTIME:-$DEFAULT_ROOT}"
STATUS_JSON="$ROOT/status.json"
READY="$ROOT/READY"
LOG="$ROOT/bootstrap.log"
VENV_DIR="$ROOT/venv"
VENV_PY="$VENV_DIR/bin/python"
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

core_runtime_ready() {
  [[ -f "$READY" && -x "$VENV_PY" ]] || return 1
  "$VENV_PY" -c "import mlx, mlx_lm, fastapi, uvicorn" >/dev/null 2>&1
}

grammar_runtime_ready() {
  [[ -x "$VENV_PY" ]] || return 1
  "$VENV_PY" -c "import importlib.metadata as m; raise SystemExit(0 if m.version('xgrammar') == '0.2.3' and m.version('apache-tvm-ffi') == '0.1.11' else 1)" >/dev/null 2>&1
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

find_python311() {
  local c p
  for c in python3.11 /opt/homebrew/bin/python3.11 /usr/local/bin/python3.11; do
    p=""
    if command -v "$c" >/dev/null 2>&1; then
      p="$(command -v "$c")"
    elif [[ -x "$c" ]]; then
      p="$c"
    else
      continue
    fi
    if "$p" -c 'import sys; raise SystemExit(0 if sys.version_info[:2]==(3,11) else 1)' 2>/dev/null; then
      echo "$p"
      return 0
    fi
  done
  return 1
}

ensure_uv() {
  local uv_bin
  if uv_bin="$(find_uv)"; then
    echo "$uv_bin"
    return 0
  fi
  write_status "installing" "Downloading uv package manager…" 5
  curl -LsSf https://astral.sh/uv/install.sh | sh >>"$LOG" 2>&1
  find_uv
}

create_venv() {
  local uv_bin py
  mkdir -p "$ROOT"
  rm -rf "$VENV_DIR"
  if uv_bin="$(find_uv)"; then
    :
  else
    uv_bin="$(ensure_uv)" || uv_bin=""
  fi
  if [[ -n "$uv_bin" ]]; then
    write_status "installing" "Creating Python 3.11 venv via uv…" 10
    "$uv_bin" python install 3.11 >>"$LOG" 2>&1 || true
    "$uv_bin" venv --python 3.11 "$VENV_DIR" >>"$LOG" 2>&1
    return 0
  fi
  if py="$(find_python311)"; then
    write_status "installing" "Creating Python 3.11 venv…" 10
    "$py" -m venv "$VENV_DIR" >>"$LOG" 2>&1
    return 0
  fi
  echo "Need Python 3.11 or uv. Install: brew install python@3.11   or   brew install uv" >&2
  write_status "failed" "Need Python 3.11 or uv (brew install python@3.11 / uv)." 0
  return 1
}

pip_install() {
  local uv_bin
  if [[ ! -f "$REQ" ]]; then
    write_status "failed" "requirements file missing: $REQ" 0
    echo "missing requirements: $REQ" >&2
    return 1
  fi
  write_status "installing" "Downloading MLX wheels + deps (one-time, may take several minutes)…" 25
  if uv_bin="$(find_uv)"; then
    "$uv_bin" pip install --python "$VENV_PY" -r "$REQ" >>"$LOG" 2>&1
  else
    "$VENV_PY" -m pip install --upgrade pip >>"$LOG" 2>&1
    "$VENV_PY" -m pip install -r "$REQ" >>"$LOG" 2>&1
  fi
  pip_install_grammar
  write_status "installing" "Verifying imports…" 90
}

pip_install_grammar() {
  local uv_bin
  if [[ ! -f "$GRAMMAR_REQ" ]]; then
    write_status "failed" "grammar requirements file missing: $GRAMMAR_REQ" 0
    echo "missing grammar requirements: $GRAMMAR_REQ" >&2
    return 1
  fi
  write_status "installing" "Installing lightweight structured-output support…" 85
  if uv_bin="$(find_uv)"; then
    "$uv_bin" pip install --python "$VENV_PY" --no-deps -r "$GRAMMAR_REQ" >>"$LOG" 2>&1
  else
    "$VENV_PY" -m pip install --no-deps -r "$GRAMMAR_REQ" >>"$LOG" 2>&1
  fi
}

verify_and_mark() {
  if ! "$VENV_PY" -c "import mlx, mlx_lm, fastapi, uvicorn, numpy"; then
    write_status "failed" "Import verification failed — see $LOG" 0
    return 1
  fi
  if ! grammar_runtime_ready; then
    write_status "failed" "Structured-output dependency verification failed — see $LOG" 0
    return 1
  fi
  local mlx_ver
  mlx_ver="$("$VENV_PY" -c "import importlib.metadata as m; print(m.version('mlx'))")"
  {
    echo "mlx=$mlx_ver"
    echo "xgrammar=0.2.3"
    echo "python=$("$VENV_PY" -c 'import sys; print(sys.version.split()[0])')"
    echo "ready_unix=$(date +%s)"
  } >"$READY"
  write_status "ready" "Slipstream mlx-runtime ready (mlx $mlx_ver)." 100
  echo "ready: mlx $mlx_ver → $ROOT"
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
  trap 'rm -f "$LOCK"' EXIT
  echo "$$ $(date +%s)" >"$LOCK"
  if core_runtime_ready && ! grammar_runtime_ready; then
    : >"$LOG"
    write_status "installing" "Upgrading MLX runtime for structured output…" 80
    pip_install_grammar
    verify_and_mark
    return 0
  fi
  write_status "installing" "Starting MLX runtime bootstrap…" 1
  create_venv
  pip_install
  verify_and_mark
}

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
