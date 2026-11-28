#!/usr/bin/env bash
# Experiment A: decode right after the 39.5k agent prompt with layer-major prefill (app regime:
# llama-server, -b 40960 -ub 2048, -fa on, MTP off), PGRN_LM_HOT_ORDER=0 vs 1, interleaved,
# 2 reps, caches 4 and 10 GiB. decode_tps = first NTOK (256) tokens after the prompt.
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=${NTOK:-256} SPEC_ARGS=" " B=40960 UB=2048 WARMUP=0 \
       EXTRA_ARGS="-fa on --cache-ram 0"
for c in ${CACHES:-4 10}; do
  for rep in 1 2; do
    ./run_arm_server.sh hot-off "$c" PGRN_LAYER_MAJOR=1;                      sleep 25
    ./run_arm_server.sh hot-on  "$c" PGRN_LAYER_MAJOR=1 PGRN_LM_HOT_ORDER=1;  sleep 25
  done
done
echo HOT_ORDER_DONE
