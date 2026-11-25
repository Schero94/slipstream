#!/usr/bin/env bash
# One A/B arm of the fetch-wall experiments: fixed greedy decode on a streamed MoE,
# reporting decode tok/s, PGRN hit-rate, lookahead recall, swap delta and an output hash
# (the hash is the parity check: lossless arms must match the baseline hash exactly).
#
# usage: run_arm.sh LABEL CACHE_GB [ENV=VAL ...]
# env:   BIN (llama-completion), GGUF, PGRN, NTOK (default 160), PROMPT_FILE, OUT (results dir)
set -euo pipefail
LABEL=$1; CACHE=$2; shift 2
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-completion"}
GGUF=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}
PGRN=${PGRN:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.pgrn"}
NTOK=${NTOK:-160}
PROMPT_FILE=${PROMPT_FILE:-"$HERE/prompt_code.txt"}
OUT=${OUT:-"$HERE/results"}
mkdir -p "$OUT"
STAMP=$(date +%Y%m%d-%H%M%S)
LOG="$OUT/$STAMP-$LABEL-c$CACHE.log"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
env "$@" "$BIN" -m "$GGUF" --pgrn "$PGRN" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 \
    ${COMPACT---pgrn-compact-slots} -c "${CTX:-4096}" -b "${UB:-2048}" -ub "${UB:-2048}" -no-cnv --temp 0 --seed 1 --ignore-eos \
    -n "$NTOK" -f "$PROMPT_FILE" --no-warmup > "$LOG.out" 2> "$LOG"
S1=$(swap_mb)
EVAL=$(grep -E "common_perf_print: +eval time" "$LOG" | sed -E 's/.* ([0-9.]+) tokens per second.*/\1/' || true)
PEVAL=$(grep -E "common_perf_print: prompt eval time" "$LOG" | sed -E 's/.* ([0-9.]+) tokens per second.*/\1/' || true)
HIT=$(grep -E "PGRN cache = " "$LOG" | sed -E 's/.*\(([0-9.]+)%\).*/\1/' | tail -1 || true)
REC=$(grep -E "PGRN lookahead recall" "$LOG" | sed -E 's/.*recall = ([0-9.]+)%.*/\1/' | tail -1 || true)
HASH=$(shasum -a 256 "$LOG.out" | cut -c1-12)
printf '%s\tlabel=%s\tcache=%s\tdecode_tps=%s\tprefill_tps=%s\thit=%s\tla_recall=%s\tswap_delta_mb=%s\tout=%s\tenv=%s\n' \
  "$STAMP" "$LABEL" "$CACHE" "${EVAL:-NA}" "${PEVAL:-NA}" "${HIT:-NA}" "${REC:-NA}" \
  "$(echo "$S1 - $S0" | bc)" "$HASH" "$*" | tee -a "$OUT/summary.tsv"
