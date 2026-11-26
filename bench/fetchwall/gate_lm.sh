#!/usr/bin/env bash
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=16 SPEC_ARGS=" "
EXTRA_ARGS="--cache-ram 2048 -fa on --jinja" B=40960 UB=2048 ./run_arm_server.sh gate-app-layermajor 4 PGRN_LAYER_MAJOR=1
echo GATELM_DONE
