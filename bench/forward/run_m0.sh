#!/usr/bin/env bash
# v0.6 M0 transport spike measurements (docs/plans/2026-11-28-use-your-other-mac.md §9 M0).
# Separate processes on one Mac, each with its own key dir: mock/llama upstream, share (raw +
# sealed) and use (raw + sealed) over loopback QUIC. Arms: direct | raw pipe | sealed pipe,
# interleaved with a rotating order.
#
# usage: bench/forward/run_m0.sh            # mock-only phases (AEAD, TTFT, paced decode, peer loss)
#        LLAMA=1 bench/forward/run_m0.sh    # + real llama-server (Groot-19B-A3B, cache 4 GiB, app flags)
# The llama phase runs inside scripts/heavy.sh (one model process at a time).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/../.." && pwd)
OUT="$HERE/results"; mkdir -p "$OUT"
STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-m0.txt"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/slipstream-m0.XXXXXX")
cargo build --release -q -p p2p-forward --manifest-path "$ROOT/Cargo.toml"
B="$ROOT/target/release/forward-spike"
PIDS=()
cleanup() { for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -CONT "$p" 2>/dev/null || true; kill "$p" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
say() { echo "$*" | tee -a "$LOG"; }

# Start share+use pairs (raw on 18091, sealed on 18092) against upstream $1.
start_pipes() {
  local up=$1 tag=$2
  "$B" share --key-dir "$WORK/big-raw" --upstream "$up" --mode raw --addr-file "$WORK/addr-raw-$tag" >"$WORK/share-raw-$tag.log" 2>&1 & PIDS+=($!); SHARE_RAW=$!
  "$B" share --key-dir "$WORK/big-sealed" --upstream "$up" --mode sealed --addr-file "$WORK/addr-sealed-$tag" >"$WORK/share-sealed-$tag.log" 2>&1 & PIDS+=($!)
  for _ in $(seq 50); do [ -s "$WORK/addr-raw-$tag" ] && [ -s "$WORK/addr-sealed-$tag" ] && break; sleep 0.1; done
  "$B" use --key-dir "$WORK/mb-raw" --peer "$(cat "$WORK/addr-raw-$tag")" --bind 127.0.0.1:18091 --mode raw --verbose >"$WORK/use-raw-$tag.log" 2>&1 & PIDS+=($!)
  "$B" use --key-dir "$WORK/mb-sealed" --peer "$(cat "$WORK/addr-sealed-$tag")" --bind 127.0.0.1:18092 --mode sealed --verbose >"$WORK/use-sealed-$tag.log" 2>&1 & PIDS+=($!)
  sleep 1
}
stop_pipes() { for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done; PIDS=(); sleep 0.5; }
open_stats() { # stream-open latency + ping RTT seen by a use process
  python3 - "$1" <<'PY'
import re,sys
t=open(sys.argv[1]).read()
o=sorted(int(x) for x in re.findall(r"stream_open_us=(\d+)",t)); r=sorted(int(x) for x in re.findall(r"rtt_us=(\d+)",t))
p=lambda v,q: v[round((len(v)-1)*q)] if v else 0
print(f"stream_open_us n={len(o)} p50={p(o,.5)} p95={p(o,.95)} | ping_rtt_us n={len(r)} p50={p(r,.5)} p95={p(r,.95)}")
PY
}

say "# M0 transport spike $STAMP  host=$(sysctl -n hw.model) $(sw_vers -productVersion)  commit=$(git -C "$ROOT" rev-parse --short HEAD)"
say "## AEAD cost per chunk (SealedStream seal + open, ChaCha20-Poly1305, release build)"
"$B" aead | tee -a "$LOG"

say "## Mock upstream"
"$B" mock --bind 127.0.0.1:18090 >"$WORK/mock.log" 2>&1 & PIDS+=($!); MOCK=$!
sleep 0.3
start_pipes 127.0.0.1:18090 mock
ARMS=(--arm direct=127.0.0.1:18090 --arm raw=127.0.0.1:18091 --arm sealed=127.0.0.1:18092)
say "### TTFT: 32-event SSE, unpaced, N=${N_TTFT:-500} per arm (request start incl. TCP connect -> first body byte)"
"$B" bench "${ARMS[@]}" --n "${N_TTFT:-500}" --path '/mock?n=32' --out "$OUT/$STAMP-m0-ttft.tsv" | tee -a "$LOG"
say "### Capacity: 512 events, unpaced (pipe ceiling, not a decode number), N=100"
"$B" bench "${ARMS[@]}" --n 100 --path '/mock?n=512' --out "$OUT/$STAMP-m0-capacity.tsv" | tee -a "$LOG"
say "### Paced decode: 512 events every 25 ms (~40 tok/s), N=${N_PACED:-6} per arm"
"$B" bench "${ARMS[@]}" --n "${N_PACED:-6}" --warmup 1 --path '/mock?n=512&gap_us=25000' --out "$OUT/$STAMP-m0-paced.tsv" | tee -a "$LOG"
say "### TTFT after idle: 32 events, 1 s pause before every request (editor think time), N=${N_IDLE:-60} per arm"
"$B" bench "${ARMS[@]}" --n "${N_IDLE:-60}" --warmup 1 --cooldown-ms 1000 --path '/mock?n=32' --out "$OUT/$STAMP-m0-idle.tsv" | tee -a "$LOG"
say "### Stream open + ping (use side, raw / sealed)"
open_stats "$WORK/use-raw-mock.log" | tee -a "$LOG"; open_stats "$WORK/use-sealed-mock.log" | tee -a "$LOG"

say "### Peer loss: SIGSTOP the raw share process (= big Mac asleep); time until the use side's ping fails and until QUIC drops the connection"
T0=$(python3 -c 'import time;print(int(time.time()*1000))')
kill -STOP "$SHARE_RAW"
for _ in $(seq 150); do grep -q "event=closed" "$WORK/use-raw-mock.log" && break; sleep 0.1; done
TP=$(grep -m1 "event=ping_failed" "$WORK/use-raw-mock.log" | sed -E 's/.*unix_ms=([0-9]+).*/\1/' || true)
TC=$(grep -m1 "event=closed" "$WORK/use-raw-mock.log" | sed -E 's/.*unix_ms=([0-9]+).*/\1/' || true)
say "peer_loss ping_failed_ms=$([ -n "$TP" ] && echo $((TP - T0)) || echo none) connection_closed_ms=$([ -n "$TC" ] && echo $((TC - T0)) || echo none) ($(grep -m1 "event=closed" "$WORK/use-raw-mock.log" || true))"
kill -CONT "$SHARE_RAW"
stop_pipes

if [ "${LLAMA:-0}" = 1 ]; then
  GGUF=${GGUF:-"$HOME/Modelle/groot-19b-a3b-q4/Groot-19B-A3B-261023-Q4_K_M.gguf"}
  PGRN=${PGRN:-"$HOME/Modelle/groot-19b-a3b-q4/Groot-19B-A3B-261023-Q4_K_M.pgrn"}
  CACHE=${CACHE:-4}; PORT=18080
  BIN=${BIN:-"$ROOT/llama.cpp-slipstream/build-static/bin/llama-server"}
  say "## Real llama-server: $(basename "$GGUF") cache=${CACHE} GiB, app streamed regime (PGRN_EVICT=decay, lookahead default, no MTP), /v1/chat/completions stream, 512 tokens greedy ignore_eos, N=${N_LLAMA:-4}/arm, cooldown ${COOL:-25} s"
  say "binary $(stat -f '%Sm' "$BIN")  newest engine src $(find "$ROOT/engine" -type f -newer "$BIN" | head -1 || true)"
  python3 - "$WORK/chat.json" <<'PY'
import json,sys
json.dump({"model":"slipstream","stream":True,"max_tokens":512,"temperature":0,"seed":1,"ignore_eos":True,"cache_prompt":False,
 "messages":[{"role":"user","content":"Write a Python module that parses a CSV of orders, groups them by customer, and prints a monthly revenue report with type hints and docstrings."}]},open(sys.argv[1],"w"))
PY
  S0=$(sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/')
  "$ROOT/scripts/heavy.sh" bash -c '
    set -u
    PGRN_EVICT=decay "$0" -m "$1" --pgrn "$2" --pgrn-cache-gb "$3" --pgrn-io-threads 8 --pgrn-compact-slots \
      --gpu-layers 99 -c 8192 --parallel 1 -b 2048 -ub 2048 -fa on --jinja --alias slipstream --no-warmup \
      --cache-ram 2048 --metrics --reasoning off --temp 0 --host 127.0.0.1 --port "$4" >"$5/llama.log" 2>&1 &
    LP=$!; trap "kill $LP 2>/dev/null; sleep 1; kill -9 $LP 2>/dev/null" EXIT
    for i in $(seq 600); do curl -sf "http://127.0.0.1:$4/health" >/dev/null && break; kill -0 $LP || exit 1; sleep 1; done
    kill -0 $LP || { echo "llama-server failed, see $5/llama.log"; exit 1; }
    echo $LP > "$5/llama.pid"
    while [ ! -e "$5/llama.done" ]; do sleep 1; kill -0 $LP || exit 1; done
  ' "$BIN" "$GGUF" "$PGRN" "$CACHE" "$PORT" "$WORK" &
  HEAVY=$!
  for _ in $(seq 900); do [ -e "$WORK/llama.pid" ] && break; kill -0 $HEAVY 2>/dev/null || { say "llama phase failed: $(tail -3 "$WORK/llama.log" 2>/dev/null)"; exit 1; }; sleep 1; done
  start_pipes 127.0.0.1:$PORT llama
  LARMS=(--arm direct=127.0.0.1:$PORT --arm raw=127.0.0.1:18091 --arm sealed=127.0.0.1:18092)
  "$B" bench "${LARMS[@]}" --n "${N_LLAMA:-4}" --warmup 1 --cooldown-ms "$(( ${COOL:-25} * 1000 ))" \
    --path /v1/chat/completions --body-file "$WORK/chat.json" --out "$OUT/$STAMP-m0-llama.tsv" | tee -a "$LOG"
  open_stats "$WORK/use-raw-llama.log" | tee -a "$LOG"; open_stats "$WORK/use-sealed-llama.log" | tee -a "$LOG"
  PEAK=$(ps -o rss= -p "$(cat "$WORK/llama.pid")" 2>/dev/null | awk '{printf "%.0f", $1/1024}')
  touch "$WORK/llama.done"; wait $HEAVY || true
  S1=$(sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/')
  say "llama rss_end_mb=${PEAK:-NA} swap_delta_mb=$(echo "$S1 - $S0" | bc)"
  stop_pipes
fi
say "log: $LOG"
