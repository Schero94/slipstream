#!/usr/bin/env bash
# Turbo v2 candidate: steer + PSR mark (lookahead-predicted experts get the bonus).
cd "$(dirname "$0")"
UB=1 CHUNKS=4 ./run_kld.sh st0.03la 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.03 PGRN_STEER_LA=1
for c in 4 10; do
  for rep in 1 2; do
    ./run_arm.sh la0 "$c" PGRN_LOOKAHEAD=0;                                            sleep 25
    ./run_arm.sh st0.01 "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.01;                          sleep 25
    ./run_arm.sh st0.01la "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.01 PGRN_STEER_LA=1;        sleep 25
    ./run_arm.sh st0.02la "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02 PGRN_STEER_LA=1;        sleep 25
    ./run_arm.sh st0.03la "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.03 PGRN_STEER_LA=1;        sleep 25
  done
done
echo PSR_DONE
