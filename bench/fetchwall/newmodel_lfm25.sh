#!/usr/bin/env bash
# New model test 1: LFM2.5-24B-A2B (lfm2moe). Download to the internal SSD, convert to PGRN,
# then baseline decode at 2/4/8 GiB (lookahead/compact are qwen35moe-only today -> compact off).
set -uo pipefail
cd "$(dirname "$0")"
D=$HOME/Modelle/lfm2.5-24b-a2b-q4; mkdir -p "$D"
G=$D/LFM2.5-24B-A2B-UD-Q4_K_XL.gguf; P=${G%.gguf}.pgrn
URL=https://huggingface.co/unsloth/LFM2.5-24B-A2B-GGUF/resolve/main/LFM2.5-24B-A2B-UD-Q4_K_XL.gguf
df -h / | tail -1
[ -s "$G" ] || curl -L --fail --retry 5 -C - -o "$G" "$URL"
ls -la "$G"
CONV=../../llama.cpp-slipstream/build-static/bin/llama-pgrn-convert
[ -s "$P" ] || "$CONV" --input "$G" --output "$P" --io-threads 4 2>&1 | tail -5
ls -la "$P"; df -h / | tail -1
for c in 2 4 8; do
  COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh lfm25-base "$c"; sleep 25
done
echo LFM25_DONE
