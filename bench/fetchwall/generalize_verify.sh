#!/usr/bin/env bash
# Lookahead/Turbo moved into the shared build_moe_ffn: Qwen3.6 must be unchanged (hash, speed),
# other archs (LFM2.5, lfm2moe) must gain from lookahead, all tests green.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && T=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ")
  cmake --build build-static --target llama-server llama-completion llama-perplexity test-llama-archs $T -j 10 2>&1 | grep -E "error:" | head
  cd build-static && ctest -R peregrine 2>&1 | tail -2 )
./run_arm.sh qwen-la0 4 PGRN_LOOKAHEAD=0; sleep 25
./run_arm.sh qwen-turbo 4 PGRN_STEER=0.02; sleep 25
G=$HOME/Modelle/lfm2.5-24b-a2b-q4/LFM2.5-24B-A2B-UD-Q4_K_XL.gguf; P=${G%.gguf}.pgrn
for c in 4 8; do
  for rep in 1 2; do
    COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh lfm25-laoff "$c" PGRN_LOOKAHEAD=-1; sleep 25
    COMPACT=" " GGUF=$G PGRN=$P ./run_arm.sh lfm25-la "$c";                      sleep 25
  done
done
echo GEN_DONE
