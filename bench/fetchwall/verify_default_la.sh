#!/usr/bin/env bash
# Lookahead default-on: no env must equal PGRN_LOOKAHEAD=0, -1 must equal the old baseline.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && cmake --build build-static --target llama-server llama-completion -j 10 2>&1 | grep -E "error:" | head )
./run_arm.sh default 4; sleep 25
./run_arm.sh la-off 4 PGRN_LOOKAHEAD=-1; sleep 25
./run_arm.sh la0 4 PGRN_LOOKAHEAD=0
echo DEFLA_DONE
