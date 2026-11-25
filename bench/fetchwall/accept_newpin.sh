#!/usr/bin/env bash
# Acceptance after switching PIN to the ported upstream: build, unit + e2e tests in the repo
# layout, static-link check, and parity/speed vs the old-pin binaries.
cd "$(dirname "$0")"
T=../../llama.cpp-slipstream
OLD="/Volumes/Crucial X10/SLipstream clone/llama.cpp-slipstream.pin-79bba02/build-static/bin"
( cd $T && cmake -B build-static -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF \
    -DGGML_METAL_EMBED_LIBRARY=ON -DCMAKE_OSX_ARCHITECTURES=arm64 -DLLAMA_BUILD_TESTS=ON >/dev/null 2>&1
  TT=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ")
  cmake --build build-static --target llama-server llama-completion llama-perplexity llama-pgrn-convert test-llama-archs $TT -j 10 2>&1 | grep -E "error:" | head
  cd build-static && ctest -R peregrine 2>&1 | tail -3 )
echo "== non-system dylibs:"; otool -L $T/build-static/bin/llama-server | tail -n +2 | grep -v -E "^\s*/(System|usr/lib)/" || echo none
BIN=$OLD/llama-completion ./run_arm.sh oldpin-la0 4 PGRN_LOOKAHEAD=0; sleep 25
./run_arm.sh newpin-la0 4 PGRN_LOOKAHEAD=0; sleep 25
BIN=$OLD/llama-completion ./run_arm.sh oldpin-la0 10 PGRN_LOOKAHEAD=0; sleep 25
./run_arm.sh newpin-la0 10 PGRN_LOOKAHEAD=0; sleep 25
BIN=$OLD/llama-server SPEC_ARGS=" " ./run_arm_server.sh oldpin-turbo2 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02; sleep 25
SPEC_ARGS=" " ./run_arm_server.sh newpin-turbo2 4 PGRN_LOOKAHEAD=0 PGRN_STEER=0.02; sleep 25
./run_arm_server.sh newpin-mtpbase 4
echo NEWPIN_DONE
