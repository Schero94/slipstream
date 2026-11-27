#!/usr/bin/env bash
# E5 restart persistence: server 1 answers S1 (system prompt + task1), saves slot 0 to disk, stops.
# Server 2 starts, restores the slot, answers S2 (same system prompt, task2). S2 output hash must
# equal the fresh-S2 control (E4_FRESH). Logs save/restore ms, file size, S2 prompt_n/ttft.
# App regime flags. usage: e5_restart.sh LABEL   (env LM=1 -> layer-major)
set -uo pipefail
LABEL=$1; CACHE=${CACHE:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18086; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S)
SLOTDIR=${SLOTDIR:-$HOME/.slipstream-bench-slots}; mkdir -p "$SLOTDIR"; rm -f "$SLOTDIR/e5.bin"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
if [ "${LM:-0}" = 1 ]; then UB=2048; BB=40960; export PGRN_LAYER_MAJOR=1; else UB=4096; BB=4096; fi
PID=
start() {
  "$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 \
    -c 40960 --parallel 1 -b "$BB" -ub "$UB" -fa on --jinja --cache-ram 2048 --slot-save-path "$SLOTDIR" \
    --host 127.0.0.1 --port $PORT > "$OUT/$STAMP-e5-$LABEL-$1.log" 2>&1 &
  PID=$!
  for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && return 0; kill -0 $PID 2>/dev/null || break; sleep 1; done
  echo "e5: server $1 not healthy" >&2; exit 1
}
stop() { kill $PID 2>/dev/null; wait $PID 2>/dev/null; PID=; }
trap '[ -n "$PID" ] && kill -9 $PID 2>/dev/null' EXIT INT TERM
cat > "$OUT/.e5.py" <<'PY'
import json, sys, urllib.request, hashlib, time
pf, port, phase = sys.argv[1:4]
raw = open(pf).read()
system = raw.split("<|im_start|>user\n", 1)[1].rsplit("\n\nTask:", 1)[0]
def post(path, body, timeout=3600):
    r = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=json.dumps(body).encode(),
                               headers={"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(r, timeout=timeout))
def chat(task, n=48):
    j = post("/v1/chat/completions", {"messages": [{"role": "system", "content": system}, {"role": "user", "content": task}],
             "max_tokens": n, "temperature": 0, "seed": 1, "cache_prompt": True, "chat_template_kwargs": {"enable_thinking": False}})
    t = j["timings"]; c = j["choices"][0]["message"]["content"] or ""
    return t["prompt_n"], t.get("cache_n", 0), t["prompt_ms"] / 1000, hashlib.sha256(c.encode()).hexdigest()[:12]
if phase == "save":
    s1 = chat("Find where the PGRN cache hit-rate is computed for the UI and explain in two sentences how it flows from the engine to the app.")
    t0 = time.time(); r = post("/slots/0?action=save", {"filename": "e5.bin"}); dt = time.time() - t0
    print(f"S1_prompt_n={s1[0]}\tS1_ttft_s={s1[2]:.1f}\tS1_out={s1[3]}\tsave_wall_s={dt:.2f}\tsave_ms={r.get('timings',{}).get('save_ms',0):.0f}\tsaved_tokens={r.get('n_saved')}", end="\t")
else:
    t0 = time.time(); r = post("/slots/0?action=restore", {"filename": "e5.bin"}); dt = time.time() - t0
    s2 = chat("List the three Tauri commands that start or stop the llama-server process, with one line each.")
    print(f"restore_wall_s={dt:.2f}\trestored_tokens={r.get('n_restored')}\tS2_prompt_n={s2[0]}\tS2_cache_n={s2[1]}\tS2_ttft_s={s2[2]:.1f}\tS2_out={s2[3]}")
PY
LINE="$STAMP	e5	label=$LABEL	"
start 1; LINE+=$(python3 "$OUT/.e5.py" "$HERE/prompt_30k.txt" $PORT save); stop
LINE+="file_mb=$(du -m "$SLOTDIR/e5.bin" | cut -f1)	"
start 2; LINE+=$(python3 "$OUT/.e5.py" "$HERE/prompt_30k.txt" $PORT restore); stop
echo "$LINE	swap_delta_mb=$(echo "$(swap_mb) - $S0" | bc)" | tee -a "$OUT/summary.tsv"
rm -f "$SLOTDIR/e5.bin"
