#!/usr/bin/env bash
# Layer-major prefill in the app regime: server, 39.5k prompt, logical batch >= prompt.
# ub 2048 vs 4096 with layer-major on, vs layer-major off; 2 interleaved reps @4 GiB.
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=16 SPEC_ARGS=" " B=40960
for rep in 1 2; do
  UB=2048 ./run_arm_server.sh lm2-off-ub2048 4;                    sleep 25
  UB=2048 ./run_arm_server.sh lm2-on-ub2048 4 PGRN_LAYER_MAJOR=1;  sleep 25
  UB=4096 ./run_arm_server.sh lm2-on-ub4096 4 PGRN_LAYER_MAJOR=1;  sleep 25
done
echo LMAPP2_DONE
