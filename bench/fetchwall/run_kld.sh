#!/usr/bin/env bash
# Quality gate for lossy routing variants: KL divergence + top-1 agreement of a variant's
# next-token distribution against EXACT routing, on a fixed corpus (llama-perplexity).
# usage: run_kld.sh base CACHE_GB              -> writes results/kld_base_c<CACHE>.bin
#        run_kld.sh LABEL CACHE_GB ENV=VAL...  -> compares against that base
set -euo pipefail
LABEL=$1; CACHE=$2; shift 2
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-perplexity"}
GGUF=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}
PGRN=${PGRN:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.pgrn"}
CHUNKS=${CHUNKS:-12}
UB=${UB:-512}   # UB=1 = decode regime (token by token: lookahead + PSR active)
OUT="$HERE/results"; mkdir -p "$OUT"
BASE="$OUT/kld_base_c${CACHE}_ub${UB}_n${CHUNKS}.bin"
COMMON=(-m "$GGUF" --pgrn "$PGRN" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots
        -f "$HERE/kld_corpus.txt" -c 512 -b 512 -ub "$UB" --chunks "$CHUNKS")
LOG="$OUT/$(date +%Y%m%d-%H%M%S)-kld-$LABEL-c$CACHE-ub$UB.log"
if [ "$LABEL" = base ]; then
  env "$@" "$BIN" "${COMMON[@]}" --kl-divergence-base "$BASE" > "$LOG" 2>&1
  grep -E "Final estimate|PPL" "$LOG" | tail -2
else
  env "$@" "$BIN" "${COMMON[@]}" --kl-divergence-base "$BASE" --kl-divergence > "$LOG" 2>&1
  MKL=$(grep -E "^Mean +KLD" "$LOG" | awk "{print \$3, \$4, \$5}" || true)
  TOP=$(grep -E "^Same top p" "$LOG" | awk '{print $4, $5, $6}' || true)
  PPLR=$(grep -E "^Mean +PPL\(Q\)/PPL\(base\)" "$LOG" | awk '{print $4, $5, $6}' || true)
  HIT=$(grep -E "PGRN cache = " "$LOG" | sed -E 's/.*\(([0-9.]+)%\).*/\1/' | tail -1 || true)
  printf '%s\tkld ub=%s\tlabel=%s\tcache=%s\tmean_kld=%s\tsame_top=%s\tppl_ratio=%s\thit=%s\tenv=%s\n' \
    "$(date +%Y%m%d-%H%M%S)" "$UB" "$LABEL" "$CACHE" "$MKL" "$TOP" "$PPLR" "${HIT:-NA}" "$*" | tee -a "$OUT/summary.tsv"
fi
