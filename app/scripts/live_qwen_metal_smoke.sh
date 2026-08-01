#!/usr/bin/env bash
# Live Metal Qwen 3.6 smoke: Start → short stream → Stop (owned PID).
# Policy: internal ~/Modelle only, free+inactive ≥ 2 GiB, one serve, tear down.
set -euo pipefail

ROOT="${HOME}/Modelle/qwen3.6-35b-a3b-q4"
GGUF="${ROOT}/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"
PGRN="${ROOT}/Qwen3.6-35B-A3B-UD-Q4_K_XL.pgrn"
SERVER="${SLIPSTREAM_LLAMA_SERVER:-/Applications/Slipstream.app/Contents/Resources/resources/llama-server}"
PORT="${PORT:-8080}"
# PROFILE=path (default): cache=6 io=1 ctx=4k — Start/Stop path smoke, not peak.
# PROFILE=warm: cache=10 io=4 — safe warm when free≥17.
# PROFILE=peak: cache=14 io=4 — preferred quiet ≥PEAK_FREE_GIB (22); admit band
#   ≥ PEAK_FREE_GIB - PEAK_FREE_TOLERANCE_GIB (21.5). Hard C-admission floor still 17.
PROFILE="${PROFILE:-path}"
case "$PROFILE" in
  peak) CACHE_GB="${CACHE_GB:-14}"; IO_THREADS="${IO_THREADS:-4}"; CTX="${CTX:-4096}" ;;
  warm) CACHE_GB="${CACHE_GB:-10}"; IO_THREADS="${IO_THREADS:-4}"; CTX="${CTX:-4096}" ;;
  *)    CACHE_GB="${CACHE_GB:-6}";  IO_THREADS="${IO_THREADS:-1}"; CTX="${CTX:-4096}" ;;
esac
HEADROOM_GB="${HEADROOM_GB:-3}"
# Preferred quiet window for peak (comfortable 2× requal); tolerance avoids aborting
# near-misses like 21.81. Effective admit = 22 - 0.5 = 21.5. Safety: 14+3=17 still
# leaves ≥2 GiB post-load estimate at 21.5 (21.5-17=4.5 ≥ MIN_FREE).
PEAK_FREE_GIB="${PEAK_FREE_GIB:-22}"
PEAK_FREE_TOLERANCE_GIB="${PEAK_FREE_TOLERANCE_GIB:-0.5}"
LOG="/tmp/slipstream-qwen-metal-smoke.log"
ARTIFACT_DIR="${ARTIFACT_DIR:-/Users/schero/Desktop/Privat.nosync/LLM-BOOM/docs/pgrn-mlx/artifacts}"
ART="${ARTIFACT_DIR}/QWEN36_INTERNAL_SMOKE_$(date +%Y%m%d-%H%M%S).md"
MIN_FREE=2.0
HARD_ADMIT_GIB=17.0

mach_free() {
  /usr/bin/python3 - <<'PY'
import re, subprocess
out = subprocess.check_output(["vm_stat"], text=True)
page = 16384
m = re.search(r"page size of (\d+)", out)
if m: page = int(m.group(1))
def pages(label):
    m = re.search(rf"Pages {label}:\s+(\d+)", out)
    return int(m.group(1)) if m else 0
print(f"{(pages('free') + pages('inactive')) * page / (1024**3):.2f}")
PY
}

swap_line() { sysctl -n vm.swapusage 2>/dev/null || echo "n/a"; }

die() { echo "FAIL: $*" | tee -a "$ART"; exit 1; }

mkdir -p "$ARTIFACT_DIR"
: >"$ART"
{
  echo "# Qwen 3.6 internal Metal smoke"
  echo
  echo "- Started: $(date -Iseconds)"
  echo "- Model dir: \`$ROOT\` (must be real internal tree, not Crucial symlink)"
  echo "- Server: \`$SERVER\`"
  echo "- Profile: $PROFILE · Cache: ${CACHE_GB} GiB · headroom: ${HEADROOM_GB} GiB · io: ${IO_THREADS} · ctx: $CTX · port: $PORT"
  echo
} >>"$ART"

[[ -x "$SERVER" ]] || die "llama-server missing at $SERVER"
[[ -f "$GGUF" && -f "$PGRN" ]] || die "GGUF/PGRN missing under $ROOT"
[[ ! -L "$ROOT" ]] || die "$ROOT is still a symlink — refuse Crucial primary"
case "$(cd "$ROOT" && pwd -P)" in
  /Volumes/*) die "resolved path is under /Volumes — refuse external as primary" ;;
esac

FREE0=$(mach_free)
echo "free+inactive before: ${FREE0} GiB" | tee -a "$ART"
echo "swap before: $(swap_line)" | tee -a "$ART"
awk -v f="$FREE0" -v m="$MIN_FREE" 'BEGIN{ if (f+0 < m+0) exit 1 }' \
  || die "free+inactive ${FREE0} < ${MIN_FREE} GiB floor — refuse start"

# Ensure port free
if curl -sf -o /dev/null --max-time 1 "http://127.0.0.1:${PORT}/health" 2>/dev/null; then
  die "port ${PORT} already in use — tear down other serve first"
fi

: >"$LOG"
ARGS=(
  --model "$GGUF" --pgrn "$PGRN"
  --pgrn-cache-gb "$CACHE_GB" --pgrn-headroom-gb "$HEADROOM_GB"
  --gpu-layers 99 --ctx-size "$CTX" --parallel 1
  --batch-size 2048 --ubatch-size 2048 -fa on --jinja
  --alias slipstream --host 127.0.0.1 --port "$PORT"
  --no-warmup --metrics --reasoning off --temp 0
)
if [[ "${IO_THREADS}" -gt 1 ]]; then
  ARGS+=(--pgrn-io-threads "$IO_THREADS")
fi
# Peak/warm preflight. Hard C-admission floor ≥17; peak also uses preferred quiet
# window PEAK_FREE_GIB with PEAK_FREE_TOLERANCE_GIB (effective admit ≥21.5).
# Hard safety: refuse if free < cache+headroom+MIN_FREE (post-load ≥2 estimate).
if [[ "$PROFILE" == "peak" || "$PROFILE" == "warm" ]]; then
  awk -v f="$FREE0" -v h="$HARD_ADMIT_GIB" 'BEGIN{ if (f+0 < h+0) exit 1 }' \
    || die "PROFILE=$PROFILE needs ≥${HARD_ADMIT_GIB} GiB free+inactive for C admission (have ${FREE0}); use PROFILE=path or free RAM"
fi
if [[ "$PROFILE" == "peak" ]]; then
  PEAK_ADMIT=$(awk -v p="$PEAK_FREE_GIB" -v t="$PEAK_FREE_TOLERANCE_GIB" 'BEGIN{ printf "%.2f", p-t }')
  NEED_FLOOR=$(awk -v c="$CACHE_GB" -v h="$HEADROOM_GB" -v m="$MIN_FREE" 'BEGIN{ printf "%.2f", c+h+m }')
  echo "peak gate: preferred quiet ≥${PEAK_FREE_GIB} GiB; admit band ≥${PEAK_ADMIT} (tolerance ${PEAK_FREE_TOLERANCE_GIB}); hard floor cache+headroom+${MIN_FREE}=${NEED_FLOOR}" | tee -a "$ART"
  awk -v f="$FREE0" -v a="$PEAK_ADMIT" 'BEGIN{ if (f+0 < a+0) exit 1 }' \
    || die "PROFILE=peak free+inactive ${FREE0} < admit band ${PEAK_ADMIT} (preferred ${PEAK_FREE_GIB} − tol ${PEAK_FREE_TOLERANCE_GIB}); wait for quieter window"
  awk -v f="$FREE0" -v n="$NEED_FLOOR" 'BEGIN{ if (f+0 < n+0) exit 1 }' \
    || die "PROFILE=peak free+inactive ${FREE0} < cache+headroom+floor ${NEED_FLOOR} (would leave <${MIN_FREE} GiB)"
fi
if [[ -f "${ROOT}/partition-weights.txt" ]]; then
  ARGS+=(--pgrn-partition-weights "${ROOT}/partition-weights.txt")
fi

echo "Starting Metal serve…" | tee -a "$ART"
"$SERVER" "${ARGS[@]}" >"$LOG" 2>&1 &
PID=$!
echo "owned_pid=$PID" | tee -a "$ART"

cleanup() {
  if kill -0 "$PID" 2>/dev/null; then
    /bin/kill -TERM "$PID" 2>/dev/null || true
    for _ in $(seq 1 40); do
      kill -0 "$PID" 2>/dev/null || break
      sleep 0.25
    done
    if kill -0 "$PID" 2>/dev/null; then
      /bin/kill -KILL "$PID" 2>/dev/null || true
    fi
    wait "$PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Wait ready (loading can take a while)
READY=0
for i in $(seq 1 180); do
  FREE_NOW=$(mach_free)
  awk -v f="$FREE_NOW" -v m="$MIN_FREE" 'BEGIN{ if (f+0 < m+0) exit 1 }' || {
    echo "ABORT: free+inactive dropped to ${FREE_NOW} GiB during load" | tee -a "$ART"
    exit 2
  }
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:${PORT}/health" || true)
  if [[ "$code" == "200" ]]; then READY=1; break; fi
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "server died during load — log tail:" | tee -a "$ART"
    tail -40 "$LOG" | tee -a "$ART"
    exit 3
  fi
  sleep 2
done
[[ "$READY" == "1" ]] || die "health never reached 200"

FREE_READY=$(mach_free)
echo "free+inactive ready: ${FREE_READY} GiB" | tee -a "$ART"
echo "swap ready: $(swap_line)" | tee -a "$ART"

# Short chat (stream)
REQ='{"model":"slipstream","messages":[{"role":"user","content":"Write a Python function is_prime(n). Only code."}],"max_tokens":64,"temperature":0,"stream":false}'
T0=$(date +%s)
RESP=$(curl -sf --max-time 180 -H 'Content-Type: application/json' \
  -d "$REQ" "http://127.0.0.1:${PORT}/v1/chat/completions" || true)
T1=$(date +%s)
WALL=$((T1 - T0))
echo "chat_wall_sec=$WALL" | tee -a "$ART"
if [[ -z "$RESP" ]]; then
  echo "chat failed — log tail:" | tee -a "$ART"
  tail -60 "$LOG" | tee -a "$ART"
  exit 4
fi
/usr/bin/python3 - "$RESP" <<'PY' | tee -a "$ART"
import json,sys
raw=sys.argv[1] if len(sys.argv)>1 else ""
try:
    d=json.loads(raw)
except Exception as e:
    print("json_parse_error", e)
    print(raw[:500])
    raise SystemExit(0)
ch=(d.get("choices") or [{}])[0]
msg=(ch.get("message") or {})
text=msg.get("content") or ""
usage=d.get("usage") or {}
print(f"completion_chars={len(text)}")
print(f"preview={text[:160]!r}")
print(f"usage={usage}")
PY

# tok/s from log if present
# Log line: "eval time = N ms / M tokens ( ... X.XX tokens per second)"
TPS=$(rg -o 'eval time =.*?([0-9.]+) tokens per second' -r '$1' "$LOG" | tail -1 || true)
HIT=$(rg -o 'PGRN cache = .*?\(([0-9.]+)%\)' -r '$1' "$LOG" | tail -1 || true)
[[ -n "$HIT" ]] && echo "pgrn_hit_rate_pct=$HIT" | tee -a "$ART" || true
[[ -n "$TPS" ]] && echo "last_eval_tps=$TPS" | tee -a "$ART" || echo "last_eval_tps=n/a" | tee -a "$ART"

# RSS of owned pid
RSS=$(ps -o rss= -p "$PID" 2>/dev/null | awk '{printf "%.2f", $1/1024/1024}')
echo "rss_gib=${RSS:-n/a}" | tee -a "$ART"

FREE_AFTER_CHAT=$(mach_free)
echo "free+inactive after chat: ${FREE_AFTER_CHAT} GiB" | tee -a "$ART"

# Stop owned PID
echo "Stopping owned pid=$PID …" | tee -a "$ART"
/bin/kill -TERM "$PID" 2>/dev/null || true
for _ in $(seq 1 40); do
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.25
done
if kill -0 "$PID" 2>/dev/null; then
  /bin/kill -KILL "$PID" 2>/dev/null || true
fi
wait "$PID" 2>/dev/null || true
trap - EXIT

# Verify dead + port free + no omlx lock left (Metal shouldn't create it)
STOP_OK=1
if kill -0 "$PID" 2>/dev/null; then
  echo "STOP_FAIL: pid still alive" | tee -a "$ART"
  STOP_OK=0
fi
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 1 "http://127.0.0.1:${PORT}/health" || echo 0)
if [[ "$code" != "0" && "$code" != "000" ]]; then
  # curl may print 000 on connect fail
  if [[ "$code" =~ ^[12345] ]]; then
    echo "STOP_FAIL: port still answering health=$code" | tee -a "$ART"
    STOP_OK=0
  fi
fi
if [[ -f /tmp/slipstream-omlx-pgrn.lock ]]; then
  echo "note: omlx lock present after Metal smoke (unexpected for Metal-only)" | tee -a "$ART"
fi

FREE_END=$(mach_free)
{
  echo
  echo "## Verdict"
  echo "- stop_ok: $STOP_OK"
  echo "- free+inactive end: ${FREE_END} GiB"
  echo "- swap end: $(swap_line)"
  echo "- Finished: $(date -Iseconds)"
} | tee -a "$ART"

[[ "$STOP_OK" == "1" ]] || exit 5
echo "ARTIFACT=$ART"
echo "PASS"
