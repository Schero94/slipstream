#!/usr/bin/env bash
# Reconstruct the Slipstream engine = upstream llama.cpp @ pinned commit + our engine/ + seams.
set -euo pipefail
PIN=79bba02a6741
DIR=${1:-llama.cpp-slipstream}
HERE=$(cd "$(dirname "$0")" && pwd)
git clone https://github.com/ggml-org/llama.cpp "$DIR"
cd "$DIR" && git checkout "$PIN"
cp -R "$HERE"/engine/* .
git apply "$HERE/patches/slipstream-seams.patch"
echo "Engine reconstructed in $DIR/"
echo "Build (self-contained, Metal-embedded, no external deps):"
echo "  cmake -B build-static -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \\"
echo "        -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF -DGGML_METAL_EMBED_LIBRARY=ON \\"
echo "        -DCMAKE_OSX_ARCHITECTURES=arm64"
echo "  cmake --build build-static --target llama-server -j"
