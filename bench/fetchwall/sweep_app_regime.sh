#!/usr/bin/env bash
# App regime (MTP speculation on): base vs lookahead vs Turbo v2, cache 4 and 10.
cd "$(dirname "$0")"
for c in 4 10; do
  for rep in 1 2; do
    ./run_arm_server.sh base "$c";                                        sleep 25
    ./run_arm_server.sh la0 "$c" PGRN_LOOKAHEAD=0;                        sleep 25
    ./run_arm_server.sh turbo2 "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02;     sleep 25
  done
done
echo APP_DONE
