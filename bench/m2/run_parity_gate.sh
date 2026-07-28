#!/usr/bin/env bash
# R2 gate C: on Linux, does streaming experts from a PGRN produce the same tokens
# as holding the whole model resident?
#
# This is the gate the roadmap names for R2 — "one reference model on Linux with
# parity" — and the only one that exercises the inference path rather than the
# container format. Both arms run on the same machine, same CPU backend, same
# thread count, greedy sampling; the only difference is where expert weights come
# from, so any divergence is ours and not the platform's.
#
# Peak RSS is reported for both arms, because identical output alone would also be
# satisfied by a silent fallback to resident weights. The streamed arm has to be
# visibly smaller for the pass to mean anything.
#
# Throughput is deliberately not reported: the model is reached through virtiofs
# from a macOS host, so any number here would describe the VM, not the engine.
#
# Both arms run with -nr (no weight repacking), and that is the whole point of the
# comparison. The CPU backend can repack quantised weights into a blocked layout at
# load time, which changes the accumulation order of the matmul. A streamed expert
# cannot live in a repacked buffer at all — repacking rewrites a whole tensor, while
# streaming writes one expert into a slot at an offset — so a repacked resident arm
# versus a plain streamed arm would compare two different kernels and tell us nothing
# about whether the right bytes arrived. With -nr on both sides, the origin of the
# expert weights is the only variable left, and identical output means exactly that.
# A third, non-gating arm records what repacking alone does to the greedy text.
#
# Reference model: granite-3.0-1b-a400m-instruct Q4_K_M — a real 32-expert MoE,
# small enough that the resident arm fits in the Docker VM.
#
# Usage: bench/m2/run_parity_gate.sh [model_dir]
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MODEL_DIR="${1:-$HOME/Modelle/gate/granite-1b-a400m-q4}"
GGUF_NAME=granite-3.0-1b-a400m-instruct-Q4_K_M.gguf
PGRN_NAME=granite.pgrn
BUILD_DIR=/tmp/slip-m2-build   # persistent: repeated runs reconfigure, not rebuild
WORK="$(mktemp -d /tmp/slip-m2-parity-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

PROMPT='Name three prime numbers.'
NPREDICT=32
THREADS=4
CACHE_GB=0.25     # well under the 0.69 GiB of experts, so misses really happen
HEADROOM_GB=1

for f in "$GGUF_NAME" "$PGRN_NAME"; do
    [ -f "$MODEL_DIR/$f" ] || { echo "missing $MODEL_DIR/$f" >&2; exit 1; }
done
require_docker
build_image
mkdir -p "$BUILD_DIR"
export_engine_tree "$WORK/engine.tar"

say "build llama-completion on Linux and run both arms"
docker run --rm \
    -v "$WORK/engine.tar:/src.tar:ro" \
    -v "$BUILD_DIR:/build" \
    -v "$MODEL_DIR:/models:ro" \
    -v "$WORK:/out" \
    -w /build "$IMAGE" bash -lc '
set -e
rm -rf /build/src && mkdir -p /build/src && tar xf /src.tar -C /build/src
cd /build/src
cmake -B build '"$CMAKE_FLAGS"' -DLLAMA_BUILD_TESTS=OFF > /tmp/cfg.log 2>&1 || {
    echo "--- configure failed ---"; tail -30 /tmp/cfg.log; exit 1; }
cmake --build build --target llama-completion -j "$(nproc)" > /tmp/build.log 2>&1 || {
    echo "--- build failed ---"; tail -25 /tmp/build.log; exit 1; }
echo "llama-completion built"

# -no-cnv keeps this a plain completion: the chat template would otherwise switch
# the tool into interactive mode, where stdin decides when generation stops.
# An array, not a string: the prompt contains spaces, and word splitting would
# hand llama-completion a truncated -p and no usable diagnostics.
CLI=/build/src/build/bin/llama-completion
# Each arm names its own repacking choice, so that none of them silently inherits what
# the engine would have chosen. That choice is itself under test in arm 4.
# (No apostrophes below this line: the whole block is single-quoted for the container.)
COMMON=(-m "/models/'"$GGUF_NAME"'" -p "'"$PROMPT"'" -n '"$NPREDICT"' -t '"$THREADS"'
        --temp 0 --seed 1234 -ngl 0 -c 512 --no-warmup -no-cnv)

run_arm() {   # name, extra args...
    local name="$1"; shift
    /usr/bin/time -v "$CLI" "${COMMON[@]}" "$@" > "/out/$name.txt" 2> "/out/$name.err" </dev/null || {
        echo "--- $name arm failed, engine said: ---"
        grep -vE "^\s|^Command being timed" "/out/$name.err" | tail -15
        return 1; }
    awk "/Maximum resident set size/ {printf \"  $name peak RSS = %.0f MiB\n\", \$NF/1024}" "/out/$name.err"
}

echo "--- arm 1: resident (experts from the GGUF) ---"
run_arm resident -nr
echo "--- arm 2: streamed (experts from the PGRN) ---"
run_arm streamed -nr --pgrn /models/'"$PGRN_NAME"' \
    --pgrn-cache-gb '"$CACHE_GB"' --pgrn-headroom-gb '"$HEADROOM_GB"' --pgrn-io-threads 2
# Informational only: what the layout change costs in token terms. Worth knowing, but
# it is the CPU backend under test here rather than anything of ours.
echo "--- arm 3: resident with repacking (informational) ---"
run_arm resident_repack --repack
# The shape a Linux user actually has: no control app to compute a cache budget and a
# reserve, and no reason to know about repacking, so --pgrn has to be enough on its own.
# Admission derives the budget and the reserve from the host (on Linux, from the cgroup
# limit where one is set), and streaming turns repacking off by itself so that this run
# lands in the same numeric regime as the resident baseline. Gated on identical output:
# a default that answers differently from the documented baseline is not a usable one.
echo "--- arm 4: streamed with nothing but --pgrn (the real default) ---"
run_arm derived --pgrn /models/'"$PGRN_NAME"'
'

say "compare"
RES_RSS=$(awk '/Maximum resident set size/ {printf "%.0f", $NF/1024}' "$WORK/resident.err")
STR_RSS=$(awk '/Maximum resident set size/ {printf "%.0f", $NF/1024}' "$WORK/streamed.err")
DER_RSS=$(awk '/Maximum resident set size/ {printf "%.0f", $NF/1024}' "$WORK/derived.err")
echo "peak RSS: resident ${RES_RSS} MiB, streamed ${STR_RSS} MiB, derived ${DER_RSS} MiB"
# The derived arm is expected to be the largest of the three, and that is the policy
# working rather than failing: with room to spare it caches every expert, which is the
# fastest thing to do. Streaming saves memory when the model does not fit, not when it
# does — arm 2 forces that case with a deliberately small cache.
echo "  (derived caches every expert because this host has room; the saving in arm 2"
echo "   comes from capping the cache at ${CACHE_GB} GiB, which is the >RAM case)"
echo "generated text:"
sed 's/^/  | /' "$WORK/resident.txt"

FAIL=0
for arm in streamed derived; do
    if ! diff -q "$WORK/resident.txt" "$WORK/$arm.txt" >/dev/null; then
        echo "$arm output differs from resident:"
        # diff exits 1 on a difference, which under pipefail plus set -e would end the
        # script here and swallow the verdict. A difference is the message, not an error.
        diff "$WORK/resident.txt" "$WORK/$arm.txt" | head -30 | sed 's/^/  /' || true
        FAIL=1
    fi
done
# A streamed arm that is not smaller did not stream.
if [ "$STR_RSS" -ge "$RES_RSS" ]; then
    echo "streamed arm is not smaller than resident — experts were probably not streamed"
    FAIL=1
fi
if diff -q "$WORK/resident.txt" "$WORK/resident_repack.txt" >/dev/null; then
    echo "note: repacking left the greedy text unchanged on this prompt"
else
    echo "note: repacking alone changes the greedy text (different accumulation order),"
    echo "      which is why both gated arms run with -nr:"
    diff "$WORK/resident.txt" "$WORK/resident_repack.txt" | head -12 | sed 's/^/        /' || true
fi

say "verdict"
if [ "$FAIL" = "0" ]; then
    echo "PASS — identical output, and streaming saved $((RES_RSS - STR_RSS)) MiB of resident memory"
else
    echo "FAIL"
    exit 1
fi
