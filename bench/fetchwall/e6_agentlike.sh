#!/usr/bin/env bash
# E6: prefix reuse with a realistic agent request shape (OpenCode/Kilo style): system prompt with
# an <env> block (cwd, date) + a `tools` array of 12 tools + user task; files arrive as a tool
# result. C1 = conversation 1 (task, tool call, 35k-token tool result, follow-up ask).
# C2 = NEW conversation, same system + tools, different task -> should reuse system+tools.
# C3 = new conversation where the date in the system prompt changed -> what does that cost?
# App regime flags, LM=1 default (as the app). usage: e6_agentlike.sh LABEL
set -uo pipefail
LABEL=$1; CACHE=${CACHE:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-"$HERE/../../llama.cpp-slipstream/build-static/bin/llama-server"}
G=${GGUF:-"$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"}; P=${PGRN:-${G%.gguf}.pgrn}
PORT=18087; OUT="$HERE/results"; STAMP=$(date +%Y%m%d-%H%M%S); LOG="$OUT/$STAMP-e6-$LABEL.log"
swap_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
S0=$(swap_mb)
if [ "${LM:-1}" = 1 ]; then UB=2048; BB=40960; export PGRN_LAYER_MAJOR=1; else UB=4096; BB=4096; fi
"$BIN" -m "$G" --pgrn "$P" --pgrn-cache-gb "$CACHE" --pgrn-io-threads 8 --pgrn-compact-slots --gpu-layers 99 \
  -c 40960 --parallel 1 -b "$BB" -ub "$UB" -fa on --jinja --cache-ram 2048 ${EXTRA_ARGS:-} --host 127.0.0.1 --port $PORT > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null' EXIT INT TERM
for i in $(seq 1 600); do curl -sf http://127.0.0.1:$PORT/health >/dev/null && break; kill -0 $PID 2>/dev/null || break; sleep 1; done
kill -0 $PID 2>/dev/null || { echo "e6: server not healthy, see $LOG" >&2; exit 1; }
python3 - "$HERE/prompt_30k.txt" "$PORT" "$LABEL" "$STAMP" <<'PY' | tee -a "$OUT/summary.tsv"
import json, sys, urllib.request, hashlib
pf, port, label, stamp = sys.argv[1:5]
raw = open(pf).read()
files = raw.split("<|im_start|>user\n", 1)[1].rsplit("\n\nTask:", 1)[0]
files = files[:110000]  # ~29k tokens of source files as a tool result
rules = ("You are an interactive coding agent running in the user's terminal. Follow the repository's "
         "conventions, read files before editing them, keep changes minimal, run the tests after edits, "
         "and explain briefly what you changed. Never invent file paths; use the tools to look. ") * 12
def system(date):
    return (rules + "\n\n<env>\n  Working directory: /Users/dev/projects/slipstream\n  Is directory a git repo: yes\n"
            f"  Platform: darwin\n  Today's date: {date}\n</env>")
def tool(name, desc, props):
    return {"type": "function", "function": {"name": name, "description": desc + " " + ("Use absolute paths. Prefer this tool over shell commands when it fits. Results are truncated to 2000 lines; ask for a range if you need more. " * 6),
            "parameters": {"type": "object", "properties": {k: {"type": "string", "description": f"The {k} for {name}."} for k in props}, "required": props[:1]}}}
tools = [tool("bash", "Run a shell command in a persistent shell.", ["command", "timeout", "description"]),
         tool("read", "Read a file from the local filesystem.", ["file_path", "offset", "limit"]),
         tool("edit", "Replace an exact string in a file.", ["file_path", "old_string", "new_string"]),
         tool("write", "Write a file, overwriting it.", ["file_path", "content"]),
         tool("glob", "Find files by glob pattern.", ["pattern", "path"]),
         tool("grep", "Search file contents with a regex.", ["pattern", "path", "include"]),
         tool("list", "List a directory.", ["path", "ignore"]),
         tool("todowrite", "Write the session todo list.", ["todos"]),
         tool("todoread", "Read the session todo list.", ["session"]),
         tool("webfetch", "Fetch a URL and return markdown.", ["url", "format"]),
         tool("task", "Launch a sub-agent for a multi-step task.", ["description", "prompt"]),
         tool("patch", "Apply a unified diff to the repository.", ["patch_text"])]
def chat(msgs, n=48):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions", data=json.dumps(
        {"messages": msgs, "tools": tools, "max_tokens": n, "temperature": 0, "seed": 1, "cache_prompt": True,
         "chat_template_kwargs": {"enable_thinking": False}}).encode(), headers={"Content-Type": "application/json"})
    j = json.load(urllib.request.urlopen(r, timeout=3600))
    t = j["timings"]; m = j["choices"][0]["message"]
    out = (m.get("content") or "") + json.dumps([c["function"] for c in (m.get("tool_calls") or [])])  # ids are random
    return t["prompt_n"], t.get("cache_n", 0), t["prompt_ms"] / 1000, hashlib.sha256(out.encode()).hexdigest()[:12]
d1 = "Sat Nov 28 2026"
c1 = [{"role": "system", "content": system(d1)},
      {"role": "user", "content": "Where is the PGRN cache hit-rate computed for the UI? Read the relevant files."},
      {"role": "assistant", "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": json.dumps({"file_path": "/Users/dev/projects/slipstream/app/src-tauri/src/main.rs"})}}]},
      {"role": "tool", "tool_call_id": "call_1", "content": files},
      {"role": "user", "content": "Explain in two sentences how the hit-rate flows from the engine to the app."}]
r1 = chat(c1)
title = chat([{"role": "user", "content": "Write a five-word title for this session."}], 8)
c2 = [{"role": "system", "content": system(d1)},
      {"role": "user", "content": "Add a unit test for the slot key function in slotstate.rs."}]
r2 = chat(c2)
c3 = [{"role": "system", "content": system("Sun Nov 29 2026")},
      {"role": "user", "content": "Add a unit test for the slot key function in slotstate.rs."}]
r3 = chat(c3)
print(f"{stamp}\te6\tlabel={label}\tC1_prompt_n={r1[0]}\tC1_ttft_s={r1[2]:.1f}\t"
      f"C2_prompt_n={r2[0]}\tC2_cache_n={r2[1]}\tC2_ttft_s={r2[2]:.1f}\tC3_prompt_n={r3[0]}\tC3_cache_n={r3[1]}\tC3_ttft_s={r3[2]:.1f}\t"
      f"C1_out={r1[3]}\tC2_out={r2[3]}\tC3_out={r3[3]}")
PY
kill $PID; wait $PID 2>/dev/null
echo "e6 $LABEL swap_delta_mb=$(echo "$(swap_mb) - $S0" | bc)" | tee -a "$OUT/summary.tsv"
