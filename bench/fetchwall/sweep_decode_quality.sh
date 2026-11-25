#!/usr/bin/env bash
# Decode-regime quality gate (ubatch 1): steer variants incl. the PSR mark vs exact routing.
cd "$(dirname "$0")"
export UB=1 CHUNKS=${CHUNKS:-4}
C=${CACHE:-4}
./run_kld.sh base "$C" PGRN_LOOKAHEAD=0
./run_kld.sh la0 "$C" PGRN_LOOKAHEAD=0
./run_kld.sh st0.01 "$C" PGRN_LOOKAHEAD=0 PGRN_STEER=0.01
./run_kld.sh st0.01la "$C" PGRN_LOOKAHEAD=0 PGRN_STEER=0.01 PGRN_STEER_LA=1
./run_kld.sh st0.02la "$C" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02 PGRN_STEER_LA=1
echo DQ_DONE
