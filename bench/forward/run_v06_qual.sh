#!/usr/bin/env bash
# v0.6 single-Mac qualification with the real engine (docs/plans/2026-11-28-use-your-other-mac.md, decision 2026-11-28).
# Two instances on ONE Mac, each with its own data dir:
#   big-Mac role : llama-server on 127.0.0.1:18080 (Groot-19B-A3B Q4_K_M from ~/Modelle, app flags, no MTP)
#                  + `slipstream-node share` (data dir big/, QUIC on 127.0.0.1:19103, no mDNS)
#   MacBook role : `slipstream-node use --token-file book/local.token` on 127.0.0.1:28080 (data dir book/, no mDNS)
# Paired with the 6-digit code first. The token file is written by the app's real Connect-editor patch
# (ignored test `qual_patch_writes_the_local_token` in app/src-tauri/src/main.rs) before `use` starts.
# No shaper: loopback QUIC, so this measures the forwarder's own cost, not Wi-Fi.
# usage: bench/forward/run_v06_qual.sh      (model phase runs inside scripts/heavy.sh; one model process)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/../.." && pwd)
OUT="$HERE/results"; mkdir -p "$OUT"
STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-v06qual.txt"; RES="$OUT/$STAMP-v06qual"; mkdir -p "$RES"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/slipstream-v06q.XXXXXX")
N="$ROOT/target/release/slipstream-node"
Q() { python3 "$HERE/v06_qual.py" --token-file "$WORK/book/local.token" "$@"; }
PIDS=()
cleanup() { for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -CONT "$p" 2>/dev/null || true; kill "$p" 2>/dev/null || true; done
            [ -n "${HEAVY:-}" ] && { touch "$WORK/llama.done"; wait "$HEAVY" 2>/dev/null || true; }; rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
say() { echo "$*" | tee -a "$LOG"; }
say "# v0.6 single-Mac qualification $STAMP host=$(sysctl -n hw.model) macOS $(sw_vers -productVersion) commit=$(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo +dirty)"
say "slipstream-node $(stat -f '%Sm' "$N"), newest p2p source: $(find "$ROOT/crates" "$ROOT/apps/p2p-node" -name '*.rs' -newer "$N" | head -3 | tr '\n' ' ')"

# 1. Pair big <-> book with the code (standalone window: share refuses to start without a paired Mac).
"$N" pair --show-code --listen /ip4/127.0.0.1/udp/19103/quic-v1 --name "Qual Studio" --data-dir "$WORK/big" >"$WORK/pair.out" 2>&1 &
PP=$!
for _ in $(seq 50); do grep -q '^dial=' "$WORK/pair.out" 2>/dev/null && break; sleep 0.1; done
T0=$(python3 -c 'import time;print(time.time())')
"$N" pair --peer "$(sed -n 's/^dial=//p' "$WORK/pair.out")" --code "$(sed -n 's/^code=//p' "$WORK/pair.out")" --name "Qual MacBook" --data-dir "$WORK/book" >"$WORK/pair-client.out" 2>&1
wait "$PP"
say "## pairing via 6-digit code: client process $(python3 -c "import time;print(round((time.time()-$T0)*1000))") ms; big trusts: $("$N" trust list --data-dir "$WORK/big" 2>&1 | tr '\n' ' ' | cut -c1-160)"

# 2. (g, part 1) the app's Connect-editor patch writes this install's local token (MacBook data dir).
mkdir -p "$WORK/home"
SLIPSTREAM_P2P_DIR="$WORK/book" SLIPSTREAM_QUAL_HOME="$WORK/home" SLIPSTREAM_QUAL_BASE_URL="http://127.0.0.1:28080/v1" \
  "$ROOT/scripts/heavy.sh" cargo test -q --manifest-path "$ROOT/app/src-tauri/Cargo.toml" qual_patch_writes_the_local_token -- --ignored --nocapture 2>&1 | grep -E "patched|test result|panicked" | sed "s#$WORK#<work>#g" | tee -a "$LOG"
python3 - "$WORK" <<'PY' | tee -a "$LOG"
import json,sys,os,stat
w=sys.argv[1]; tok=open(f"{w}/book/local.token").read().strip()
for p in (f"{w}/home/.config/kilo/kilo.jsonc", f"{w}/home/.config/opencode/opencode.json"):
    o=json.load(open(p))["provider"]["peregrine"]["options"]
    print(f"patch {p.split('/home/')[1]}: baseURL={o['baseURL']} apiKey==local.token: {o['apiKey']==tok} (len {len(tok)})")
print(f"local.token mode {oct(stat.S_IMODE(os.stat(f'{w}/book/local.token').st_mode))}")
PY

# 3. Real llama-server (app streamed regime for a 36 GB Mac: ctx 40960, layer-major, decay, no MTP).
GGUF="$HOME/Modelle/groot-19b-a3b-q4/Groot-19B-A3B-261023-Q4_K_M.gguf"
PGRN="$HOME/Modelle/groot-19b-a3b-q4/Groot-19B-A3B-261023-Q4_K_M.pgrn"
CACHE=${CACHE:-4}; CTX=${CTX:-40960}; PORT=18080
BIN="$ROOT/llama.cpp-slipstream/build-static/bin/llama-server"
say "## llama-server $(stat -f '%Sm' "$BIN"): $(basename "$GGUF") cache ${CACHE} GiB ctx $CTX -b $CTX -ub 2048 PGRN_EVICT=decay PGRN_LAYER_MAJOR=1 lookahead default, no MTP, io 8, compact slots, --cache-ram 2048 --metrics --slot-save-path"
mkdir -p "$WORK/slots"
S0=$(sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/')
"$ROOT/scripts/heavy.sh" bash -c '
  set -u
  PGRN_EVICT=decay PGRN_LAYER_MAJOR=1 "$0" -m "$1" --pgrn "$2" --pgrn-cache-gb "$3" --pgrn-io-threads 8 --pgrn-compact-slots \
    --gpu-layers 99 -c "$6" --parallel 1 -b "$6" -ub 2048 -fa on --jinja --alias slipstream --no-warmup \
    --cache-ram 2048 --metrics --slot-save-path "$5/slots" --reasoning off --temp 0 --host 127.0.0.1 --port "$4" >"$5/llama.log" 2>&1 &
  LP=$!; trap "kill $LP 2>/dev/null; sleep 1; kill -9 $LP 2>/dev/null" EXIT
  for i in $(seq 600); do curl -sf "http://127.0.0.1:$4/health" >/dev/null && break; kill -0 $LP || exit 1; sleep 1; done
  echo $LP > "$5/llama.pid"
  while [ ! -e "$5/llama.done" ]; do sleep 1; kill -0 $LP || exit 1; done
' "$BIN" "$GGUF" "$PGRN" "$CACHE" "$PORT" "$WORK" "$CTX" &
HEAVY=$!
for _ in $(seq 900); do [ -e "$WORK/llama.pid" ] && break; kill -0 $HEAVY 2>/dev/null || { say "llama failed: $(tail -5 "$WORK/llama.log")"; exit 1; }; sleep 1; done
LP=$(cat "$WORK/llama.pid")

# 4. share (big) + use (book, token required).
"$N" share --listen /ip4/127.0.0.1/udp/19103/quic-v1 --forward "http://127.0.0.1:$PORT" --data-dir "$WORK/big" --name "Qual Studio" \
  --no-announce --verbose >"$WORK/share.log" 2>&1 & SP=$!; PIDS+=($SP)
sleep 1
"$N" use --http 127.0.0.1:28080 --data-dir "$WORK/book" --token-file "$WORK/book/local.token" --no-discovery --verbose >"$WORK/use.log" 2>&1 & UP=$!; PIDS+=($UP)
for _ in $(seq 60); do curl -sf -H "Authorization: Bearer $(cat "$WORK/book/local.token")" http://127.0.0.1:28080/health >/dev/null && break; sleep 0.25; done
say "## forwarder up: no token -> $(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:28080/v1/models), with token -> $(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $(cat "$WORK/book/local.token")" http://127.0.0.1:28080/v1/models), POST /slots/0?action=save -> $(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $(cat "$WORK/book/local.token")" 'http://127.0.0.1:28080/slots/0?action=save' -d '{"filename":"x"}')"

D=127.0.0.1:$PORT; F=127.0.0.1:28080
say "## (a)+(b) fidelity: plain / strict JSON / forced tool (named) / tool auto / tool required; greedy seed 1, 160 tok, 2 reps x 2 arms"
Q fidelity --direct $D --fwd $F --reps "${FID_REPS:-2}" --out "$RES/fidelity" 2>&1 | tee -a "$LOG" || say "FIDELITY MISMATCH"
say "## (c) long agent prompt bench/fetchwall/prompt_30k.txt via /v1/completions, return_progress, /slots polled through the forwarder"
Q longprompt --direct $D --fwd $F --prompt "$ROOT/bench/fetchwall/prompt_30k.txt" --out "$RES/longprompt" 2>&1 | tee -a "$LOG" || say "LONGPROMPT CHECK FAILED"
say "## (d) cancel mid-prefill through the forwarder"
Q cancel --fwd $F --log "$WORK/llama.log" --reps "${CANCEL_REPS:-3}" 2>&1 | tee -a "$LOG" || say "CANCEL NOT CONFIRMED"
grep -h "event=cancelled" "$WORK/share.log" | sed 's/peer=[^ ]* //' | tail -3 | tee -a "$LOG" || true
say "## (f) first-token overhead + decode throughput, interleaved"
Q perf --direct $D --fwd $F --n-ttft "${N_TTFT:-20}" --n-decode "${N_DECODE:-6}" --out "$RES/perf.tsv" 2>&1 | tee -a "$LOG"
say "## (g, part 2) app/scripts/qualify_openai_stream.py through the forwarder with the key from the patched kilo.jsonc"
python3 -c "import json;print(json.load(open('$WORK/home/.config/kilo/kilo.jsonc'))['provider']['peregrine']['options']['apiKey'])" > "$WORK/kilo.key"
python3 "$ROOT/app/scripts/qualify_openai_stream.py" --base-url http://$F --engine llama.cpp-pgrn --model slipstream --repetitions 1 \
  --pid "$LP" --api-key-file "$WORK/kilo.key" --output "$RES/qualify.json" >/dev/null 2>&1 && say "qualify_openai_stream: passed=true" || say "QUALIFY FAILED"
python3 - "$RES/qualify.json" <<'PY' | tee -a "$LOG"
import json,sys
d=json.load(open(sys.argv[1]))
for k,v in d["summary"]["cases"].items():
    print(f"  {k}: passed={v['passed']} ttft_median={v.get('ttft_median_seconds')} decode_tps={v.get('decode_tps_mean')} max_gap={v.get('chunk_gap_max_seconds')}")
PY
say "## (e) SIGSTOP of the share process mid-stream (= big Mac asleep), then SIGCONT"
Q sigstop --direct $D --fwd $F --share-pid "$SP" --log "$WORK/llama.log" 2>&1 | tee -a "$LOG" || say "SIGSTOP CHECK FAILED"
grep -hE "peer_lost|disconnected|connected|Waking|state" "$WORK/use.log" | tail -6 | sed 's/12D3[A-Za-z0-9]*/<peer>/g' | tee -a "$LOG" || true

RSS=$(ps -o rss= -p "$LP" 2>/dev/null | awk '{printf "%.0f", $1/1024}')
grep -E "layer-major prefill aborted|prompt_abort" "$WORK/llama.log" > "$RES/llama-aborts.log" || true
cp "$WORK/share.log" "$RES/share.log"; cp "$WORK/use.log" "$RES/use.log"
sed -i '' -E 's/12D3[A-Za-z0-9]{40,}/<peer>/g' "$RES/share.log" "$RES/use.log"
touch "$WORK/llama.done"; wait $HEAVY || true; HEAVY=
S1=$(sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/')
say "llama rss_end_mb=${RSS:-NA} swap_delta_mb=$(echo "$S1 - $S0" | bc)"
say "log: $LOG"
