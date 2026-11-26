#!/usr/bin/env bash
# Fully-resident-layer guard: Qwen unchanged, Mellum @8 GiB back to baseline, tests green.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && T=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ")
  cmake --build build-static --target llama-server llama-completion llama-perplexity llama-pgrn-convert test-llama-archs $T -j 10 2>&1 | grep -E "error:" | head
  cd build-static && ctest -R peregrine 2>&1 | grep -E "tests passed|tests failed|\*\*\*" )
B=../../llama.cpp-slipstream/build-static/bin/llama-server
echo "binary $(stat -f %Sm -t %H:%M:%S $B), layer-major strings $(strings $B | grep -c PGRN_LAYER_MAJOR)"
./run_arm.sh qwen-la0 4; sleep 20
./run_arm.sh qwen-la0 10; sleep 20
G=$HOME/Modelle/mellum2.1-12b-a2.5b-q4/Mellum2.1-12B-A2.5B-Instruct-Q4_K_M.gguf; P=${G%.gguf}.pgrn
for rep in 1 2; do
  COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh mellum-laoff 8 PGRN_LOOKAHEAD=-1; sleep 15
  COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh mellum-la 8;                      sleep 15
done
echo GEN4_DONE
