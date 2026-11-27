#!/usr/bin/env bash
# Layer-major prompt progress: llama-server in the e4/app regime with PGRN_LAYER_MAJOR=1 (one decode call for the
# whole 39.5k-token prompt), stream /completion with "return_progress": true and record when each prompt_progress
# event arrives; poll /slots meanwhile (phase + prompt_progress) and collect the prompt_start / prompt_progress /
# prompt_done log lines.
# usage: scripts/heavy.sh bench/fetchwall/lm_progress.sh [LABEL]   (env CACHE=GiB, LM=0 for the control arm)
set -uo pipefail
LABEL=${1:-lmprog}; CACHE=${CACHE:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18088; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-lmprog-$LABEL.log"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
if [ "${LM:-1}" = 1 ]; then UB=2048; BB=40960; export PGRN_LAYER_MAJOR=1; else UB=4096; BB=4096; fi
"$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 \
  -c 40960 --parallel 1 -b "$BB" -ub "$UB" -fa on --jinja --cache-ram "${CRAM:-2048}" ${EXTRA_ARGS:-} \
  --host 127.0.0.1 --port $PORT > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null' EXIT INT TERM
( peak=0; while kill -0 $PID 2>/dev/null; do r=$(ps -o rss= -p $PID 2>/dev/null | tr -d " "); [ -n "$r" ] && [ "$r" -gt "$peak" ] && peak=$r && echo $peak > "$LOG.peak"; sleep 1; done ) &
for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
kill -0 $PID 2>/dev/null || { echo "lm_progress: server not healthy, see $LOG" >&2; exit 1; }
python3 - "$HERE/prompt_30k.txt" "$PORT" "$LABEL" "$STAMP" "$LOG" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, time, threading, http.client, hashlib, urllib.request, statistics
pf, port, label, stamp, log = sys.argv[1:6]
slots_seen = []
stop = False
def poll_slots():
    while not stop:
        try:
            j = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/slots", timeout=5))
            s = j[0]
            slots_seen.append((time.time(), s.get("phase"), (s.get("prompt_progress") or {}).get("processed")))
        except Exception as e:
            slots_seen.append((time.time(), "err:" + str(e)[:40], None))
        time.sleep(2)
body = json.dumps({"prompt": open(pf).read(), "n_predict": 16, "temperature": 0, "seed": 1, "cache_prompt": True,
                   "stream": True, "return_progress": True})
th = threading.Thread(target=poll_slots, daemon=True); th.start()
conn = http.client.HTTPConnection("127.0.0.1", int(port), timeout=3600)
t0 = time.time()
conn.request("POST", "/completion", body, {"Content-Type": "application/json"})
resp = conn.getresponse()
events, content, first_tok, total = [], "", None, None
while True:
    line = resp.readline()
    if not line:
        break
    line = line.decode().strip()
    if not line.startswith("data: "):
        continue
    d = json.loads(line[6:])
    t = time.time() - t0
    if "prompt_progress" in d:
        pp = d["prompt_progress"]; total = pp["total"]
        events.append((t, pp["processed"], pp["cache"], pp["time_ms"]))
    if d.get("content"):
        if first_tok is None: first_tok = t
        content += d["content"]
    if d.get("stop"):
        break
stop = True
h = hashlib.sha256(content.encode()).hexdigest()[:12]
ts = [e[0] for e in events]
gaps = [b - a for a, b in zip(ts, ts[1:])]
# progress gaps while the prompt is running: from the first event to the last event before the first token
mono = all(b[1] >= a[1] for a, b in zip(events, events[1:]))
print(f"{stamp}\tlmprog\tlabel={label}\tprompt_n={total}\tevents={len(events)}\tfirst_event_s={ts[0] if ts else 0:.2f}\t"
      f"last_event_s={ts[-1] if ts else 0:.1f}\tfirst_token_s={first_tok if first_tok else 0:.1f}\t"
      f"gap_max_s={max(gaps) if gaps else 0:.2f}\tgap_median_s={statistics.median(gaps) if gaps else 0:.2f}\t"
      f"monotonic={mono}\tout={h}")
with open(log + ".events.tsv", "w") as f:
    for e in events: f.write("%.3f\t%d\t%d\t%d\n" % e)
print("slots samples (t, phase, processed):", [(round(t - t0, 1), p, n) for t, p, n in slots_seen][:12], "...", file=sys.stderr)
txt = open(log, errors="replace").read().splitlines()
for key in ("prompt_start:", "prompt_progress:", "prompt_done:"):
    ls = [l for l in txt if key in l]
    print(f"log {key} count={len(ls)} first={ls[0].strip() if ls else ''}", file=sys.stderr)
print("reply:", repr(content), file=sys.stderr)
PY
RSS=$(awk '{printf "%.0f", $1/1024}' "$LOG.peak" 2>/dev/null)
kill $PID; wait $PID 2>/dev/null
echo "lmprog $LABEL peak_rss_mb=$RSS swap_delta_mb=$(echo "$(swap_mb) - $S0" | bc)" | tee -a "$OUT/summary.tsv"
