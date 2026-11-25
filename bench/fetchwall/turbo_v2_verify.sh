#!/usr/bin/env bash
# Verify the decode-only Turbo v2 build: prefill KL must be ~0 now, decode KL ~0.020,
# speed unchanged vs the env-flag PSR arm.
cd "$(dirname "$0")"
UB=512 CHUNKS=12 ./run_kld.sh turbo2 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02
UB=1   CHUNKS=4  ./run_kld.sh turbo2 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02
for rep in 1 2; do
  ./run_arm.sh la0 4 PGRN_LOOKAHEAD=0; sleep 25
  ./run_arm.sh turbo2 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02; sleep 25
done
echo TURBO2_DONE
