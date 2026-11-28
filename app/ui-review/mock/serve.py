"""Serve app/dist with the stateful __TAURI__ mock injected (UI review only).

    python3 app/ui-review/mock/serve.py [port]    # default 4317

Open http://127.0.0.1:4317/ and drive it with Playwright. Knobs live on window.__M
(see tauri_mock.js): freeRam, diskFree, speed, startCrash, startErr, prefillTokens,
editorRequest({total, cached, slots}) for /slots + log prompt progress of an editor request,
remote.* for "Your Macs" (found, code, useState, modelRunning, removedBy, peerPairs(), ...).
"""
import http.server, os, sys
HERE = os.path.dirname(os.path.abspath(__file__))
DIST = os.path.normpath(os.path.join(HERE, "..", "..", "dist"))
MOCK = os.path.join(HERE, "tauri_mock.js")

class H(http.server.SimpleHTTPRequestHandler):
    def __init__(s, *a, **k): super().__init__(*a, directory=DIST, **k)
    def end_headers(s):
        s.send_header("Cache-Control", "no-store"); super().end_headers()
    def do_GET(s):
        p = s.path.split("?")[0]
        if p == "/__mock.js":
            data = open(MOCK, "rb").read(); ct = "application/javascript"
        elif p in ("/", "/index.html"):
            html = open(os.path.join(DIST, "index.html"), encoding="utf-8").read()
            html = html.replace('<script src="app.js"', '<script src="/__mock.js"></script><script src="app.js"', 1)
            data = html.encode(); ct = "text/html; charset=utf-8"
        else:
            return super().do_GET()
        s.send_response(200); s.send_header("Content-Type", ct); s.send_header("Content-Length", str(len(data))); s.end_headers(); s.wfile.write(data)
    def log_message(s, *a): pass

http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 4317), H).serve_forever()
