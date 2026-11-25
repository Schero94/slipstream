#!/usr/bin/env bash
# Turbo v2 quality on the new pin: decode-regime KL vs exact routing (new-pin base).
cd "$(dirname "$0")"
UB=1 CHUNKS=4 ./run_kld.sh base 4 PGRN_LOOKAHEAD=0
UB=1 CHUNKS=4 ./run_kld.sh newpin-turbo2 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02
UB=512 CHUNKS=12 ./run_kld.sh base 4 PGRN_LOOKAHEAD=0
UB=512 CHUNKS=12 ./run_kld.sh newpin-turbo2 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02
echo NPQ_DONE
