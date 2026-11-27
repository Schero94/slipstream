#!/usr/bin/env bash
# Prefill scaling: prompt time at ~10k / 20k / 40k tokens on one server (cache_prompt off),
# fit t = a + b*n + c*n^2 -> quadratic (attention) vs linear (experts, DeltaNet, projections) share.
# App regime flags; LM=1 -> layer-major. usage: prefill_scaling.sh LABEL
set -uo pipefail
LABEL=$1; CACHE=${CACHE:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18085; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-scale-$LABEL.log"
if [ "${LM:-0}" = 1 ]; then UB=2048; BB=40960; export PGRN_LAYER_MAJOR=1; else UB=4096; BB=4096; fi
"$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 \
  -c 40960 --parallel 1 -b "$BB" -ub "$UB" -fa on --jinja --cache-ram 0 ${EXTRA_ARGS:-} --host 127.0.0.1 --port $PORT > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null' EXIT INT TERM
for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
kill -0 $PID 2>/dev/null || { echo "scale: server not healthy, see $LOG" >&2; exit 1; }
python3 - "$HERE/prompt_30k.txt" "$PORT" "$LABEL" "$STAMP" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, urllib.request
pf, port, label, stamp = sys.argv[1:5]
raw = open(pf).read()
head = raw.rsplit("\n\nTask:", 1)[0]
tail = "\n\nTask: summarize the code above in one sentence.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
def tok(s):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/tokenize", data=json.dumps({"content": s}).encode(),
                               headers={"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(r))["tokens"]
def detok(t):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/detokenize", data=json.dumps({"tokens": t}).encode(),
                               headers={"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(r))["content"]
toks = tok(head)
rows = []
for n in (10000, 20000, 39000):
    p = detok(toks[:n]) + tail
    r = urllib.request.Request(f"http://127.0.0.1:{port}/completion", data=json.dumps(
        {"prompt": p, "n_predict": 1, "temperature": 0, "cache_prompt": False}).encode(),
        headers={"Content-Type": "application/json"})
    t = json.load(urllib.request.urlopen(r, timeout=3600))["timings"]
    rows.append((t["prompt_n"], t["prompt_ms"] / 1000))
    print(f"{stamp}\tscale\tlabel={label}\tprompt_n={t['prompt_n']}\tprompt_s={t['prompt_ms']/1000:.1f}\tlm={t.get('pgrn_lm_decodes',0)}", flush=True)
# exact fit through 3 points: t = a + b n + c n^2
import itertools
(n1,t1),(n2,t2),(n3,t3) = rows
c = ((t3-t1)/(n3-n1) - (t2-t1)/(n2-n1)) / (n3-n2)
b = (t2-t1)/(n2-n1) - c*(n1+n2)
a = t1 - b*n1 - c*n1*n1
nq = c*n3*n3
print(f"{stamp}\tscale_fit\tlabel={label}\ta={a:.1f}s\tlinear={b*n3:.1f}s\tquadratic={nq:.1f}s\tquad_share={nq/t3*100:.0f}%")
PY
kill $PID; wait $PID 2>/dev/null
