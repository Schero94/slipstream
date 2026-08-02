#!/usr/bin/env bash
# Launch forked oMLX with PGRN SSD expert staging.
#
# Layout-agnostic: works from the repo checkout OR from a staged/bundled
# Slipstream.app resource folder (resources/omlx-pgrn/).
#
# Python resolution (first match):
#   1. SLIPSTREAM_MLX_RUNTIME / OMLX_APP_RESOURCES override
#   2. ~/Library/Application Support/Slipstream/mlx-runtime (bootstrap)
#   3. /Applications/oMLX.app (legacy fallback)
#
# Bootstrap once: resources/omlx-pgrn/bootstrap_mlx_runtime.sh
#   or: tools/pgrn-mlx/bootstrap_mlx_runtime.sh
set -euo pipefail

# Clear polluted oMLX shell exports before we set our own.
unset PYTHONHOME || true

HERE="$(cd "$(dirname "$0")" && pwd)"
DEFAULT_MLX_RUNTIME="${HOME}/Library/Application Support/Slipstream/mlx-runtime"
OMLX_APP_DEFAULT="/Applications/oMLX.app/Contents/Resources"

resolve_layout() {
  # Bundled / staged: HERE contains omlx/, libpgrn_host.dylib, pgrn_host.py
  if [[ -d "$HERE/omlx/pgrn" && -f "$HERE/libpgrn_host.dylib" ]]; then
    FORK_PARENT="$HERE"
    LIB="${SLIPSTREAM_PGRN_LIB:-$HERE/libpgrn_host.dylib}"
    HOST_PY="${SLIPSTREAM_PGRN_HOST:-$HERE/pgrn_host.py}"
    return 0
  fi
  # Dev checkout: tools/pgrn-mlx → repo/vendor/omlx + native/build dylib
  local repo
  repo="$(cd "$HERE/../.." && pwd)"
  if [[ -d "$repo/vendor/omlx/omlx/pgrn" ]]; then
    FORK_PARENT="$repo/vendor/omlx"
    LIB="${SLIPSTREAM_PGRN_LIB:-$HERE/native/build/libpgrn_host.dylib}"
    HOST_PY="${SLIPSTREAM_PGRN_HOST:-$HERE/pgrn_host.py}"
    return 0
  fi
  echo "Slipstream oMLX+PGRN layout not found next to $HERE" >&2
  echo "  expected bundled omlx/ + libpgrn_host.dylib, or vendor/omlx in a checkout" >&2
  exit 1
}

# Sets: PY, PYTHON_MODE (venv|omlx_app), MLX_SITE (omlx_app only), PYTHONHOME_VAL
resolve_python() {
  local rt app py site

  if [[ -n "${SLIPSTREAM_MLX_RUNTIME:-}" ]]; then
    rt="${SLIPSTREAM_MLX_RUNTIME}"
    for py in "$rt/venv/bin/python" "$rt/bin/python" "$rt/bin/python3"; do
      if [[ -x "$py" ]]; then
        PY="$py"
        PYTHON_MODE="venv"
        MLX_SITE=""
        PYTHONHOME_VAL=""
        return 0
      fi
    done
    echo "SLIPSTREAM_MLX_RUNTIME set but no python under $rt" >&2
    exit 1
  fi

  if [[ -n "${OMLX_APP_RESOURCES:-}" ]]; then
    app="${OMLX_APP_RESOURCES}"
    py="$app/Python/cpython-3.11/bin/python3"
    site="$app/Python/framework-mlx-base/lib/python3.11/site-packages"
    if [[ -x "$py" ]]; then
      PY="$py"
      PYTHON_MODE="omlx_app"
      MLX_SITE="$site"
      PYTHONHOME_VAL="$app/Python/cpython-3.11"
      return 0
    fi
    echo "OMLX_APP_RESOURCES set but python missing at $py" >&2
    exit 1
  fi

  # Preferred: Slipstream Application Support runtime (no oMLX.app).
  rt="${DEFAULT_MLX_RUNTIME}"
  if [[ -f "$rt/READY" ]]; then
    for py in "$rt/venv/bin/python" "$rt/bin/python" "$rt/bin/python3"; do
      if [[ -x "$py" ]]; then
        PY="$py"
        PYTHON_MODE="venv"
        MLX_SITE=""
        PYTHONHOME_VAL=""
        return 0
      fi
    done
  fi

  # Legacy: oMLX.app bundled CPython + wheels.
  app="${OMLX_APP_DEFAULT}"
  py="$app/Python/cpython-3.11/bin/python3"
  site="$app/Python/framework-mlx-base/lib/python3.11/site-packages"
  if [[ -x "$py" ]]; then
    PY="$py"
    PYTHON_MODE="omlx_app"
    MLX_SITE="$site"
    PYTHONHOME_VAL="$app/Python/cpython-3.11"
    return 0
  fi

  echo "No MLX Python runtime found." >&2
  echo "  Install once: \"$HERE/bootstrap_mlx_runtime.sh\"" >&2
  echo "  (or install oMLX.app from https://github.com/jundot/omlx/releases)" >&2
  exit 1
}

resolve_layout
resolve_python

if [[ ! -f "$LIB" ]]; then
  echo "libpgrn_host.dylib missing at $LIB" >&2
  echo "  run: make -C tools/pgrn-mlx   (or tools/pgrn-mlx/stage_app_bundle.sh)" >&2
  exit 1
fi
if [[ ! -f "$HOST_PY" ]]; then
  echo "pgrn_host.py missing at $HOST_PY" >&2
  exit 1
fi
if [[ ! -d "$FORK_PARENT/omlx/pgrn" ]]; then
  echo "fork missing pgrn package under $FORK_PARENT/omlx" >&2
  exit 1
fi

# venv: site-packages already on sys.path — only prepend the Slipstream fork.
# oMLX.app: need PYTHONHOME + framework site-packages.
if [[ "$PYTHON_MODE" == "omlx_app" ]]; then
  export PYTHONHOME="$PYTHONHOME_VAL"
  export PYTHONPATH="$FORK_PARENT:$MLX_SITE${PYTHONPATH:+:$PYTHONPATH}"
else
  unset PYTHONHOME || true
  export PYTHONPATH="$FORK_PARENT${PYTHONPATH:+:$PYTHONPATH}"
fi

export SLIPSTREAM_PGRN="${SLIPSTREAM_PGRN:-1}"
export SLIPSTREAM_PGRN_PROFILE="${SLIPSTREAM_PGRN_PROFILE:-balanced}"
# Product default: profile io (balanced/quality=16). No silent cold-io=32 boost —
# that softened first warm vs pinned io16 (PERF_RECOVERY). Opt-in: COLD_IO_WIDTH=32.
export SLIPSTREAM_PGRN_COLD_IO_WIDTH="${SLIPSTREAM_PGRN_COLD_IO_WIDTH:-0}"
# Sticky L+1 kick after keep-hot under PREFETCH=0: default OFF (warm p50 collapsed
# to ~3 tok/s when auto-on). Opt-in A/B: SLIPSTREAM_PGRN_STICKY_AFTER_KEEP_HOT=1.
export SLIPSTREAM_PGRN_STICKY_AFTER_KEEP_HOT="${SLIPSTREAM_PGRN_STICKY_AFTER_KEEP_HOT:-0}"
# Residency: leave unset → Python/store default `touch` (interactive-safe).
# Benches must pin SLIPSTREAM_PGRN_RESIDENCY=mlock explicitly (short measured runs).
# Do not force mlock here — overnight dual-mlock freezes (CRASH_AVOIDANCE.md).
export SLIPSTREAM_PGRN_LIB="$LIB"
export SLIPSTREAM_PGRN_HOST="$HOST_PY"

# MCP tools: default OFF. Opt-in via OMLX_MCP_CONFIG, SLIPSTREAM_MCP_CONFIG, or
# --mcp-config on the serve argv (forwarded via "$@"). oMLX merges MCP tools into
# chat/messages/responses only when mcp_manager starts from a configured path.
if [[ -z "${OMLX_MCP_CONFIG:-}" && -n "${SLIPSTREAM_MCP_CONFIG:-}" ]]; then
  export OMLX_MCP_CONFIG="$SLIPSTREAM_MCP_CONFIG"
fi

# Heavy serve: singleton lock + optional max-wall watchdog (see CRASH_AVOIDANCE.md).
# Non-serve (--help etc.) stays a plain exec.
# shellcheck source=pgrn_serve_lock.sh
source "$HERE/pgrn_serve_lock.sh"

if pgrn_serve_lock_is_serve_cmd "$@"; then
  # oMLX's upstream `auto` prefix-cache limit is 10% of total filesystem
  # capacity. On a nearly full internal SSD that can exceed current free space
  # by tens of GiB. Preserve explicit user policy; otherwise cap the total
  # cache against current free bytes while keeping a 3 GiB disk reserve.
  _pgrn_cache_policy_explicit=0
  _pgrn_cache_base=""
  _pgrn_cache_dir="${OMLX_SSD_CACHE_DIR:-}"
  _pgrn_prev=""
  for _pgrn_arg in "$@"; do
    if [[ "$_pgrn_prev" == "--base-path" ]]; then _pgrn_cache_base="$_pgrn_arg"; fi
    if [[ "$_pgrn_prev" == "--paged-ssd-cache-dir" ]]; then _pgrn_cache_dir="$_pgrn_arg"; fi
    case "$_pgrn_arg" in
      --no-cache|--hot-cache-only|--paged-ssd-cache-max-size|--paged-ssd-cache-max-size=*)
        _pgrn_cache_policy_explicit=1
        ;;
      --base-path=*) _pgrn_cache_base="${_pgrn_arg#*=}" ;;
      --paged-ssd-cache-dir=*) _pgrn_cache_dir="${_pgrn_arg#*=}" ;;
    esac
    _pgrn_prev="$_pgrn_arg"
  done
  _pgrn_cache_enabled_lc="$(printf '%s' "${OMLX_CACHE_ENABLED:-true}" | tr '[:upper:]' '[:lower:]')"
  _pgrn_hot_only_lc="$(printf '%s' "${OMLX_HOT_CACHE_ONLY:-false}" | tr '[:upper:]' '[:lower:]')"
  if [[ -n "${OMLX_SSD_CACHE_MAX_SIZE:-}" \
     || "$_pgrn_cache_enabled_lc" =~ ^(false|0|no)$ \
     || "$_pgrn_hot_only_lc" =~ ^(true|1|yes)$ ]]; then
    _pgrn_cache_policy_explicit=1
  fi
  if [[ "$_pgrn_cache_policy_explicit" == "0" ]]; then
    if [[ -z "$_pgrn_cache_base" ]]; then
      if [[ -n "${OMLX_BASE_PATH:-}" ]]; then
        _pgrn_cache_base="$OMLX_BASE_PATH"
      elif [[ -s "${HOME}/Library/Application Support/oMLX/base-path" ]]; then
        _pgrn_cache_base="$(<"${HOME}/Library/Application Support/oMLX/base-path")"
      else
        _pgrn_cache_base="${HOME}/.omlx"
      fi
    fi
    _pgrn_budget_args=(--base-path "$_pgrn_cache_base" --reserve-gib "${SLIPSTREAM_OMLX_SSD_RESERVE_GIB:-3}")
    if [[ -n "$_pgrn_cache_dir" ]]; then
      _pgrn_budget_args+=(--cache-dir "$_pgrn_cache_dir")
    fi
    _pgrn_cache_action="$("$PY" "$HERE/ssd_cache_budget.py" "${_pgrn_budget_args[@]}")"
    case "$_pgrn_cache_action" in
      preserve) ;;
      disable)
        echo "Slipstream SSD cache: disabled (free space is at/below reserve)" >&2
        set -- "$@" --no-cache
        ;;
      *[!0-9]*|'')
        echo "invalid Slipstream SSD cache budget: $_pgrn_cache_action" >&2
        exit 2
        ;;
      *)
        echo "Slipstream SSD cache: capped at ${_pgrn_cache_action} bytes (reserve=${SLIPSTREAM_OMLX_SSD_RESERVE_GIB:-3} GiB)" >&2
        set -- "$@" --paged-ssd-cache-max-size "${_pgrn_cache_action}B"
        ;;
    esac
  fi

  _pgrn_port="$(pgrn_serve_lock_parse_port "$@")"
  _pgrn_own_lock=1
  if [[ "${SLIPSTREAM_PGRN_ALLOW_PARALLEL:-0}" == "1" ]]; then
    _pgrn_own_lock=0
  fi
  pgrn_serve_lock_acquire "$_pgrn_port" || exit $?

  # Agent API: expose model_alias (default slipstream) in oMLX model_settings.json
  # so clients that hardcode Metal's --alias keep working on MLX. Disable with
  # SLIPSTREAM_OMLX_MODEL_ALIAS=0|off|false|none|disabled|no. Best-effort — never blocks serve.
  _alias_flag="${SLIPSTREAM_OMLX_MODEL_ALIAS:-slipstream}"
  _alias_flag_lc="$(printf '%s' "$_alias_flag" | tr '[:upper:]' '[:lower:]')"
  if [[ -n "$_alias_flag" \
     && "$_alias_flag_lc" != "0" \
     && "$_alias_flag_lc" != "off" \
     && "$_alias_flag_lc" != "false" \
     && "$_alias_flag_lc" != "none" \
     && "$_alias_flag_lc" != "disabled" \
     && "$_alias_flag_lc" != "no" \
     && "$_alias_flag_lc" != "-" ]]; then
    _alias_base=""
    _alias_model_dir=""
    _alias_model_id="${SLIPSTREAM_OMLX_MODEL_ID:-}"
    _prev=""
    for _a in "$@"; do
      if [[ "$_prev" == "--base-path" ]]; then _alias_base="$_a"; fi
      if [[ "$_prev" == "--model-dir" ]]; then _alias_model_dir="$_a"; fi
      if [[ "$_prev" == "--model" ]]; then _alias_model_id="$_a"; fi
      _prev="$_a"
    done
    if [[ -z "$_alias_base" ]]; then
      _alias_base="${HOME}/Library/Application Support/Slipstream/omlx-pgrn"
    fi
    _alias_args=(--base-path "$_alias_base")
    if [[ -n "$_alias_model_id" ]]; then
      _alias_args+=(--model-id "$_alias_model_id")
    elif [[ -n "$_alias_model_dir" ]]; then
      _alias_args+=(--model-dir "$_alias_model_dir")
    fi
    if [[ ${#_alias_args[@]} -ge 3 ]]; then
      /usr/bin/python3 "$HERE/ensure_mlx_model_alias.py" "${_alias_args[@]}" \
        2>/dev/null || true
    fi
  fi

  # Keep bash as parent so EXIT/TERM kills the Python child and drops the lock.
  # (exec would drop traps — overnight orphans were part of the freeze story.)
  "$PY" -m omlx.cli "$@" &
  _pgrn_child=$!
  if [[ "$_pgrn_own_lock" == "1" ]]; then
    pgrn_serve_lock_write "$_pgrn_child" "$_pgrn_port"
  fi

  _pgrn_wd=""
  _pgrn_max_wall="${SLIPSTREAM_PGRN_MAX_WALL_SEC:-0}"
  if [[ "$_pgrn_max_wall" =~ ^[0-9]+$ ]] && [[ "$_pgrn_max_wall" -gt 0 ]]; then
    (
      sleep "$_pgrn_max_wall"
      echo "SLIPSTREAM_PGRN_MAX_WALL_SEC=$_pgrn_max_wall elapsed — stopping serve pid=$_pgrn_child" >&2
      kill "$_pgrn_child" 2>/dev/null || true
    ) &
    _pgrn_wd=$!
  fi

  _pgrn_cleanup_done=0
  _pgrn_serve_cleanup() {
    [[ "$_pgrn_cleanup_done" == "1" ]] && return 0
    _pgrn_cleanup_done=1
    if [[ -n "$_pgrn_wd" ]]; then
      kill "$_pgrn_wd" 2>/dev/null || true
      wait "$_pgrn_wd" 2>/dev/null || true
    fi
    if kill -0 "$_pgrn_child" 2>/dev/null; then
      kill "$_pgrn_child" 2>/dev/null || true
      wait "$_pgrn_child" 2>/dev/null || true
    fi
    if [[ "$_pgrn_own_lock" == "1" ]]; then
      pgrn_serve_lock_release "$_pgrn_child"
    fi
  }
  trap '_pgrn_serve_cleanup' EXIT INT TERM HUP

  set +e
  wait "$_pgrn_child"
  _pgrn_rc=$?
  set -e
  _pgrn_serve_cleanup
  trap - EXIT INT TERM HUP
  exit "$_pgrn_rc"
fi

exec "$PY" -m omlx.cli "$@"
