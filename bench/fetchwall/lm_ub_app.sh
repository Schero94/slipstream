#!/usr/bin/env bash
# Layer-major in the exact app regime: which ubatch? 2 interleaved reps each.
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=16 SPEC_ARGS=" " EXTRA_ARGS="--cache-ram 2048 -fa on --jinja" B=40960
for rep in 1 2; do
  UB=4096 ./run_arm_server.sh lmapp-ub4096 4 PGRN_LAYER_MAJOR=1; sleep 25
  UB=2048 ./run_arm_server.sh lmapp-ub2048 4 PGRN_LAYER_MAJOR=1; sleep 25
done
echo LMUB_DONE
