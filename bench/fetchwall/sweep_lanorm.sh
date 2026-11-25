#!/usr/bin/env bash
# Predictor input A/B: this layer's normed MoE input (la0) vs residual + next layer's norm.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && cmake --build build-static --target llama-completion llama-perplexity llama-server llama-pgrn-convert -j 10 2>&1 | grep -E " error|Built target llama-server" )
for c in 4 10; do
  for rep in 1 2; do
    ./run_arm.sh la0 "$c" PGRN_LOOKAHEAD=0;                          sleep 25
    ./run_arm.sh la0norm "$c" PGRN_LOOKAHEAD=0 PGRN_LOOKAHEAD_NORM=1; sleep 25
  done
done
# bundle-check the app against the fresh engine
cp ../../llama.cpp-slipstream/build-static/bin/llama-server ../../app/src-tauri/resources/llama-server
cp ../../llama.cpp-slipstream/build-static/bin/llama-pgrn-convert ../../app/src-tauri/resources/pgrn-convert
( cd ../../app/src-tauri && nice -n 10 cargo check 2>&1 | grep -E "^(error|warning: unused)|Finished|error\[" | head -20 )
echo LANORM_DONE
