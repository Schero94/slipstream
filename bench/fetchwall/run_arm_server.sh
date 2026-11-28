#!/usr/bin/env bash
# One A/B arm in the APP regime: llama-server with the app's streamed-run flags (compact
# slots, io 8, lookahead on by engine default, NO speculative decoding: the app drops MTP
# for streamed + lookahead runs, app/src-tauri/src/main.rs start_server), one fixed greedy
# /completion request (warm-up first).
# SPEC_ARGS overrides the speculative flags, e.g. SPEC_ARGS="$MTP_ARGS" with
# MTP_ARGS="--spec-type draft-mtp --spec-draft-n-max 4" for the MTP regime (the default
# until 2026-11-28; the app only uses it when the cache holds ~all experts).
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
env "$@" "$BIN" -m "$GGUF" --pgrn "$PGRN" --pgrn-cache-gb "$CACHE" --pgrn-io-threads "${IO:-8}" \
  --pgrn-compact-slots --gpu-layers 99 -c "${CTX:-4096}" --parallel 1 -b "${B:-${UB:-2048}}" -ub "${UB:-2048}" \
  ${SPEC_ARGS:-} ${EXTRA_ARGS:-} --host 127.0.0.1 --port "$PORT" > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null' EXIT INT TERM
( peak=0; while kill -0 $PID 2>/dev/null; do r=$(ps -o rss= -p $PID 2>/dev/null | tr -d " "); [ -n "$r" ] && [ "$r" -gt "$peak" ] && peak=$r && echo $peak > "$LOG.peak"; sleep 1; done ) &
for i in $(seq 1 600); do curl -sf "http://127.0.0.1:$PORT/health" >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
# never measure a server that is not ours (e.g. a stale one still bound to $PORT)
kill -0 $PID 2>/dev/null || { echo "run_arm_server: server exited before it was healthy (port $PORT busy?), see $LOG" >&2; exit 1; }
req() { python3 -c 'import json,sys,os
r={"prompt": open(sys.argv[1]).read(), "n_predict": int(sys.argv[2]), "temperature": 0, "seed": 1, "cache_prompt": False}
sf=os.environ.get("SCHEMA_FILE")
if sf: r["json_schema"]=json.load(open(sf))
else: r["ignore_eos"]=True
print(json.dumps(r))' "${PROMPT_FILE:-$HERE/prompt_code.txt}" "$1"; }
[ "${WARMUP:-1}" = 0 ] || curl -s "http://127.0.0.1:$PORT/completion" -d "$(req 16)" >/dev/null   # warm-up (WARMUP=0 skips it, e.g. for a 40k prompt)
R=$(curl -s "http://127.0.0.1:$PORT/completion" -d "$(req "$NTOK")")
kill $PID; wait $PID 2>/dev/null
S1=$(swap_mb)
echo "$R" > "$LOG.json"
RSS=$(awk '{printf "%.0f", $1/1024}' "$LOG.peak" 2>/dev/null)
python3 - "$LOG.json" "$LABEL" "$CACHE" "$STAMP" "$(echo "$S1 - $S0" | bc)" "$*" "${RSS:-NA}" "${SPEC_ARGS:-none}" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, hashlib
r = json.load(open(sys.argv[1])); t = r.get("timings", {})
h = hashlib.sha256(r.get("content", "").encode()).hexdigest()[:12]
acc = t.get("draft_n_accepted"); dn = t.get("draft_n")
print(f"{sys.argv[4]}\tsrv\tlabel={sys.argv[2]}\tcache={sys.argv[3]}\tdecode_tps={t.get('predicted_per_second', 0):.2f}\tprompt_n={t.get('prompt_n')}\tttft_s={t.get('prompt_ms', 0)/1000:.1f}\tprefill_tps={t.get('prompt_per_second', 0):.1f}"
      f"\tdraft_acc={acc}/{dn}\tswap_delta_mb={sys.argv[5]}\tpeak_rss_mb={sys.argv[7]}\tlm_decodes={t.get('pgrn_lm_decodes', 0)}\tout={h}\tspec={sys.argv[8].strip() or 'none'}\tenv={sys.argv[6]}")
PY
