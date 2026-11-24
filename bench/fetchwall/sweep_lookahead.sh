#!/usr/bin/env bash
# Interleaved lookahead sweep: for each cache, reps x (base, la0, la2, la4).
cd "$(dirname "$0")"
for c in ${CACHES:-4 8}; do
  for rep in 1 2; do
    ./run_arm.sh base "$c";                     sleep 25
    ./run_arm.sh la0  "$c" PGRN_LOOKAHEAD=0;    sleep 25
    ./run_arm.sh la2  "$c" PGRN_LOOKAHEAD=2;    sleep 25
    ./run_arm.sh la4  "$c" PGRN_LOOKAHEAD=4;    sleep 25
  done
done
echo SWEEP_DONE
