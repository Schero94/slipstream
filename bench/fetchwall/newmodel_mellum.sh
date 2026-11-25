#!/usr/bin/env bash
# New model test: Mellum2.1-12B-A2.5B (JetBrains, coding) - needs the widened PGRN type whitelist
# (down_exps Q8_0/Q5_0). Lossless parity of streamed vs resident is checked via output hash of
# a small cache (streamed) vs a cache that holds all experts.
set -uo pipefail
cd "$(dirname "$0")"
D=$HOME/Modelle/mellum2.1-12b-a2.5b-q4; mkdir -p "$D"
G=$D/Mellum2.1-12B-A2.5B-Instruct-Q4_K_M.gguf; P=${G%.gguf}.pgrn
URL=https://huggingface.co/JetBrains/Mellum2.1-12B-A2.5B-Instruct-GGUF/resolve/main/Mellum2.1-12B-A2.5B-Instruct-Q4_K_M.gguf
[ -s "$G" ] || curl -L --fail --retry 5 -C - -o "$G" "$URL"
ls -la "$G"
( cd ../../llama.cpp-slipstream && cmake --build build-static --target llama-pgrn-convert -j 10 2>&1 | grep -E "error:" | head )
[ -s "$P" ] || ../../llama.cpp-slipstream/build-static/bin/llama-pgrn-convert --input "$G" --output "$P" --io-threads 4 2>&1 | tail -4
ls -la "$P"
for c in 1 2 8; do
  COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh mellum-la "$c"; sleep 20
  COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh mellum-laoff "$c" PGRN_LOOKAHEAD=-1; sleep 20
done
echo MELLUM_DONE
