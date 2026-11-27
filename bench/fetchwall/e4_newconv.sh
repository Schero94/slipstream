#!/usr/bin/env bash
# E4: does a NEW agent conversation reuse the shared system prompt (tools/files) of an earlier one?
# Chat API (/v1/chat/completions) like real agents: S1 = system + task1, then a short unrelated
# request, then S2 = same system + task2 (new conversation). Logs prompt_n / cache_n / ttft.
# App regime flags. usage: e4_newconv.sh LABEL   (env LM=1 -> layer-major arm, CACHE=GiB)
set -uo pipefail
LABEL=$1; CACHE=${CACHE:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18084; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-e4-$LABEL.log"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
if [ "${LM:-0}" = 1 ]; then UB=2048; BB=40960; export PGRN_LAYER_MAJOR=1; else UB=4096; BB=4096; fi
"$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 \
  -c 40960 --parallel 1 -b "$BB" -ub "$UB" -fa on --jinja --cache-ram "${CRAM:-2048}" ${EXTRA_ARGS:-} \
  --host 127.0.0.1 --port $PORT > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null' EXIT INT TERM
( peak=0; while kill -0 $PID 2>/dev/null; do r=$(ps -o rss= -p $PID 2>/dev/null | tr -d " "); [ -n "$r" ] && [ "$r" -gt "$peak" ] && peak=$r && echo $peak > "$LOG.peak"; sleep 1; done ) &
for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
kill -0 $PID 2>/dev/null || { echo "e4: server not healthy, see $LOG" >&2; exit 1; }
python3 - "$HERE/prompt_30k.txt" "$PORT" "$LABEL" "$STAMP" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, urllib.request, hashlib
pf, port, label, stamp = sys.argv[1:5]
raw = open(pf).read()
body = raw.split("<|im_start|>user\n", 1)[1].rsplit("\n\nTask:", 1)[0]
system = body  # instructions + source files, identical for every conversation
def chat(msgs, n):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions", data=json.dumps(
        {"messages": msgs, "max_tokens": n, "temperature": 0, "seed": 1, "cache_prompt": True,
         "chat_template_kwargs": {"enable_thinking": False}}).encode(),
        headers={"Content-Type": "application/json"})
    j = json.load(urllib.request.urlopen(r, timeout=3600))
    t = j["timings"]; c = j["choices"][0]["message"]["content"] or ""
    return t["prompt_n"], t.get("cache_n", 0), t["prompt_ms"] / 1000, hashlib.sha256(c.encode()).hexdigest()[:12]
s1 = chat([{"role": "system", "content": system},
           {"role": "user", "content": "Find where the PGRN cache hit-rate is computed for the UI and explain in two sentences how it flows from the engine to the app."}], 48)
b = chat([{"role": "user", "content": "Write a five-word title for a chat about caching."}], 8)
s2 = chat([{"role": "system", "content": system},
           {"role": "user", "content": "List the three Tauri commands that start or stop the llama-server process, with one line each."}], 48)
print(f"{stamp}\te4\tlabel={label}\tS1_prompt_n={s1[0]}\tS1_ttft_s={s1[2]:.1f}\tB_prompt_n={b[0]}\t"
      f"S2_prompt_n={s2[0]}\tS2_cache_n={s2[1]}\tS2_ttft_s={s2[2]:.1f}\tS1_out={s1[3]}\tS2_out={s2[3]}")
PY
RSS=$(awk '{printf "%.0f", $1/1024}' "$LOG.peak" 2>/dev/null)
grep -c "created context checkpoint" "$LOG" | sed 's/^/checkpoints_created=/'
kill $PID; wait $PID 2>/dev/null
echo "e4 $LABEL peak_rss_mb=$RSS swap_delta_mb=$(echo "$(swap_mb) - $S0" | bc)" | tee -a "$OUT/summary.tsv"
