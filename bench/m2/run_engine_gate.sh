#!/usr/bin/env bash
# R2 gate A: does the streaming engine build and pass its own tests on Linux?
#
# The PGRN sources are compiled into libllama unconditionally — only
# peregrine_ane.mm is APPLE-guarded — so this gate answers a question the macOS
# build cannot: which of our code silently depends on Apple's headers, and which
# of our invariants are actually platform-independent.
#
# It builds libllama plus every test-peregrine-* target and runs them under
# ctest. test-peregrine-ane is expected to be absent (Apple-only by design);
# test-peregrine-model-e2e needs a real model and is excluded here.
#
# Usage: bench/m2/run_engine_gate.sh
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

WORK="$(mktemp -d /tmp/slip-m2-engine-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

require_docker
build_image
export_engine_tree "$WORK/engine.tar"

say "build and test on Linux"
docker run --rm -v "$WORK/engine.tar:/src.tar:ro" -w /work "$IMAGE" bash -lc '
set -e
mkdir -p src && tar xf /src.tar -C src
cd src
cmake -B build '"$CMAKE_FLAGS"' -DLLAMA_BUILD_TESTS=ON > /tmp/cfg.log 2>&1 || {
    echo "--- configure failed ---"; tail -40 /tmp/cfg.log; exit 1; }

cmake --build build --target llama -j "$(nproc)" > /tmp/lib.log 2>&1 || {
    echo "--- libllama failed ---"
    grep -E "error:|fatal error:" /tmp/lib.log | sed -E "s|/work/src/||" | sort -u | head -30
    exit 1; }
echo "libllama links; $(find build -name "peregrine_*.o" | wc -l | tr -d " ") peregrine objects"

# test-peregrine-ane is Objective-C++ and only exists on APPLE, so its target is
# absent here rather than failing — skip it without pretending it ran.
fail=""
for t in $(grep -oE "add_executable\(test-peregrine-[a-z0-9-]+" tests/CMakeLists.txt \
           | sed "s/add_executable(//" | sort -u | grep -v -- "-ane$"); do
    cmake --build build --target "$t" -j "$(nproc)" > "/tmp/b-$t.log" 2>&1 || fail="$fail $t"
done
if [ -n "$fail" ]; then
    echo "--- build failed:$fail ---"
    for t in $fail; do
        echo "  [$t]"
        grep -E "error:" "/tmp/b-$t.log" | sed -E "s|/work/src/||" | sort -u | head -6
    done
    exit 1
fi
echo "all portable peregrine test targets built"

cd build
ctest -R peregrine -E "model-e2e|-ane" --output-on-failure 2>&1 | tail -22
'

say "verdict"
echo "PASS — engine builds on Linux and its portable tests pass"
