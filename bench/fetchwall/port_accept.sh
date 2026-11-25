#!/usr/bin/env bash
# Acceptance of the upstream port: build + unit/e2e tests on the port tree, then app-regime
# A/B old binary vs new binary (same server flags, same prompt, greedy).
cd "$(dirname "$0")"
PORT="/Volumes/Crucial X10/SLipstream clone/upstream-port/llama.cpp"
OLD="$PWD/../../llama.cpp-slipstream/build-static/bin/llama-server"
NEW="$PORT/build-static/bin/llama-server"
( cd "$PORT" && T=$(grep -o "test-peregrine-[a-z0-9-]*" tests/CMakeLists.txt | sort -u | grep -v e2e | tr "\n" " ");
  cmake --build build-static --target llama-server llama-completion llama-perplexity llama-pgrn-convert test-llama-archs $T -j 10 2>&1 | grep -E "error:" | head -20;
  cd build-static && cmake .. >/dev/null 2>&1 && ctest -R peregrine 2>&1 | tail -4 )
otool -L "$NEW" | grep -v -E "/System/|/usr/lib/" | tail -n +2
for c in 4 10; do
  for rep in 1 2; do
    BIN=$OLD SPEC_ARGS=" " ./run_arm_server.sh old-la0 "$c" PGRN_LOOKAHEAD=0;               sleep 25
    BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh new-la0 "$c" PGRN_LOOKAHEAD=0;               sleep 25
    BIN=$OLD SPEC_ARGS=" " ./run_arm_server.sh old-turbo2 "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02; sleep 25
    BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh new-turbo2 "$c" PGRN_LOOKAHEAD=0 PGRN_STEER=0.02; sleep 25
  done
done
BIN=$OLD ./run_arm_server.sh old-mtpbase 4; sleep 25
BIN=$NEW ./run_arm_server.sh new-mtpbase 4
echo PORT_DONE
