#!/usr/bin/env bash
# Singleton lock for heavy oMLX/PGRN serve (mlock/keep-hot).
# Sourced by run_omlx_pgrn.sh; also testable in isolation.
#
# Lockfile: ${SLIPSTREAM_PGRN_LOCK:-/tmp/slipstream-omlx-pgrn.lock}
#   pid=<serve_or_launcher_pid>
#   port=<listen_port_or_unknown>
#   started_at=<ISO-8601>
#
# Override: SLIPSTREAM_PGRN_ALLOW_PARALLEL=1  (DANGEROUS — dual mlock freeze risk)
# Lifetime: SLIPSTREAM_PGRN_MAX_WALL_SEC (bench default 3600; unset/0 = no watchdog)
#
# Memory floor (Track I / CRASH_AVOIDANCE):
#   SLIPSTREAM_PGRN_MIN_FREE_GIB  default 8 — refuse serve when free+inactive below
#                                  (matches safety_watchdog_8h.sh). Set 0 to disable.
#   SLIPSTREAM_PGRN_MOCK_FREE_GIB  test-only override for mach_free_gib()
# Soft warn (does not refuse): RESIDENCY=mlock when free+inactive < 22 GiB
#   (touch@17 is liveness-only; trusted PERF needs ≥22 — see trackj_policy / ITER_300).

# Avoid double-source side effects.
if [[ -n "${_PGRN_SERVE_LOCK_LOADED:-}" ]]; then
  return 0 2>/dev/null || exit 0
fi
_PGRN_SERVE_LOCK_LOADED=1

pgrn_serve_lock_path() {
  echo "${SLIPSTREAM_PGRN_LOCK:-/tmp/slipstream-omlx-pgrn.lock}"
}

pgrn_serve_lock_read_pid() {
  local lock="$1"
  [[ -f "$lock" ]] || return 1
  awk -F= '/^pid=/ { print $2; exit }' "$lock" 2>/dev/null
}

pgrn_serve_lock_holder_alive() {
  local lock="$1"
  local pid
  pid="$(pgrn_serve_lock_read_pid "$lock" || true)"
  [[ -n "${pid:-}" ]] || return 1
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" 2>/dev/null
}

# Write/replace lock metadata. Args: pid port
pgrn_serve_lock_write() {
  local pid="$1"
  local port="${2:-unknown}"
  local lock
  lock="$(pgrn_serve_lock_path)"
  {
    echo "pid=$pid"
    echo "port=$port"
    echo "started_at=$(date -Iseconds 2>/dev/null || date)"
  } >"$lock"
}

# Release only if we own the lock (pid match). Arg: owner_pid
pgrn_serve_lock_release() {
  local owner_pid="${1:-}"
  local lock
  lock="$(pgrn_serve_lock_path)"
  [[ -f "$lock" ]] || return 0
  if [[ -n "$owner_pid" ]]; then
    local cur
    cur="$(pgrn_serve_lock_read_pid "$lock" || true)"
    if [[ -n "$cur" && "$cur" != "$owner_pid" ]]; then
      return 0
    fi
  fi
  rm -f "$lock"
}

# Echo free+inactive GiB (Mach vm_stat), or empty on failure.
# Test hook: SLIPSTREAM_PGRN_MOCK_FREE_GIB=<float>
pgrn_serve_mach_free_gib() {
  if [[ -n "${SLIPSTREAM_PGRN_MOCK_FREE_GIB:-}" ]]; then
    echo "${SLIPSTREAM_PGRN_MOCK_FREE_GIB}"
    return 0
  fi
  /usr/bin/python3 - <<'PYM' 2>/dev/null || true
import re, subprocess
try:
    out = subprocess.check_output(["vm_stat"], text=True)
except Exception:
    raise SystemExit(0)
page = 16384
m = re.search(r"page size of (\d+)", out)
if m:
    page = int(m.group(1))

def pages(label):
    m = re.search(rf"Pages {label}:\s+(\d+)", out)
    return int(m.group(1)) if m else 0

print(f"{(pages('free') + pages('inactive')) * page / (1024**3):.2f}")
PYM
}

# Soft warn when mlock is requested under free+inactive < 22 (does not refuse).
# Override: SLIPSTREAM_PGRN_MLOCK_SOFT_GIB (default 22). touch@17 remains liveness-only.
pgrn_serve_mlock_soft_warn() {
  local free_gib soft="${SLIPSTREAM_PGRN_MLOCK_SOFT_GIB:-22}"
  if ! [[ "$soft" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    soft=22
  fi
  free_gib="$(pgrn_serve_mach_free_gib || true)"
  [[ -n "${free_gib:-}" ]] || return 0
  local res
  res="$(echo "${SLIPSTREAM_PGRN_RESIDENCY:-touch}" | tr '[:upper:]' '[:lower:]')"
  case "$res" in
    mlock|1|true|yes|on|lock) ;;
    *) return 0 ;;
  esac
  local under
  under="$(/usr/bin/python3 -c "print(1 if float('${free_gib}') < float('${soft}') else 0)" 2>/dev/null || echo 0)"
  if [[ "$under" == "1" ]]; then
    echo "WARN: SLIPSTREAM_PGRN_RESIDENCY=mlock with free+inactive=${free_gib} GiB < ${soft} GiB" >&2
    echo "  Prefer touch@17 for liveness; trusted PERF/mlock needs free≥22 (trackj_policy)." >&2
    echo "  Dual mlock/keep-hot under pressure hard-froze this Mac (CRASH_INVESTIGATION)." >&2
  fi
}

# Hard refuse when free+inactive below floor. Exit 5.
# Floor: SLIPSTREAM_PGRN_MIN_FREE_GIB (default 8; 0 = disable).
pgrn_serve_memory_floor_check() {
  local floor="${SLIPSTREAM_PGRN_MIN_FREE_GIB:-8}"
  if ! [[ "$floor" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    floor=8
  fi
  # 0 / 0.0 → disabled (tests / explicit override).
  local disabled
  disabled="$(/usr/bin/python3 -c "print(1 if float('${floor}') <= 0 else 0)" 2>/dev/null || echo 0)"
  if [[ "$disabled" == "1" ]]; then
    return 0
  fi
  local free_gib
  free_gib="$(pgrn_serve_mach_free_gib || true)"
  if [[ -z "${free_gib:-}" ]]; then
    echo "WARN: could not read free+inactive (vm_stat); skipping memory floor check" >&2
    return 0
  fi
  local under
  under="$(/usr/bin/python3 -c "print(1 if float('${free_gib}') < float('${floor}') else 0)" 2>/dev/null || echo 0)"
  if [[ "$under" == "1" ]]; then
    local lock
    lock="$(pgrn_serve_lock_path)"
    echo "REFUSE: free+inactive=${free_gib} GiB < floor ${floor} GiB — not starting heavy PGRN serve" >&2
    echo "  Watchdog / CRASH_AVOIDANCE floor is 8 GiB; closing apps or tearing down other servers may help." >&2
    if [[ -f "$lock" ]] && pgrn_serve_lock_holder_alive "$lock"; then
      local old_pid old_port
      old_pid="$(pgrn_serve_lock_read_pid "$lock" || echo '?')"
      old_port="$(awk -F= '/^port=/ { print $2; exit }' "$lock" 2>/dev/null || echo '?')"
      echo "  Also: live lock holder pid=$old_pid port=$old_port ($lock)" >&2
    fi
    echo "  Override floor (not recommended): SLIPSTREAM_PGRN_MIN_FREE_GIB=0" >&2
    echo "  See docs/pgrn-mlx/CRASH_AVOIDANCE.md / artifacts/RESIDENCY_POLICY_TRACK_I.md" >&2
    return 5
  fi
  return 0
}

# Acquire exclusive serve lock. Args: port
# Exit 4 = live holder; exit 5 = memory floor. Prints clear message to stderr.
pgrn_serve_lock_acquire() {
  local port="${1:-unknown}"
  local lock
  lock="$(pgrn_serve_lock_path)"

  # Floor applies even with ALLOW_PARALLEL — dual mlock under low RAM is worse.
  pgrn_serve_memory_floor_check || return $?

  if [[ "${SLIPSTREAM_PGRN_ALLOW_PARALLEL:-0}" == "1" ]]; then
    echo "WARN: SLIPSTREAM_PGRN_ALLOW_PARALLEL=1 — skipping singleton lock (dual mlock/keep-hot freeze risk)" >&2
    pgrn_serve_mlock_soft_warn
    return 0
  fi

  if [[ -f "$lock" ]] && pgrn_serve_lock_holder_alive "$lock"; then
    local old_pid old_port old_started free_gib
    old_pid="$(pgrn_serve_lock_read_pid "$lock" || echo '?')"
    old_port="$(awk -F= '/^port=/ { print $2; exit }' "$lock" 2>/dev/null || echo '?')"
    old_started="$(awk -F= '/^started_at=/ { print $2; exit }' "$lock" 2>/dev/null || echo '?')"
    free_gib="$(pgrn_serve_mach_free_gib || true)"
    echo "REFUSE: another live PGRN oMLX serve holds $lock" >&2
    echo "  holder pid=$old_pid port=$old_port started_at=$old_started" >&2
    if [[ -n "${free_gib:-}" ]]; then
      echo "  free+inactive=${free_gib} GiB (dual serve + mlock/keep-hot = freeze class — CRASH_INVESTIGATION)" >&2
    fi
    echo "  Wait for it to exit, or tear it down (kill that pid only — never broad pkill)." >&2
    echo "  Override (DANGEROUS): SLIPSTREAM_PGRN_ALLOW_PARALLEL=1" >&2
    echo "  See docs/pgrn-mlx/CRASH_AVOIDANCE.md" >&2
    return 4
  fi

  # Stale lock (dead pid) or missing — take over.
  pgrn_serve_lock_write "$$" "$port"
  pgrn_serve_mlock_soft_warn
  return 0
}

# Parse --port N from argv (best-effort). Echoes port or "unknown".
pgrn_serve_lock_parse_port() {
  local prev=""
  local a
  for a in "$@"; do
    if [[ "$prev" == "--port" ]]; then
      echo "$a"
      return 0
    fi
    if [[ "$a" == --port=* ]]; then
      echo "${a#--port=}"
      return 0
    fi
    prev="$a"
  done
  echo "unknown"
}

# True (0) if argv contains a bare "serve" subcommand.
pgrn_serve_lock_is_serve_cmd() {
  local a
  for a in "$@"; do
    if [[ "$a" == "serve" ]]; then
      return 0
    fi
  done
  return 1
}
