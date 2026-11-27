#!/usr/bin/env bash
# Cancellable layer-major prefill: start llama-server in the e4/app regime with PGRN_LAYER_MAJOR=1, send the
# 39.5k-token prompt via /completion, kill the client after KILL_S seconds and measure how fast the server
# aborts the running prefill; then check that a short request is still served exactly like before.
# usage: scripts/heavy.sh bench/fetchwall/lm_abort.sh [LABEL]   (env CACHE=GiB, KILL_S=s, LM=0 for the control arm)
set -uo pipefail
LABEL=${1:-lmabort}; CACHE=${CACHE:-4}; KILL_S=${KILL_S:-20}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18085; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-lmabort-$LABEL.log"
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
kill -0 $PID 2>/dev/null || { echo "lm_abort: server not healthy, see $LOG" >&2; exit 1; }
python3 - "$HERE/prompt_30k.txt" "$PORT" "$LABEL" "$STAMP" "$LOG" "$KILL_S" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, time, subprocess, urllib.request, hashlib
pf, port, label, stamp, log, kill_s = sys.argv[1:7]
kill_s = float(kill_s)
url = f"http://127.0.0.1:{port}/completion"
def short():
    body = {"prompt": "<|im_start|>user\nName the capital of France in one word.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n",
            "n_predict": 12, "temperature": 0, "seed": 1, "cache_prompt": True}
    t = time.time()
    r = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    j = json.load(urllib.request.urlopen(r, timeout=600))
    return time.time() - t, j["content"], hashlib.sha256(j["content"].encode()).hexdigest()[:12]
ref_s, ref_txt, ref_h = short()
long_body = json.dumps({"prompt": open(pf).read(), "n_predict": 16, "temperature": 0, "seed": 1, "cache_prompt": True})
open(log + ".req.json", "w").write(long_body)
c = subprocess.Popen(["curl", "-s", "-o", "/dev/null", "-H", "Content-Type: application/json",
                      "--data-binary", "@" + log + ".req.json", url])
time.sleep(kill_s)
t_kill = time.time(); c.kill(); c.wait()
abort_s = None
while time.time() - t_kill < 120:
    txt = open(log, errors="replace").read()
    if "layer-major prefill aborted" in txt and "after the cancel" in txt:
        abort_s = time.time() - t_kill
        break
    time.sleep(0.1)
txt = open(log, errors="replace").read()
srv_line = next((l for l in txt.splitlines() if "after the cancel of its task" in l), "")
eng_line = next((l for l in txt.splitlines() if "layer-major prefill aborted at" in l or "aborted in the final pass" in l), "")
post_s, post_txt, post_h = short()
same = post_h == ref_h
print(f"{stamp}\tlmabort\tlabel={label}\tkill_after_s={kill_s:.0f}\tabort_seen_s={abort_s if abort_s is None else round(abort_s, 2)}\t"
      f"short_ref_s={ref_s:.1f}\tshort_after_s={post_s:.1f}\tshort_same={same}\tshort_out={post_h}\t"
      f"srv=\"{srv_line.split('srv')[-1].strip()}\"\teng=\"{eng_line.split(':', 3)[-1].strip()}\"")
print("short reply:", repr(post_txt), file=sys.stderr)
PY
RSS=$(awk '{printf "%.0f", $1/1024}' "$LOG.peak" 2>/dev/null)
kill $PID; wait $PID 2>/dev/null
rm -f "$LOG.req.json"
echo "lmabort $LABEL peak_rss_mb=$RSS swap_delta_mb=$(echo "$(swap_mb) - $S0" | bc)" | tee -a "$OUT/summary.tsv"
