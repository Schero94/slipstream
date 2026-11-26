#!/usr/bin/env bash
# After the shared-lookahead move: full ctest summary, Turbo decode quality (new impl), LFM2.5 debug.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && T=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ")
  cmake --build build-static --target llama-server llama-completion llama-perplexity test-llama-archs $T -j 10 2>&1 | grep -E "error:" | head
  cd build-static && ctest -R peregrine 2>&1 | grep -E "tests passed|tests failed|Failed|\*\*\*" )
UB=1 CHUNKS=4 ./run_kld.sh base 4 PGRN_LOOKAHEAD=0
UB=1 CHUNKS=4 ./run_kld.sh turbo-shared 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02
G=$HOME/Modelle/lfm2.5-24b-a2b-q4/LFM2.5-24B-A2B-UD-Q4_K_XL.gguf; P=${G%.gguf}.pgrn
COMPACT=" " GGUF=$G PGRN=$P NTOK=16 ./run_arm.sh lfm25-debug 4 PGRN_DEBUG=1
grep -h "PGRN_DEBUG" $(ls -t results/*-lfm25-debug-c4.log | head -1) | head -6
echo GENCHECK_DONE
