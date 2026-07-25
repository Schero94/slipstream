The self-contained engine binary `llama-server` is placed here before bundling.
Build it via ../../apply.sh + cmake (see repo README), then:
  cp <llama.cpp-slipstream>/build-static/bin/llama-server ./llama-server
  cargo tauri build
