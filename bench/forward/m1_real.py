#!/usr/bin/env python3
"""v0.6 M1 real-engine checks for bench/forward/run_m1.sh (direct llama-server vs the forwarder).

  m1_real.py fidelity --direct 127.0.0.1:18080 --fwd 127.0.0.1:18191 --reps 2 --out DIR
  m1_real.py cancel   --fwd 127.0.0.1:18191 --log llama.log --reps 3 --wait 6
  m1_real.py stats    --tsv run.tsv --base direct --arm fwd --field event_rate|ttft_us
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import socket
import statistics
import sys
import time
from pathlib import Path

# Extra request header lines (e.g. "Authorization: Bearer <token>\r\n" for a `use --token-file`
# forwarder); bench/forward/v06_qual.py sets it. Empty = no extra headers (M1 behaviour).
EXTRA_HEADERS = ""

CASES = {
    "plain": {"messages": [{"role": "user", "content": "Write a Python function that checks whether a string is a palindrome, with a docstring and two doctests."}]},
    "json": {
        "messages": [{"role": "user", "content": "Return a JSON object describing a book: title, author, year, tags (3 strings)."}],
        "response_format": {"type": "json_schema", "json_schema": {"name": "book", "strict": True, "schema": {
            "type": "object", "additionalProperties": False, "required": ["title", "author", "year", "tags"],
            "properties": {"title": {"type": "string"}, "author": {"type": "string"}, "year": {"type": "integer"},
                           "tags": {"type": "array", "items": {"type": "string"}}}}}},
    },
    "tool": {
        "messages": [{"role": "user", "content": "What is 19 + 23? Use the tool."}],
        "tools": [{"type": "function", "function": {"name": "add", "description": "Add two integers",
                   "parameters": {"type": "object", "required": ["a", "b"], "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}}}}],
        "tool_choice": {"type": "function", "function": {"name": "add"}},
    },
}


def post_stream(addr: str, path: str, body: dict, timeout: float = 600.0) -> bytes:
    host, port = addr.rsplit(":", 1)
    data = json.dumps(body).encode()
    req = (f"POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n"
           f"Accept: text/event-stream\r\n{EXTRA_HEADERS}Content-Length: {len(data)}\r\nConnection: close\r\n\r\n").encode() + data
    with socket.create_connection((host, int(port)), timeout=timeout) as s:
        s.sendall(req)
        out = bytearray()
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            out += chunk
    return bytes(out)


def dechunk(body: bytes) -> bytes:
    out = bytearray()
    while body:
        i = body.find(b"\r\n")
        if i < 0:
            break
        size = int(body[:i].split(b";")[0] or b"0", 16)
        body = body[i + 2:]
        if size == 0:
            break
        out += body[:size]
        body = body[size + 2:]
    return bytes(out)


def events(raw: bytes) -> list[dict]:
    head, _, body = raw.partition(b"\r\n\r\n")
    if b"chunked" in head.lower():
        body = dechunk(body)
    evs = []
    for line in body.split(b"\n"):
        if line.startswith(b"data: ") and line[6:].strip() != b"[DONE]":
            evs.append(json.loads(line[6:]))
    return evs


def semantic(raw: bytes) -> dict:
    """Everything that is not a timestamp/id: per-event choices, final text, tool calls."""
    evs = events(raw)
    choices = [e.get("choices") for e in evs if "choices" in e]
    text = "".join((c[0].get("delta", {}).get("content") or "") for c in choices if c)
    calls: dict[int, dict] = {}
    for c in choices:
        if not c:
            continue
        for tc in c[0].get("delta", {}).get("tool_calls") or []:
            d = calls.setdefault(tc.get("index", 0), {"name": "", "arguments": ""})
            f = tc.get("function") or {}
            d["name"] += f.get("name") or ""
            d["arguments"] += f.get("arguments") or ""
    finish = [c[0].get("finish_reason") for c in choices if c and c[0].get("finish_reason")]
    # llama-server draws a random tool-call "id" per request (like the chunk id / created):
    # blank it so every other byte of every choice is compared.
    for c in choices:
        for ch in c or []:
            for tc in (ch.get("delta") or {}).get("tool_calls") or []:
                if "id" in tc:
                    tc["id"] = "<random>"
    canon = json.dumps(choices, sort_keys=True, separators=(",", ":"))
    return {"events": len(evs), "choices_sha256": hashlib.sha256(canon.encode()).hexdigest(),
            "text": text, "tool_calls": [calls[k] for k in sorted(calls)], "finish": finish}


def cmd_fidelity(a) -> int:
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    ok = True
    for name, extra in CASES.items():
        body = {"model": "slipstream", "stream": True, "max_tokens": a.max_tokens, "temperature": 0, "seed": 1,
                "cache_prompt": False, **extra}
        seen = {}
        for rep in range(a.reps):
            for arm, addr in (("direct", a.direct), ("fwd", a.fwd)):
                raw = post_stream(addr, "/v1/chat/completions", body)
                (out / f"{name}-{arm}-{rep}.sse").write_bytes(raw)
                sem = semantic(raw)
                seen.setdefault(arm, []).append(sem)
                time.sleep(a.cooldown)
        ref = seen["direct"][0]
        same = all(s["choices_sha256"] == ref["choices_sha256"] for arm in seen for s in seen[arm])
        ok &= same and ref["events"] > 0
        print(f"fidelity case={name} identical={same} events={ref['events']} choices_sha={ref['choices_sha256'][:16]} "
              f"finish={ref['finish']} text={json.dumps(ref['text'][:80])} tool_calls={json.dumps(ref['tool_calls'])[:160]}")
        for arm in seen:
            print(f"  {arm}: " + " ".join(s['choices_sha256'][:12] for s in seen[arm]))
    return 0 if ok else 1


def cmd_recheck(a) -> int:
    ok = True
    for name in CASES:
        files = sorted(Path(a.dir).glob(f"{name}-*.sse"))
        sems = {f.name: semantic(f.read_bytes()) for f in files}
        shas = {k: v["choices_sha256"][:12] for k, v in sems.items()}
        same = len(set(shas.values())) == 1 and files
        ok &= bool(same)
        print(f"recheck case={name} identical={bool(same)} files={len(files)} {shas}")
    return 0 if ok else 1


def long_prompt(n_words: int) -> str:
    base = ("def handler(event, context):\n    # parse the incoming order event and update the ledger\n"
            "    order = json.loads(event['body'])\n    total = sum(item['price'] * item['qty'] for item in order['items'])\n")
    words = base.split()
    return "Review this code carefully and list every bug:\n" + " ".join(words[i % len(words)] for i in range(n_words))


def cmd_cancel(a) -> int:
    host, port = a.fwd.rsplit(":", 1)
    log = Path(a.log)
    ok = True
    for rep in range(a.reps):
        before = log.read_text(errors="replace") if log.exists() else ""
        body = {"model": "slipstream", "stream": True, "max_tokens": 32, "temperature": 0, "cache_prompt": False,
                "messages": [{"role": "user", "content": long_prompt(a.words) + f" (run {rep} {time.time()})"}]}
        data = json.dumps(body).encode()
        s = socket.create_connection((host, int(port)))
        s.sendall((f"POST /v1/chat/completions HTTP/1.1\r\nHost: {a.fwd}\r\nContent-Type: application/json\r\n"
                   f"{EXTRA_HEADERS}Content-Length: {len(data)}\r\nConnection: close\r\n\r\n").encode() + data)
        time.sleep(a.wait)
        slots = post_get(a.fwd, "/slots")
        t0 = time.time()
        s.close()  # editor stop button
        found = None
        for _ in range(int(a.timeout * 20)):
            new = (log.read_text(errors="replace") if log.exists() else "")[len(before):]
            m = re.search(r"layer-major prefill aborted ([0-9.]+) s after the cancel", new)
            if m and "prompt_abort" in new:
                found = (m.group(1), time.time() - t0, new)
                break
            time.sleep(0.05)
        if not found:
            ok = False
            print(f"cancel rep={rep} NOT FOUND in log within {a.timeout}s; slots={slots[:200]}")
            continue
        lm = re.search(r"layer-major prefill aborted at layer (\d+)/(\d+)[^\n]*", found[2])
        pa = re.search(r"slot prompt_abort:[^\n]*", found[2])
        print(f"cancel rep={rep} engine_abort_after_cancel_s={found[0]} close_to_log_s={found[1]:.2f} "
              f"where={lm.group(0) if lm else 'final pass'} | {pa.group(0) if pa else ''} | slots_via_forwarder={slots}")
        time.sleep(a.cooldown)
    return 0 if ok else 1


def post_get(addr: str, path: str) -> str:
    host, port = addr.rsplit(":", 1)
    with socket.create_connection((host, int(port)), timeout=5) as s:
        s.sendall(f"GET {path} HTTP/1.1\r\nHost: {addr}\r\n{EXTRA_HEADERS}Connection: close\r\n\r\n".encode())
        out = bytearray()
        while (c := s.recv(65536)):
            out += c
    head, _, body = bytes(out).partition(b"\r\n\r\n")
    if b"chunked" in head.lower():
        body = dechunk(body)
    return body.decode(errors="replace")


T95 = {1: 12.71, 2: 4.30, 3: 3.18, 4: 2.78, 5: 2.57, 6: 2.45, 7: 2.36, 8: 2.31, 9: 2.26, 10: 2.23, 11: 2.20,
       12: 2.18, 13: 2.16, 14: 2.14, 15: 2.13, 19: 2.09, 24: 2.06, 29: 2.05, 39: 2.02, 59: 2.00}


def t95(df: int) -> float:
    keys = sorted(k for k in T95 if k <= df)
    return T95[keys[-1]] if keys else 12.71


def cmd_stats(a) -> int:
    rows = [l.split("\t") for l in Path(a.tsv).read_text().splitlines()[1:]]
    col = {"ttft_us": 2, "event_rate": 5}[a.field]
    by: dict[str, dict[int, float]] = {}
    for r in rows:
        by.setdefault(r[1], {})[int(r[0])] = float(r[col])
    base, arm = by[a.base], by[a.arm]
    iters = sorted(set(base) & set(arm))
    b = [base[i] for i in iters]
    x = [arm[i] for i in iters]
    if a.field == "event_rate":
        d = [(xi / bi - 1) * 100 for xi, bi in zip(x, b)]
        unit = "%"
    else:
        d = [(xi - bi) / 1000 for xi, bi in zip(x, b)]
        unit = "ms"
    m = statistics.mean(d)
    sd = statistics.stdev(d) if len(d) > 1 else 0.0
    half = t95(len(d) - 1) * sd / math.sqrt(len(d)) if len(d) > 1 else float("nan")
    srt = sorted(d)
    p95 = srt[round((len(srt) - 1) * 0.95)]
    print(f"stats field={a.field} n_pairs={len(d)} {a.base}_mean={statistics.mean(b):.3f} {a.arm}_mean={statistics.mean(x):.3f} "
          f"paired_delta_mean={m:+.3f}{unit} 95%CI=[{m - half:+.3f},{m + half:+.3f}]{unit} sd={sd:.3f} p95_delta={p95:+.3f}{unit} "
          f"per_pair={[round(v, 3) for v in d]}")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fidelity")
    f.add_argument("--direct", required=True)
    f.add_argument("--fwd", required=True)
    f.add_argument("--reps", type=int, default=2)
    f.add_argument("--max-tokens", type=int, default=160)
    f.add_argument("--cooldown", type=float, default=2.0)
    f.add_argument("--out", required=True)
    r = sub.add_parser("recheck", help="re-run the fidelity comparison on saved .sse files")
    r.add_argument("--dir", required=True)
    c = sub.add_parser("cancel")
    c.add_argument("--fwd", required=True)
    c.add_argument("--log", required=True)
    c.add_argument("--reps", type=int, default=3)
    c.add_argument("--wait", type=float, default=6.0)
    c.add_argument("--words", type=int, default=9000)
    c.add_argument("--timeout", type=float, default=30.0)
    c.add_argument("--cooldown", type=float, default=10.0)
    s = sub.add_parser("stats")
    s.add_argument("--tsv", required=True)
    s.add_argument("--base", required=True)
    s.add_argument("--arm", required=True)
    s.add_argument("--field", choices=("event_rate", "ttft_us"), required=True)
    a = p.parse_args()
    return {"fidelity": cmd_fidelity, "recheck": cmd_recheck, "cancel": cmd_cancel, "stats": cmd_stats}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
