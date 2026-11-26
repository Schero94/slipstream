#!/usr/bin/env bash
# Lookahead capacity guard: Qwen3.6 unchanged, Mellum small caches back to baseline, all tests.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && T=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ")
  cmake --build build-static --target llama-server llama-completion llama-perplexity llama-pgrn-convert test-llama-archs $T -j 10 2>&1 | grep -E "error:" | head
  cd build-static && ctest -R peregrine 2>&1 | grep -E "tests passed|tests failed|\*\*\*" )
df -h / | tail -1
./run_arm.sh qwen-la0 4 PGRN_LOOKAHEAD=0; sleep 20
G=$HOME/Modelle/mellum2.1-12b-a2.5b-q4/Mellum2.1-12B-A2.5B-Instruct-Q4_K_M.gguf; P=${G%.gguf}.pgrn
for c in 1 2 8; do
  for rep in 1 2; do
    COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh mellum-laoff "$c" PGRN_LOOKAHEAD=-1; sleep 15
    COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh mellum-la "$c";                      sleep 15
  done
done
echo GEN3_DONE
