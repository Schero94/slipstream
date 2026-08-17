#!/usr/bin/env bash
set -euo pipefail
ROOT="${HOME}/Modelle/dolphin3-cyber-8b-q4"
GGUF="${ROOT}/Dolphin3.0-Llama3.1-8B-abliterated.Q4_K_M.gguf"
SERVER="${SLIPSTREAM_LLAMA_SERVER:-/Applications/Slipstream.app/Contents/Resources/resources/llama-server}"
PORT="${PORT:-8080}"
CTX="${CTX:-8192}"
LOG="/tmp/slipstream-dolphin-dense-smoke.log"
ART_DIR="${ARTIFACT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/docs}"
STAMP="$(date +%Y%m%d-%H%M%S)"
ART="${ART_DIR}/DOLPHIN3_CYBER_8B_SMOKE_${STAMP}.md"
HANDS_OFF="/tmp/slipstream-HANDS_OFF_OMLX.txt"
MIN_FREE=2.0

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
rss_gib() {
  local pid=$1
  /usr/bin/python3 - "$pid" <<'PY'
import sys
pid=int(sys.argv[1])
# macOS rss from ps is in KB
import subprocess
out=subprocess.check_output(["ps","-o","rss=","-p",str(pid)], text=True).strip()
print(f"{int(out)/(1024**2):.2f}")
PY
}
die() { echo "FAIL: $*" | tee -a "$ART"; exit 1; }

mkdir -p "$ART_DIR"
: >"$ART"
{
  echo "# Dolphin3-Cyber-8B dense Metal smoke"
  echo
  echo "- Started: $(date -Iseconds)"
  echo "- GGUF: \`$GGUF\`"
  echo "- Server: \`$SERVER\`"
  echo "- ctx=$CTX ngl=99 jinja port=$PORT (no --pgrn)"
  echo
} >>"$ART"

[[ -x "$SERVER" ]] || die "llama-server missing"
[[ -f "$GGUF" ]] || die "GGUF missing"
[[ ! -L "$ROOT" ]] || die "model dir is symlink"
case "$(cd "$ROOT" && pwd -P)" in /Volumes/*) die "external path refused" ;; esac

FREE0=$(mach_free)
echo "free+inactive before: ${FREE0} GiB" | tee -a "$ART"
echo "swap before: $(sysctl -n vm.swapusage)" | tee -a "$ART"
awk -v f="$FREE0" -v m="$MIN_FREE" 'BEGIN{ if (f+0 < m+0) exit 1 }' || die "free+inactive ${FREE0} < ${MIN_FREE}"

if curl -sf -o /dev/null --max-time 1 "http://127.0.0.1:${PORT}/health" 2>/dev/null; then
  die "port ${PORT} in use"
fi
if pgrep -lf 'llama-server|omlx-server' >/dev/null 2>&1; then
  die "other server running"
fi
if [[ -f /tmp/slipstream-omlx-pgrn.lock ]]; then
  # allow stale
  pid=$(awk -F= '/^pid=/{print $2}' /tmp/slipstream-omlx-pgrn.lock 2>/dev/null || true)
  if [[ -n "${pid:-}" ]] && kill -0 "$pid" 2>/dev/null; then
    die "live omlx lock holder $pid"
  fi
fi

: >"$LOG"
echo "HANDS_OFF Dolphin dense $(date)" >"$HANDS_OFF"
"$SERVER" \
  --model "$GGUF" \
  --gpu-layers 99 --ctx-size "$CTX" --parallel 1 \
  --batch-size 512 --ubatch-size 512 -fa on --jinja \
  --alias slipstream --host 127.0.0.1 --port "$PORT" \
  --metrics --no-warmup \
  --reasoning off --temp 0.7 --top-p 0.9 --top-k 40 --repeat-penalty 1.1 \
  -ctk q8_0 -ctv q8_0 \
  >"$LOG" 2>&1 &
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
  rm -f "$HANDS_OFF"
}
trap cleanup EXIT

echo "Waiting for /health…" | tee -a "$ART"
READY=0
for i in $(seq 1 120); do
  if ! kill -0 "$PID" 2>/dev/null; then
    tail -40 "$LOG" | tee -a "$ART"
    die "server died during load"
  fi
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:${PORT}/health" || true)
  if [[ "$code" == "200" ]]; then READY=1; break; fi
  sleep 1
done
[[ "$READY" == "1" ]] || die "health not ready in 120s"
FREE1=$(mach_free)
RSS1=$(rss_gib "$PID")
echo "ready after ${i}s · free+inactive=${FREE1} GiB · RSS=${RSS1} GiB" | tee -a "$ART"

# short chat
BODY=$(/usr/bin/python3 - <<'PY'
import json
print(json.dumps({
  "model": "slipstream",
  "messages": [
    {"role":"system","content":"You are Dolphin3-Cyber, a concise cybersecurity assistant."},
    {"role":"user","content":"In one short sentence, what is SQL injection?"}
  ],
  "max_tokens": 64,
  "temperature": 0.7,
  "stream": False
}))
PY
)
T0=$(/usr/bin/python3 -c 'import time; print(time.time())')
RESP=$(curl -sf --max-time 120 -H 'Content-Type: application/json' \
  -d "$BODY" "http://127.0.0.1:${PORT}/v1/chat/completions") || die "chat failed"
T1=$(/usr/bin/python3 -c 'import time; print(time.time())')
/usr/bin/python3 - "$RESP" "$T0" "$T1" <<'PY' | tee -a "$ART"
import json,sys
resp=json.loads(sys.argv[1])
t0=float(sys.argv[2]); t1=float(sys.argv[3])
u=resp.get("usage") or {}
text=(resp.get("choices") or [{}])[0].get("message",{}).get("content","")
pred=u.get("completion_tokens") or 0
prompt=u.get("prompt_tokens") or 0
dt=max(t1-t0, 1e-6)
tps=pred/dt
print(f"- chat_ok: yes")
print(f"- prompt_tokens: {prompt}")
print(f"- completion_tokens: {pred}")
print(f"- wall_s: {dt:.2f}")
print(f"- decode_tok_s_approx: {tps:.1f}")
print(f"- reply_preview: {text[:240]!r}")
PY

# timings from /metrics if present
curl -sf --max-time 2 "http://127.0.0.1:${PORT}/metrics" 2>/dev/null | rg -i 'prompt|predicted|tokens' | head -20 | tee -a "$ART" || true

FREE2=$(mach_free)
RSS2=$(rss_gib "$PID")
echo "- free+inactive after chat: ${FREE2} GiB" | tee -a "$ART"
echo "- RSS after chat: ${RSS2} GiB" | tee -a "$ART"
echo "- swap after: $(sysctl -n vm.swapusage)" | tee -a "$ART"

# Stop owned PID
cleanup
trap - EXIT
FREE3=$(mach_free)
echo "- free+inactive after stop: ${FREE3} GiB" | tee -a "$ART"
echo "SMOKE_OK art=$ART"
