#!/usr/bin/env bash
# v0.4.1 release gate: fresh full build, tests, Qwen parity, app-regime default vs layer-major opt-in.
cd "$(dirname "$0")"
T=../../llama.cpp-slipstream
( cd $T && TT=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ")
  cmake --build build-static --target llama-server llama-completion llama-perplexity llama-pgrn-convert test-llama-archs $TT -j 10 2>&1 | grep -E "error:" | head
  cd build-static && TMPDIR="/Volumes/Crucial X10/SLipstream clone/tmp" ctest -R peregrine 2>&1 | grep -E "tests passed|tests failed|\*\*\*" )
B=$T/build-static/bin/llama-server
echo "binary $(stat -f %Sm -t %H:%M:%S $B); newest src $(ls -t $T/src/*.cpp $T/src/models/*.cpp $T/tools/server/*.cpp | head -1 | xargs stat -f %Sm -t %H:%M:%S); layer-major strings $(strings $B | grep -c PGRN_LAYER_MAJOR)"
./run_arm.sh gate-qwen-la 4; sleep 20
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=16 SPEC_ARGS=" "
EXTRA_ARGS="--cache-ram 2048 -fa on --jinja" UB=4096 ./run_arm_server.sh gate-app-default 4; sleep 25
EXTRA_ARGS="--cache-ram 2048 -fa on --jinja" B=40960 UB=2048 ./run_arm_server.sh gate-app-layermajor 4 PGRN_LAYER_MAJOR=1
echo GATE_DONE
