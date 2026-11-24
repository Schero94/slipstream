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
OUT="$HERE/results"; mkdir -p "$OUT"
BASE="$OUT/kld_base_c$CACHE.bin"
COMMON=(-m "$GGUF" --pgrn "$PGRN" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots
        -f "$HERE/kld_corpus.txt" -c 512 -b 512 -ub 512 --chunks "$CHUNKS")
LOG="$OUT/$(date +%Y%m%d-%H%M%S)-kld-$LABEL-c$CACHE.log"
if [ "$LABEL" = base ]; then
  env "$@" "$BIN" "${COMMON[@]}" --kl-divergence-base "$BASE" > "$LOG" 2>&1
  grep -E "Final estimate|PPL" "$LOG" | tail -2
else
  env "$@" "$BIN" "${COMMON[@]}" --kl-divergence-base "$BASE" --kl-divergence > "$LOG" 2>&1
  MKL=$(grep -E "^Mean +KLD" "$LOG" | awk '{print $3, $4, $5}')
  TOP=$(grep -E "^Same top p" "$LOG" | awk '{print $4, $5, $6}')
  PPLR=$(grep -E "^Mean +PPL\(Q\)/PPL\(base\)" "$LOG" | awk '{print $3, $4, $5}')
  HIT=$(grep -E "PGRN cache = " "$LOG" | sed -E 's/.*\(([0-9.]+)%\).*/\1/' | tail -1)
  printf '%s\tkld\tlabel=%s\tcache=%s\tmean_kld=%s\tsame_top=%s\tppl_ratio=%s\thit=%s\tenv=%s\n' \
    "$(date +%Y%m%d-%H%M%S)" "$LABEL" "$CACHE" "$MKL" "$TOP" "$PPLR" "${HIT:-NA}" "$*" | tee -a "$OUT/summary.tsv"
fi
