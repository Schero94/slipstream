#!/usr/bin/env bash
# Compact path experiments: parallel cold reads (PAR) and compact for small batches (BATCH).
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && cmake --build build-static --target llama-server llama-completion -j 10 2>&1 | grep -E "error:" | head )
for c in 4 10; do
  for rep in 1 2; do
    ./run_arm.sh la0 "$c" PGRN_LOOKAHEAD=0;                         sleep 25
    ./run_arm.sh la0par "$c" PGRN_LOOKAHEAD=0 PGRN_COMPACT_PAR=1;   sleep 25
  done
done
for rep in 1 2; do
  SPEC_ARGS=" " ./run_arm_server.sh none-la0 4 PGRN_LOOKAHEAD=0;                                        sleep 25
  SPEC_ARGS=" " ./run_arm_server.sh none-la0par 4 PGRN_LOOKAHEAD=0 PGRN_COMPACT_PAR=1;                  sleep 25
  ./run_arm_server.sh mtp-la0 4 PGRN_LOOKAHEAD=0;                                                       sleep 25
  ./run_arm_server.sh mtp-la0batch 4 PGRN_LOOKAHEAD=0 PGRN_COMPACT_BATCH=1 PGRN_COMPACT_PAR=1;          sleep 25
done
echo COMPACT_DONE
