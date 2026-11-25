#!/usr/bin/env bash
# E3: cold insertion of prompt reads. Long prompt then decode; lossless -> identical hash.
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=160
for c in 4 10; do
  for rep in 1 2; do
    ./run_arm.sh e3-warm "$c";                        sleep 25
    ./run_arm.sh e3-cold "$c" PGRN_COLD_PREFILL=1;    sleep 25
  done
done
echo E3_DONE
