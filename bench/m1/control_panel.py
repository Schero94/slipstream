"""Peregrine control panel — a local UI to steer and test the offline coding engine.

A dependency-free stdlib HTTP server that serves a single-page dashboard plus a
small JSON API. It surfaces the live state of the engine and lets you exercise it:

* GET  /api/status   — gateway health + memory headroom (wired/free/compressed)
* POST /api/complete — send a test prompt to the gateway, report decode tok/s
* POST /api/retrieve — run bounded-window repo retrieval for a query
* POST /api/plan     — resident-vs-streaming load plan for a hypothetical model

The routing/composition logic is pure and unit-tested; the HTTP glue is thin.
Everything is loopback-only. This is the "steuern/testen mit UI" layer.
"""

from __future__ import annotations

import json
import re
import subprocess
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable

DEFAULT_GATEWAY_URL = "http://127.0.0.1:8080"
GIB = 1024 ** 3


class ControlPanelError(Exception):
    """Raised for malformed control-panel inputs."""


def parse_vm_stat(text: str) -> dict[str, int]:
    page_match = re.search(r"page size of (\d+) bytes", text)
    if not page_match:
        raise ControlPanelError("vm_stat output missing page size")
    page = int(page_match.group(1))

    def pages(key: str) -> int:
        m = re.search(rf"{re.escape(key)}:\s*(\d+)", text)
        return int(m.group(1)) if m else 0

    return {
        "page_size": page,
        "wired_bytes": pages("Pages wired down") * page,
        "free_bytes": pages("Pages free") * page,
        "compressed_bytes": pages("Pages occupied by compressor") * page,
    }


def status_view(health: dict | None, memory: dict, *, total_bytes: int) -> dict[str, Any]:
    non_reclaimable = memory["wired_bytes"] + memory["compressed_bytes"]
    return {
        "gateway": "running" if (health and health.get("status") == "ok") else "offline",
        "gateway_pid": (health or {}).get("pid"),
        "memory": {
            "total_gib": round(total_bytes / GIB, 1),
            "wired_gib": round(memory["wired_bytes"] / GIB, 1),
            "compressed_gib": round(memory["compressed_bytes"] / GIB, 1),
            "free_gib": round(memory["free_bytes"] / GIB, 1),
            "non_reclaimable_gib": round(non_reclaimable / GIB, 1),
            "headroom_gib": round((total_bytes - non_reclaimable) / GIB, 1),
        },
    }


# ---- external effects (injectable for tests) ----

def _fetch_health(gateway_url: str) -> dict | None:
    try:
        with urllib.request.urlopen(f"{gateway_url}/peregrine/health", timeout=2) as resp:
            return json.loads(resp.read())
    except Exception:
        return None


def _read_memory() -> dict[str, int]:
    out = subprocess.run(["vm_stat"], capture_output=True, text=True, timeout=5).stdout
    return parse_vm_stat(out)


def _detect_total() -> int:
    out = subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, timeout=5)
    return int(out.stdout.strip())


def _complete(gateway_url: str, prompt: str, max_tokens: int) -> dict[str, Any]:
    body = json.dumps(
        {
            "model": "peregrine-qualification",
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens,
            "temperature": 0,
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        f"{gateway_url}/v1/chat/completions",
        data=body,
        headers={"Content-Type": "application/json", "Authorization": "Bearer local"},
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.loads(resp.read().decode("utf-8", "replace"), strict=False)
    choice = data["choices"][0]["message"]
    timings = data.get("timings", {})
    return {
        "content": choice.get("content") or choice.get("reasoning_content", "")[:2000],
        "decode_tok_s": round(timings.get("predicted_per_second", 0.0), 2),
        "tokens": data.get("usage", {}).get("completion_tokens"),
    }


def _retrieve(query: str, budget: int) -> dict[str, Any]:
    from bench.m1.retrieval import estimate_tokens, retrieve_repo

    result = retrieve_repo(Path.cwd(), query, budget_tokens=budget)
    return {
        "files_scanned": result["files_scanned"],
        "tokens_used": result["tokens_used"],
        "selected": [
            {"file": c.file, "start": c.start_line, "end": c.end_line, "tokens": estimate_tokens(c.text)}
            for c in result["selected"][:20]
        ],
    }


def _plan(model_gb: float, ctx: int) -> dict[str, Any]:
    from bench.m0a.constants import DEV_EXPERTS, DEV_LAYERS, INT4_EXPERT_BYTES
    from bench.m1.memory_admission import plan_load

    total = _detect_total()
    expert_total = DEV_LAYERS * DEV_EXPERTS * INT4_EXPERT_BYTES
    model = int(model_gb * 1e9)
    plan = plan_load(
        total_bytes=total,
        available_bytes=None,
        model_bytes=model,
        expert_total_bytes=min(expert_total, int(model * 0.8)),
        kv_bytes=int(ctx * 102_400),
        overhead_bytes=int(1.5 * GIB),
        layers=DEV_LAYERS,
        expert_bytes=INT4_EXPERT_BYTES,
    )
    return {k: plan[k] for k in ("mode", "resident_bytes", "streamed_expert_bytes", "reason") if k in plan}


def api_router(
    path: str,
    method: str,
    body: dict | None,
    *,
    fetch_health: Callable[[], dict | None],
    memory_fn: Callable[[], dict],
    total_bytes: int,
    complete_fn: Callable[[str, int], dict],
    retrieve_fn: Callable[[str, int], dict],
    plan_fn: Callable[[float, int], dict],
) -> tuple[int, dict[str, Any]]:
    if path == "/api/status" and method == "GET":
        return 200, status_view(fetch_health(), memory_fn(), total_bytes=total_bytes)
    if path == "/api/complete" and method == "POST":
        prompt = (body or {}).get("prompt")
        if not isinstance(prompt, str) or not prompt.strip():
            return 400, {"error": "prompt required"}
        max_tokens = int((body or {}).get("max_tokens", 200))
        try:
            return 200, complete_fn(prompt, max_tokens)
        except Exception as error:  # gateway offline / error
            return 502, {"error": f"gateway request failed: {error}"}
    if path == "/api/retrieve" and method == "POST":
        query = (body or {}).get("query")
        if not isinstance(query, str) or not query.strip():
            return 400, {"error": "query required"}
        return 200, retrieve_fn(query, int((body or {}).get("budget", 4000)))
    if path == "/api/plan" and method == "POST":
        return 200, plan_fn(float((body or {}).get("model_gb", 40)), int((body or {}).get("ctx", 32768)))
    return 404, {"error": "not found"}


def render_index_html() -> str:
    return """<!doctype html><html><head><meta charset="utf-8">
<title>Peregrine Control Panel</title>
<style>
:root{color-scheme:light dark}
body{font-family:-apple-system,system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;line-height:1.5}
h1{font-size:1.4rem} .card{border:1px solid #8884;border-radius:10px;padding:1rem;margin:1rem 0}
.row{display:flex;gap:1rem;flex-wrap:wrap} .stat{flex:1;min-width:120px}
.stat b{display:block;font-size:1.5rem} textarea,input{width:100%;box-sizing:border-box;font:inherit;padding:.5rem;border-radius:8px;border:1px solid #8886;background:transparent;color:inherit}
button{font:inherit;padding:.5rem 1rem;border-radius:8px;border:0;background:#3b82f6;color:#fff;cursor:pointer}
pre{white-space:pre-wrap;background:#8881;padding:.75rem;border-radius:8px;max-height:340px;overflow:auto}
.dot{display:inline-block;width:.7rem;height:.7rem;border-radius:50%;margin-right:.4rem}
.on{background:#22c55e}.off{background:#ef4444}
</style></head><body>
<h1>🦅 Peregrine — Control Panel</h1>
<div class="card"><div class="row" id="status">loading status…</div></div>
<div class="card"><h3>Test a prompt</h3>
<textarea id="prompt" rows="3">Write a Python function to merge two sorted lists.</textarea>
<p><input id="maxtok" type="number" value="200" style="width:8rem"> max tokens
<button onclick="runComplete()">Run</button></p>
<pre id="cout">—</pre></div>
<div class="card"><h3>Repo retrieval</h3>
<input id="query" value="gateway serve status stop">
<p><button onclick="runRetrieve()">Retrieve</button></p>
<pre id="rout">—</pre></div>
<script>
async function refresh(){
 try{const s=await (await fetch('/api/status')).json();
  const m=s.memory;
  document.getElementById('status').innerHTML=
   `<div class="stat"><b><span class="dot ${s.gateway==='running'?'on':'off'}"></span>${s.gateway}</b>gateway</div>`+
   `<div class="stat"><b>${m.wired_gib}</b>GiB wired</div>`+
   `<div class="stat"><b>${m.headroom_gib}</b>GiB headroom</div>`+
   `<div class="stat"><b>${m.free_gib}</b>GiB free</div>`;
 }catch(e){document.getElementById('status').textContent='status error: '+e}
}
async function runComplete(){
 const o=document.getElementById('cout');o.textContent='running…';
 const r=await fetch('/api/complete',{method:'POST',headers:{'Content-Type':'application/json'},
  body:JSON.stringify({prompt:document.getElementById('prompt').value,max_tokens:+document.getElementById('maxtok').value})});
 const d=await r.json();
 o.textContent=d.error?('error: '+d.error):(`decode ${d.decode_tok_s} tok/s · ${d.tokens} tokens\\n\\n`+d.content);
}
async function runRetrieve(){
 const o=document.getElementById('rout');o.textContent='…';
 const r=await fetch('/api/retrieve',{method:'POST',headers:{'Content-Type':'application/json'},
  body:JSON.stringify({query:document.getElementById('query').value,budget:3000})});
 const d=await r.json();
 o.textContent=d.error?('error: '+d.error):(`scanned ${d.files_scanned} files · ${d.tokens_used} tokens\\n`+
  d.selected.map(s=>`${s.file}:${s.start}-${s.end} (${s.tokens})`).join('\\n'));
}
refresh();setInterval(refresh,4000);
</script></body></html>"""


def _make_handler(gateway_url: str):
    total = _detect_total()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            return

        def _send(self, code: int, body: bytes, ctype: str):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/" or self.path == "/index.html":
                self._send(200, render_index_html().encode(), "text/html; charset=utf-8")
                return
            self._route("GET", None)

        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            raw = self.rfile.read(length) if length else b""
            try:
                body = json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                body = {}
            self._route("POST", body)

        def _route(self, method: str, body: dict | None):
            code, data = api_router(
                self.path, method, body,
                fetch_health=lambda: _fetch_health(gateway_url),
                memory_fn=_read_memory,
                total_bytes=total,
                complete_fn=lambda p, m: _complete(gateway_url, p, m),
                retrieve_fn=_retrieve,
                plan_fn=_plan,
            )
            self._send(code, json.dumps(data).encode(), "application/json")

    return Handler


def serve(host: str = "127.0.0.1", port: int = 8090, gateway_url: str = DEFAULT_GATEWAY_URL) -> int:
    if host not in {"127.0.0.1", "::1", "localhost"}:
        raise ControlPanelError("control panel must bind loopback")
    httpd = ThreadingHTTPServer((host, port), _make_handler(gateway_url))
    print(f"Peregrine control panel: http://{host}:{port}  (gateway: {gateway_url})")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8090)
    parser.add_argument("--gateway-url", default=DEFAULT_GATEWAY_URL)
    args = parser.parse_args(argv)
    return serve(args.host, args.port, args.gateway_url)


if __name__ == "__main__":
    raise SystemExit(main())
