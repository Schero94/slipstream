#!/usr/bin/env bash
# Same server, MTP on vs off, with lookahead and Turbo v2 — does MTP still pay off once
# lookahead makes single-token decode cheap?
cd "$(dirname "$0")"
for c in 4 10; do
  for rep in 1 2; do
    SPEC_ARGS="--spec-type draft-mtp --spec-draft-n-max 4" ./run_arm_server.sh mtp-la0 "$c" PGRN_LOOKAHEAD=0;                                   sleep 25
    SPEC_ARGS=" " ./run_arm_server.sh none-la0 "$c" PGRN_LOOKAHEAD=0;                     sleep 25
    ./run_arm_server.sh mtp-turbo2 "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02;                 sleep 25
    SPEC_ARGS=" " ./run_arm_server.sh none-turbo2 "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02;  sleep 25
  done
done
echo MTPCMP_DONE
