#!/usr/bin/env bash
# Experiment B: lookahead top-6 (PGRN_LOOKAHEAD_TOPN=6) vs default top-(k+0)=8, app regime
# (llama-server, -fa on, MTP off as the app does for streamed+lookahead), code prompt,
# interleaved, 3 reps, caches 4 and 10 GiB.
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_code.txt NTOK=${NTOK:-256} SPEC_ARGS=" " EXTRA_ARGS="-fa on"
for c in ${CACHES:-4 10}; do
  for rep in 1 2 3; do
    ./run_arm_server.sh la-top8 "$c";                         sleep 25
    ./run_arm_server.sh la-top6 "$c" PGRN_LOOKAHEAD_TOPN=6;   sleep 25
  done
done
echo LA_TOPN_DONE
