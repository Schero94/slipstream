#!/usr/bin/env bash
# Turbo (cache-steered routing) qualification at one cache size: quality vs exact routing
# (KL / top-1 agreement) and decode speed vs the lossless lookahead baseline.
cd "$(dirname "$0")"
C=${CACHE:-4}
./run_kld.sh base "$C"
for b in 0.01 0.02 0.05; do ./run_kld.sh "st$b" "$C" PGRN_STEER=$b; done
./run_kld.sh "st0.02la" "$C" PGRN_STEER=0.02 PGRN_STEER_LA=1 PGRN_LOOKAHEAD=0
for rep in 1 2; do
  ./run_arm.sh la0 "$C" PGRN_LOOKAHEAD=0;                                     sleep 25
  for b in 0.01 0.02 0.05; do ./run_arm.sh "st$b" "$C" PGRN_LOOKAHEAD=0 PGRN_STEER=$b; sleep 25; done
  ./run_arm.sh "st0.02la" "$C" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02 PGRN_STEER_LA=1; sleep 25
done
echo STEER_DONE
