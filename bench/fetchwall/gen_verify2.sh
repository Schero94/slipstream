#!/usr/bin/env bash
# Lookahead ordering fix (router depends on the lookahead node) + cold-insert removal.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && T=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ")
  cmake --build build-static --target llama-server llama-completion llama-perplexity llama-pgrn-convert test-llama-archs $T -j 10 2>&1 | grep -E "error:" | head
  cd build-static && ctest -R peregrine 2>&1 | grep -E "tests passed|tests failed|\*\*\*" )
./run_arm.sh qwen-la0 4 PGRN_LOOKAHEAD=0; sleep 20
./run_arm.sh qwen-laoff 4 PGRN_LOOKAHEAD=-1; sleep 20
G=$HOME/Modelle/lfm2.5-24b-a2b-q4/LFM2.5-24B-A2B-UD-Q4_K_XL.gguf; P=${G%.gguf}.pgrn
for c in 4 8; do
  for rep in 1 2; do
    COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh lfm25-laoff "$c" PGRN_LOOKAHEAD=-1; sleep 20
    COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh lfm25-la "$c";                      sleep 20
  done
done
echo GEN2_DONE
