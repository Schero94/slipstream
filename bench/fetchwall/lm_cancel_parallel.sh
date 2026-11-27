#!/usr/bin/env bash
# D1 check: a cancelled layer-major prefill must end only its own slot. llama-server with --parallel 2 and
# PGRN_LAYER_MAJOR=1, -b 8192 so a long prompt runs as several single-slot layer-major decodes. Request A (~20k
# tokens, slot 0) starts; request B (~12k tokens) arrives 3 s later and waits (A fills every batch, B is a
# processing slot that is NOT in the decode). A is cancelled (client disconnect) during a later layer-major chunk.
# Pass: B completes and its output hash equals B decoded alone (cache_prompt false) on the same server.
# usage: scripts/heavy.sh bench/fetchwall/lm_cancel_parallel.sh [LABEL]   (env CACHE=GiB)
set -uo pipefail
LABEL=${1:-lmcancelpar}; CACHE=${CACHE:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18087; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-lmcancelpar-$LABEL.log"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
export PGRN_LAYER_MAJOR=1
"$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 \
  -c 65536 --parallel 2 -b 8192 -ub 2048 -fa on --jinja --cache-ram 0 ${EXTRA_ARGS:-} \
  --host 127.0.0.1 --port $PORT > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null' EXIT INT TERM
( peak=0; while kill -0 $PID 2>/dev/null; do r=$(ps -o rss= -p $PID 2>/dev/null | tr -d " "); [ -n "$r" ] && [ "$r" -gt "$peak" ] && peak=$r && echo $peak > "$LOG.peak"; sleep 1; done ) &
for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
kill -0 $PID 2>/dev/null || { echo "lm_cancel_parallel: server not healthy, see $LOG" >&2; exit 1; }
python3 - "$HERE/prompt_30k.txt" "$PORT" "$LABEL" "$STAMP" "$LOG" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, time, threading, http.client, hashlib, urllib.request
pf, port, label, stamp, log = sys.argv[1:6]
raw = open(pf).read()
body = raw.split("<|im_start|>user\n", 1)[1].rsplit("\n\nTask:", 1)[0]
wrap = lambda u: f"<|im_start|>user\n{u}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
A = wrap(body[:68000] + "\n\nTask: summarise the files above in three sentences.")
B = wrap(body[70000:110000] + "\n\nTask: list the functions defined above, one per line.")
def req(prompt, n, cache=True, stream=False):
    return json.dumps({"prompt": prompt, "n_predict": n, "temperature": 0, "seed": 1, "cache_prompt": cache,
                       "stream": stream, "return_progress": stream})
res = {}
def run_b(key, cache):
    t = time.time()
    try:
        r = urllib.request.Request(f"http://127.0.0.1:{port}/completion", data=req(B, 32, cache).encode(),
                                   headers={"Content-Type": "application/json"})
        j = json.load(urllib.request.urlopen(r, timeout=3600))
        res[key] = (time.time() - t, j["content"], hashlib.sha256(j["content"].encode()).hexdigest()[:12], None)
    except Exception as e:
        body_err = e.read().decode()[:200] if hasattr(e, "read") else str(e)
        res[key] = (time.time() - t, "", None, body_err)
def slots():
    return json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/slots", timeout=10))
# A streams with progress on slot 0
ca = http.client.HTTPConnection("127.0.0.1", int(port), timeout=3600)
ca.request("POST", "/completion", req(A, 32, True, True), {"Content-Type": "application/json"})
ra = ca.getresponse()
time.sleep(3)
tb = threading.Thread(target=run_b, args=("B_concurrent", True)); tb.start()
# wait until B holds a slot in the prompt phase and A's prompt is in a later layer-major chunk (processed moving
# while B waits), then disconnect A
t_cancel, snap = None, None
deadline = time.time() + 600
while time.time() < deadline:
    s = slots()
    pp = lambda x: x.get("prompt_progress") or {}
    a = next((x for x in s if x.get("is_processing") and pp(x).get("total", 0) > 15000), None)   # A ~20k tokens
    b = next((x for x in s if x.get("is_processing") and 0 < pp(x).get("total", 0) <= 15000), None)
    if a and b and a.get("phase") == "prompt" and b.get("phase") == "prompt":
        pa = (a.get("prompt_progress") or {}).get("processed", 0)
        if pa > 8192 + 1000:   # inside the second 8192 chunk
            snap = (a.get("prompt_progress"), b.get("prompt_progress"))
            break
    time.sleep(1)
t_cancel = time.time()
ra.close(); ca.close()   # client gone -> server cancels A
tb.join()
time.sleep(1)
run_b("B_alone", False)
txt = open(log, errors="replace").read().splitlines()
abort_lines = [l.strip() for l in txt if "prompt_abort:" in l or "after the cancel of its task" in l or "layer-major prefill aborted at" in l]
bc, ba = res["B_concurrent"], res["B_alone"]
print(f"{stamp}\tlmcancelpar\tlabel={label}\tcancel_snapshot_A={snap[0] if snap else None}\tB_at_cancel={snap[1] if snap else None}\t"
      f"B_concurrent_s={bc[0]:.1f}\tB_concurrent_out={bc[2]}\tB_concurrent_err={bc[3]}\tB_alone_s={ba[0]:.1f}\t"
      f"B_alone_out={ba[2]}\tsame={bc[2] is not None and bc[2] == ba[2]}")
for l in abort_lines: print("  ", l, file=sys.stderr)
print("B reply:", repr(bc[1]), file=sys.stderr)
PY
RSS=$(awk '{printf "%.0f", $1/1024}' "$LOG.peak" 2>/dev/null)
kill $PID; wait $PID 2>/dev/null
echo "lmcancelpar $LABEL peak_rss_mb=$RSS swap_delta_mb=$(echo "$(swap_mb) - $S0" | bc)" | tee -a "$OUT/summary.tsv"
