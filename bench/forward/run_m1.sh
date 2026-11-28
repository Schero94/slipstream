#!/usr/bin/env bash
# v0.6 M1 forwarder measurements (docs/plans/2026-11-28-use-your-other-mac.md §9 M1).
# Separate processes on one Mac with their own data dirs: upstream (mock or llama-server),
# `slipstream-node share` (big Mac), a userspace UDP shaper (RTT_MS, default 8 ms) and two
# `slipstream-node use` processes (MacBook; one with the pre-opened spare stream, one
# without). Arms: direct (client -> upstream) vs fwd (client -> use -> shaped QUIC -> share -> upstream).
#
# usage: bench/forward/run_m1.sh            # mock phase
#        LLAMA=1 bench/forward/run_m1.sh    # + real llama-server (Groot-19B-A3B, cache 4 GiB, app flags incl. layer-major)
# The llama phase runs inside scripts/heavy.sh (one model process at a time).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/../.." && pwd)
OUT="$HERE/results"; mkdir -p "$OUT"
STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-m1.txt"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/slipstream-m1.XXXXXX")
cargo build --release -q -p p2p-forward -p p2p-node --manifest-path "$ROOT/Cargo.toml"
F="$ROOT/target/release/forward-spike"; N="$ROOT/target/release/slipstream-node"
RTT_MS=${RTT_MS:-8}
PIDS=()
cleanup() { for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
say() { echo "$*" | tee -a "$LOG"; }

# share on udp 19003, shaper 19004 -> 19003, use (spare) on 18191, use (no spare) on 18192
start_pair() {
  local up=$1 tag=$2
  local b1 b2; b1=$("$N" peer-id --data-dir "$WORK/book"); b2=$("$N" peer-id --data-dir "$WORK/book-nospare")
  "$N" share --listen /ip4/127.0.0.1/udp/19003/quic-v1 --forward "http://$up" --data-dir "$WORK/big" \
    --allow-peer "$b1,$b2" --name "Bench Studio" --addr-file "$WORK/addr-$tag" >"$WORK/share-$tag.log" 2>&1 & PIDS+=($!)
  for _ in $(seq 50); do [ -s "$WORK/addr-$tag" ] && break; sleep 0.1; done
  "$F" shaper --bind 127.0.0.1:19004 --target 127.0.0.1:19003 --rtt-ms "$RTT_MS" >"$WORK/shaper-$tag.log" 2>&1 & PIDS+=($!)
  sleep 0.2
  local dial; dial=$(sed 's#/udp/19003/#/udp/19004/#' "$WORK/addr-$tag")
  "$N" use --peer "$dial" --http 127.0.0.1:18191 --data-dir "$WORK/book" --verbose >"$WORK/use-$tag.log" 2>&1 & PIDS+=($!)
  "$N" use --peer "$dial" --http 127.0.0.1:18192 --data-dir "$WORK/book-nospare" --no-spare-stream --verbose >"$WORK/use-nospare-$tag.log" 2>&1 & PIDS+=($!)
  sleep 1.5
}
stop_pair() { for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done; PIDS=(); sleep 0.5; }
ping_stats() {
  python3 - "$1" <<'PY'
import re,sys
t=open(sys.argv[1]).read()
o=sorted(int(x) for x in re.findall(r"stream_open_us=(\d+)",t)); r=sorted(int(x) for x in re.findall(r"rtt_us=(\d+)",t))
p=lambda v,q: v[round((len(v)-1)*q)] if v else 0
print(f"stream_open_us n={len(o)} p50={p(o,.5)} p95={p(o,.95)} | shaped ping_rtt_us n={len(r)} p50={p(r,.5)} p95={p(r,.95)}")
PY
}

say "# M1 forwarder $STAMP  host=$(sysctl -n hw.model) $(sw_vers -productVersion)  commit=$(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo +dirty)  RTT_MS=$RTT_MS"
say "## AEAD per chunk (SealedStream framing on ring 0.17, release)"
"$F" aead --iters 100000 | tee -a "$LOG"

python3 - "$WORK" <<'PY'
import json,sys
w=sys.argv[1]
msgs=[{"role":"user","content":"Write a Python module that parses a CSV of orders, groups them by customer, and prints a monthly revenue report with type hints and docstrings."}]
json.dump({"model":"slipstream","stream":True,"messages":[{"role":"user","content":"hi"}]},open(f"{w}/mock.json","w"))
json.dump({"model":"slipstream","stream":True,"max_tokens":4,"temperature":0,"seed":1,"cache_prompt":True,"messages":msgs},open(f"{w}/ttft.json","w"))
json.dump({"model":"slipstream","stream":True,"max_tokens":int(__import__('os').environ.get('DECODE_TOKENS','256')),"temperature":0,"seed":1,"ignore_eos":True,"cache_prompt":True,"messages":msgs},open(f"{w}/decode.json","w"))
PY

say "## Mock upstream (forward-spike mock-llama), shaped RTT ${RTT_MS} ms"
"$F" mock-llama --bind 127.0.0.1:18190 >"$WORK/mock.log" 2>&1 & PIDS+=($!)
sleep 0.3
start_pair 127.0.0.1:18190 mock
ARMS=(--arm direct=127.0.0.1:18190 --arm fwd=127.0.0.1:18191 --arm fwd_nospare=127.0.0.1:18192)
say "### TTFT back-to-back: 32-token SSE, N=${N_TTFT:-300}/arm (request start incl. TCP connect -> first body byte)"
"$F" bench "${ARMS[@]}" --n "${N_TTFT:-300}" --path '/v1/chat/completions?n=32' --body-file "$WORK/mock.json" --out "$OUT/$STAMP-m1-ttft.tsv" | tee -a "$LOG"
say "### TTFT after 1 s idle (editor think time), N=${N_IDLE:-60}/arm"
"$F" bench "${ARMS[@]}" --n "${N_IDLE:-60}" --warmup 1 --cooldown-ms 1000 --path '/v1/chat/completions?n=32' --body-file "$WORK/mock.json" --out "$OUT/$STAMP-m1-idle.tsv" | tee -a "$LOG"
say "### Paced decode: 512 tokens every 25 ms, N=${N_PACED:-6}/arm"
"$F" bench "${ARMS[@]}" --n "${N_PACED:-6}" --warmup 1 --path '/v1/chat/completions?n=512&gap_us=25000' --body-file "$WORK/mock.json" --out "$OUT/$STAMP-m1-paced.tsv" | tee -a "$LOG"
ping_stats "$WORK/use-mock.log" | sed 's/^/spare:    /' | tee -a "$LOG"
ping_stats "$WORK/use-nospare-mock.log" | sed 's/^/nospare:  /' | tee -a "$LOG"
stop_pair

if [ "${LLAMA:-0}" = 1 ]; then
  GGUF=${GGUF:-"$HOME/Modelle/groot-19b-a3b-q4/Groot-19B-A3B-261023-Q4_K_M.gguf"}
  PGRN=${PGRN:-"$HOME/Modelle/groot-19b-a3b-q4/Groot-19B-A3B-261023-Q4_K_M.pgrn"}
  CACHE=${CACHE:-4}; CTX=${CTX:-16384}; PORT=18080
  BIN=${BIN:-"$ROOT/llama.cpp-slipstream/build-static/bin/llama-server"}
  say "## Real llama-server: $(basename "$GGUF") cache=${CACHE} GiB ctx=$CTX, app streamed regime: PGRN_EVICT=decay, PGRN_LAYER_MAJOR=1 (-b $CTX -ub 2048), lookahead default, no MTP, --slot-save-path; shaped RTT ${RTT_MS} ms"
  say "binary $(stat -f '%Sm' "$BIN")  newest engine src newer than binary: $(find "$ROOT/engine" -type f -newer "$BIN" | head -1 || true)"
  mkdir -p "$WORK/slots"
  S0=$(sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/')
  "$ROOT/scripts/heavy.sh" bash -c '
    set -u
    PGRN_EVICT=decay PGRN_LAYER_MAJOR=1 "$0" -m "$1" --pgrn "$2" --pgrn-cache-gb "$3" --pgrn-io-threads 8 --pgrn-compact-slots \
      --gpu-layers 99 -c "$6" --parallel 1 -b "$6" -ub 2048 -fa on --jinja --alias slipstream --no-warmup \
      --cache-ram 2048 --metrics --slot-save-path "$5/slots" --reasoning off --temp 0 --host 127.0.0.1 --port "$4" >"$5/llama.log" 2>&1 &
    LP=$!; trap "kill $LP 2>/dev/null; sleep 1; kill -9 $LP 2>/dev/null" EXIT
    for i in $(seq 600); do curl -sf "http://127.0.0.1:$4/health" >/dev/null && break; kill -0 $LP || exit 1; sleep 1; done
    kill -0 $LP || exit 1
    echo $LP > "$5/llama.pid"
    while [ ! -e "$5/llama.done" ]; do sleep 1; kill -0 $LP || exit 1; done
  ' "$BIN" "$GGUF" "$PGRN" "$CACHE" "$PORT" "$WORK" "$CTX" &
  HEAVY=$!
  for _ in $(seq 900); do [ -e "$WORK/llama.pid" ] && break; kill -0 $HEAVY 2>/dev/null || { say "llama phase failed: $(tail -5 "$WORK/llama.log" 2>/dev/null)"; exit 1; }; sleep 1; done
  LP=$(cat "$WORK/llama.pid")
  start_pair 127.0.0.1:$PORT llama
  M1PY="$HERE/m1_real.py"
  say "### Fidelity (plain / strict JSON / forced tool), greedy seed 1, ${FID_REPS:-2} reps per arm: per-event choices (all but id/created/timings) must match"
  python3 "$M1PY" fidelity --direct 127.0.0.1:$PORT --fwd 127.0.0.1:18191 --reps "${FID_REPS:-2}" --out "$WORK/fidelity" | tee -a "$LOG" || say "FIDELITY MISMATCH"
  say "### First-token overhead: cached prompt, 4 tokens, N=${N_LTTFT:-30}/arm, 1 s pause between requests"
  "$F" bench --arm direct=127.0.0.1:$PORT --arm fwd=127.0.0.1:18191 --n "${N_LTTFT:-30}" --warmup 2 --cooldown-ms 1000 \
    --path /v1/chat/completions --body-file "$WORK/ttft.json" --out "$OUT/$STAMP-m1-llama-ttft.tsv" | tee -a "$LOG"
  python3 "$M1PY" stats --tsv "$OUT/$STAMP-m1-llama-ttft.tsv" --base direct --arm fwd --field ttft_us | tee -a "$LOG"
  say "### Decode throughput: ${DECODE_TOKENS:-256} tokens ignore_eos, cached prompt, N=${N_DECODE:-12}/arm interleaved, cooldown ${COOL:-25} s"
  "$F" bench --arm direct=127.0.0.1:$PORT --arm fwd=127.0.0.1:18191 --n "${N_DECODE:-12}" --warmup 1 --cooldown-ms "$(( ${COOL:-25} * 1000 ))" \
    --path /v1/chat/completions --body-file "$WORK/decode.json" --out "$OUT/$STAMP-m1-llama-decode.tsv" | tee -a "$LOG"
  python3 "$M1PY" stats --tsv "$OUT/$STAMP-m1-llama-decode.tsv" --base direct --arm fwd --field event_rate | tee -a "$LOG"
  say "### Cancel during a long prompt (layer-major prefill) through the forwarder"
  python3 "$M1PY" cancel --fwd 127.0.0.1:18191 --log "$WORK/llama.log" --reps "${CANCEL_REPS:-3}" | tee -a "$LOG" || say "CANCEL NOT CONFIRMED"
  grep -h "event=cancelled" "$WORK/share-llama.log" | sed 's/peer=[^ ]* //' | tee -a "$LOG" || true
  if [ "${QUALIFY:-1}" = 1 ]; then
    say "### app/scripts/qualify_openai_stream.py against the forwarder"
    python3 "$ROOT/app/scripts/qualify_openai_stream.py" --base-url http://127.0.0.1:18191 --engine llama.cpp-pgrn --model slipstream \
      --repetitions 1 --pid "$LP" --output "$OUT/$STAMP-m1-qualify.json" 2>&1 | tail -15 | tee -a "$LOG" || say "QUALIFY FAILED"
  fi
  ping_stats "$WORK/use-llama.log" | tee -a "$LOG"
  PEAK=$(ps -o rss= -p "$LP" 2>/dev/null | awk '{printf "%.0f", $1/1024}')
  grep -E "layer-major prefill aborted|prompt_abort" "$WORK/llama.log" | tail -6 > "$OUT/$STAMP-m1-llama-aborts.log" || true
  touch "$WORK/llama.done"; wait $HEAVY || true
  S1=$(sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/')
  say "llama rss_end_mb=${PEAK:-NA} swap_delta_mb=$(echo "$S1 - $S0" | bc)"
  stop_pair
fi
say "log: $LOG"
