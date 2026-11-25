#!/usr/bin/env bash
# One A/B arm in the APP regime: llama-server with the app's flags (MTP speculation,
# draft 4, compact slots, io 8), one fixed greedy /completion request (warm-up first).
# usage: run_arm_server.sh LABEL CACHE_GB [ENV=VAL ...]
set -uo pipefail
LABEL=$1; CACHE=$2; shift 2
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
GGUF=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}
PGRN=${PGRN:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.pgrn"}
NTOK=${NTOK:-160}; PORT=${PORT:-18080}
OUT="$HERE/results"; mkdir -p "$OUT"
STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-srv-$LABEL-c$CACHE.log"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
env "$@" "$BIN" -m "$GGUF" --pgrn "$PGRN" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 \
  --pgrn-compact-slots --gpu-layers 99 -c 4096 --parallel 1 -b 2048 -ub 2048 \
  --spec-type draft-mtp --spec-draft-n-max 4 --host 127.0.0.1 --port "$PORT" > "$LOG" 2>&1 &
PID=$!
for i in $(seq 1 600); do curl -sf "http://127.0.0.1:$PORT/health" >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
req() { python3 -c 'import json,sys; print(json.dumps({"prompt": open(sys.argv[1]).read(), "n_predict": int(sys.argv[2]), "temperature": 0, "seed": 1, "ignore_eos": True, "cache_prompt": False}))' "$HERE/prompt_code.txt" "$1"; }
curl -s "http://127.0.0.1:$PORT/completion" -d "$(req 16)" >/dev/null   # warm-up
R=$(curl -s "http://127.0.0.1:$PORT/completion" -d "$(req "$NTOK")")
kill $PID; wait $PID 2>/dev/null
S1=$(swap_mb)
echo "$R" > "$LOG.json"
python3 - "$LOG.json" "$LABEL" "$CACHE" "$STAMP" "$(echo "$S1 - $S0" | bc)" "$*" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, hashlib
r = json.load(open(sys.argv[1])); t = r.get("timings", {})
h = hashlib.sha256(r.get("content", "").encode()).hexdigest()[:12]
acc = t.get("draft_n_accepted"); dn = t.get("draft_n")
print(f"{sys.argv[4]}\tsrv\tlabel={sys.argv[2]}\tcache={sys.argv[3]}\tdecode_tps={t.get('predicted_per_second', 0):.2f}"
      f"\tdraft_acc={acc}/{dn}\tswap_delta_mb={sys.argv[5]}\tout={h}\tenv={sys.argv[6]}")
PY
