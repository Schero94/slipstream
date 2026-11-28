#!/usr/bin/env bash
# Forced tool choice smoke on llama-server (app flags): tool_choice "required", named function, auto.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G="$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"; P=${G%.gguf}.pgrn; PORT=18088
"$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb 4 --pgrn-compact-slots --gpu-layers 99 -c 8192 --parallel 1 -fa on --jinja \
  --reasoning off --temp 0 --host 127.0.0.1 --port $PORT > "$HERE/results/toolchoice.log" 2>&1 &
PID=$!; trap 'kill $PID 2>/dev/null' EXIT
for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && break; sleep 1; done
python3 - $PORT <<'PY'
import json, sys, urllib.request
port = sys.argv[1]
tools = [{"type":"function","function":{"name":"read_file","description":"Read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}},
         {"type":"function","function":{"name":"list_dir","description":"List a directory","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}]
def ask(tc):
    body = {"messages":[{"role":"user","content":"Show me what is in the src folder."}],"tools":tools,"max_tokens":64,"temperature":0}
    if tc is not None: body["tool_choice"] = tc
    r = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions", data=json.dumps(body).encode(), headers={"Content-Type":"application/json"})
    try:
        m = json.load(urllib.request.urlopen(r, timeout=600))["choices"][0]["message"]
        calls = [(c["function"]["name"], c["function"]["arguments"]) for c in (m.get("tool_calls") or [])]
        return "ok", calls, (m.get("content") or "")[:60]
    except urllib.error.HTTPError as e:
        return f"HTTP {e.code}", e.read().decode()[:200], ""
for tc in [None, "auto", "required", {"type":"function","function":{"name":"read_file"}}, "none"]:
    print(json.dumps(tc), "->", ask(tc))
PY
