#!/usr/bin/env bash
# E2: does a long agent context survive an interleaved short request? A(39.5k) -> B(short) -> A+suffix.
# Arms differ only in --cache-ram (MiB). Logs prompt_n / ttft for each request, swap delta, RSS.
# usage: e2_context.sh LABEL CACHE_RAM_MIB [CACHE_GB]
set -uo pipefail
LABEL=$1; CRAM=$2; CACHE=${3:-10}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18082; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-e2-$LABEL.log"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
"$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 \
  -c 40960 --parallel 1 -b "${B:-${UB:-2048}}" -ub "${UB:-2048}" -fa on --jinja --cache-ram "$CRAM" --host 127.0.0.1 --port $PORT > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null' EXIT INT TERM
( peak=0; while kill -0 $PID 2>/dev/null; do r=$(ps -o rss= -p $PID 2>/dev/null | tr -d " "); [ -n "$r" ] && [ "$r" -gt "$peak" ] && peak=$r && echo $peak > "$LOG.peak"; sleep 1; done ) &
for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
kill -0 $PID 2>/dev/null || { echo "e2_context: server exited before it was healthy (port $PORT busy?), see $LOG" >&2; exit 1; }
python3 - "$HERE/prompt_30k.txt" "$PORT" "$LABEL" "$CRAM" "$STAMP" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, urllib.request
pf, port, label, cram, stamp = sys.argv[1:6]
A = open(pf).read()
B = "<|im_start|>user\nWrite a five-word title for a chat about caching.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
import os
if os.environ.get("E1_APPEND"):
    # realistic agent turn: previous prompt + its answer + a new user message (append-only)
    A2 = None
else:
    A2 = A.replace("<|im_end|>\n<|im_start|>assistant", "\nAlso list the three files you would change first.<|im_end|>\n<|im_start|>assistant", 1)
def req(p, n):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/completion", data=json.dumps(
        {"prompt": p, "n_predict": n, "temperature": 0, "seed": 1, "cache_prompt": True}).encode(),
        headers={"Content-Type": "application/json"})
    t = json.load(urllib.request.urlopen(r, timeout=3600))["timings"]
    return t["prompt_n"], t["prompt_ms"] / 1000, t.get("cache_n", 0)
def req_full(p, n):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/completion", data=json.dumps(
        {"prompt": p, "n_predict": n, "temperature": 0, "seed": 1, "cache_prompt": True}).encode(),
        headers={"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(r, timeout=3600))
ra = req_full(A, 48); t = ra["timings"]; a = (t["prompt_n"], t["prompt_ms"] / 1000, t.get("cache_n", 0)); a_lm = t.get("pgrn_lm_decodes", 0)
b = req(B, 8)
if A2 is None:
    A2 = A + ra["content"] + "<|im_end|>\n<|im_start|>user\nGood. Now show the exact diff for the first file.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
if os.environ.get("E1_FRESH"):
    # control: no history in the cache - fresh prefill of the exact same follow-up prompt
    pass
rc = req_full(A2, 64); t = rc["timings"]
c = (t["prompt_n"], t["prompt_ms"] / 1000, t.get("cache_n", 0)); c_lm = t.get("pgrn_lm_decodes", 0)
import hashlib as _h
a_out = _h.sha256(ra["content"].encode()).hexdigest()[:12]
import hashlib
h = hashlib.sha256(rc["content"].encode()).hexdigest()[:12]
print(f"{stamp}\te2\tlabel={label}\tcache_ram={cram}\tA_prompt_n={a[0]}\tA_ttft_s={a[1]:.1f}\tB_prompt_n={b[0]}\t"
      f"A2_prompt_n={c[0]}\tA2_cache_n={c[2]}\tA2_ttft_s={c[1]:.1f}\tA_out={a_out}\tA2_out={h}\tA_lm={a_lm}\tA2_lm={c_lm}")
PY
RSS=$(awk '{printf "%.0f", $1/1024}' "$LOG.peak" 2>/dev/null)
kill $PID; wait $PID 2>/dev/null
echo "e2 $LABEL peak_rss_mb=$RSS swap_delta_mb=$(echo "$(swap_mb) - $S0" | bc)" | tee -a "$OUT/summary.tsv"
