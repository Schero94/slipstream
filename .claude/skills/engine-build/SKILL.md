---
name: engine-build
description: Reconstruct and build the Slipstream engine (llama.cpp fork + PGRN) as a self-contained static Metal binary, and optionally bundle it into the Tauri app. Use whenever you need llama-server / llama-completion / llama-perplexity / llama-pgrn-convert binaries or before benchmarking an engine change.
---

# Build the Slipstream engine

1. Reconstruct (only if `llama.cpp-slipstream/` is missing — it is gitignored):
   `./apply.sh` (set `UPSTREAM=<local llama.cpp clone>` to avoid a network clone).
2. Configure once:
   ```bash
   cd llama.cpp-slipstream
   cmake -B build-static -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
     -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF -DGGML_METAL_EMBED_LIBRARY=ON \
     -DCMAKE_OSX_ARCHITECTURES=arm64
   ```
3. Build only what you need (`-j 10` on the M3 Pro):
   `cmake --build build-static --target llama-server llama-completion llama-perplexity llama-pgrn-convert -j 10`
4. Check: `otool -L build-static/bin/llama-server` lists only system frameworks.

Rules:
- Never edit sources while a build is running (mixed objects → misleading A/B).
- Keep a copy of the pre-change binary (e.g. `../bin-baseline/`) when an A/B needs it;
  prefer env-gated features so one binary serves both arms.
- Bundle for the app: copy `llama-server` → `app/src-tauri/resources/llama-server`,
  `llama-pgrn-convert` → `app/src-tauri/resources/pgrn-convert`, then
  `cd app/src-tauri && cargo tauri build`.
- After any engine change, run the `engine-sync` skill before committing.
