#!/usr/bin/env bash
# Does the lookahead + Turbo win hold at the cache a 36 GB Mac actually uses?
cd "$(dirname "$0")"
C=${CACHE:-10}
for rep in 1 2; do
  ./run_arm.sh base "$C";                                      sleep 25
  ./run_arm.sh la0  "$C" PGRN_LOOKAHEAD=0;                     sleep 25
  ./run_arm.sh st0.01 "$C" PGRN_LOOKAHEAD=0 PGRN_STEER=0.01;   sleep 25
done
echo BAND_DONE
