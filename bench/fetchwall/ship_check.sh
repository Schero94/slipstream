#!/usr/bin/env bash
# Ship check for the two new app defaults (critic review 2026-11-26):
#  - peak RSS ub 2048 vs 4096 on the 39.5k agent prompt
#  - combined defaults ub 4096 + cache-ram 2048, append-only follow-up turn, 2 interleaved reps
#  - output identity: reused follow-up (64 tokens) vs a fresh prefill of the same prompt
cd "$(dirname "$0")"
for rep in 1 2; do
  UB=2048 E1_APPEND=1 ./e2_context.sh ship-ub2048-cram2048 2048 10; sleep 25
  UB=4096 E1_APPEND=1 ./e2_context.sh ship-ub4096-cram2048 2048 10; sleep 25
done
# fresh control (ub 4096)
BIN=../../llama.cpp-slipstream/build-static/bin/llama-server
G=$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf; P=${G%.gguf}.pgrn
$BIN -m $G --pgrn $P --pgrn-cache-gb 10 --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 -c 40960 \
  --parallel 1 -b 4096 -ub 4096 -fa on --jinja --cache-ram 0 --port 18083 > results/ship-fresh.log 2>&1 &
PID=$!
for i in $(seq 1 600); do curl -sf http://127.0.0.1:18083/health >/dev/null && break; sleep 1; done
python3 fresh_a2.py prompt_30k.txt 18083 results/ship-a2.txt
python3 - <<'PY'
import json, urllib.request, hashlib
A2 = open("results/ship-a2.txt").read()
r = urllib.request.Request("http://127.0.0.1:18083/completion", data=json.dumps(
    {"prompt": A2, "n_predict": 64, "temperature": 0, "seed": 1, "cache_prompt": False}).encode(),
    headers={"Content-Type": "application/json"})
rc = json.load(urllib.request.urlopen(r, timeout=3600))
print(f"ship fresh A2_out={hashlib.sha256(rc['content'].encode()).hexdigest()[:12]} prompt_n={rc['timings']['prompt_n']}")
PY
kill $PID; wait $PID 2>/dev/null
echo SHIP_DONE
