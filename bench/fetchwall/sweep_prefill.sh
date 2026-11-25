#!/usr/bin/env bash
# v0.5 step 1: prefill baseline for a ~35k-token agent prompt (app regime: llama-server,
# lookahead default-on, no MTP), plus the cheap levers: ubatch size and I/O threads.
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=16 SPEC_ARGS=" "
for c in 4 10; do
  UB=2048 ./run_arm_server.sh pf-ub2048 "$c";            sleep 25
  UB=4096 ./run_arm_server.sh pf-ub4096 "$c";            sleep 25
  UB=2048 IO=16 ./run_arm_server.sh pf-ub2048-io16 "$c"; sleep 25
done
echo PREFILL_DONE
