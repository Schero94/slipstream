# Shared plumbing for the R2 Linux gates. Source, do not execute.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENGINE_DIR="$REPO_ROOT/vendor/llama.cpp"
IMAGE=slipstream-linux-gate

say() { printf '\n=== %s ===\n' "$*"; }

require_docker() {
    docker version --format '{{.Server.Version}}' >/dev/null 2>&1 || {
        echo "no docker daemon; start Docker Desktop first" >&2
        exit 1
    }
}

build_image() {
    say "build the Linux image"
    docker build -q -t "$IMAGE" "$REPO_ROOT/bench/m2" | sed 's/^/image /'
}

# Export the engine *working tree* (not HEAD) so a gate run tests what is on disk.
# --others picks up files that are new and not yet committed: a gate that only saw
# tracked files would miss exactly the file a change just added.
# COPYFILE_DISABLE + --no-mac-metadata suppress AppleDouble `._*` siblings, which
# macOS tar otherwise emits for xattr-carrying files and which GCC then tries to
# compile as C++ source.
export_engine_tree() {
    local out="$1"
    say "export the engine working tree"
    ( cd "$ENGINE_DIR" \
      && COPYFILE_DISABLE=1 git ls-files -z --cached --others --exclude-standard \
         | tar cf "$out" --no-mac-metadata -C . --null -T - )
    local dirty
    dirty=$(git -C "$ENGINE_DIR" status --porcelain | wc -l | tr -d ' ')
    echo "engine at $(git -C "$ENGINE_DIR" rev-parse --short HEAD)" \
         "($dirty uncommitted), $(du -h "$out" | cut -f1)"
}

# Configure once, the same way, for every gate: no Metal, no curl, so the only
# thing under test is our own portability.
CMAKE_FLAGS='-DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF -DGGML_METAL=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=OFF'
