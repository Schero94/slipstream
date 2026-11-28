#!/usr/bin/env python3
"""v0.6 single-Mac qualification checks (bench/forward/run_v06_qual.sh): a real llama-server (big-Mac role)
behind `slipstream-node share`, and `slipstream-node use --token-file` (MacBook role) on the same Mac.

  v06_qual.py --token-file F fidelity   --direct A --fwd B --reps 2 --out DIR
  v06_qual.py --token-file F longprompt --direct A --fwd B --prompt bench/fetchwall/prompt_30k.txt --out DIR
  v06_qual.py --token-file F cancel     --fwd B --log llama.log --reps 3
  v06_qual.py --token-file F sigstop    --direct A --fwd B --share-pid PID --log llama.log
  v06_qual.py --token-file F perf       --direct A --fwd B --n-ttft 20 --n-decode 6 --out TSV

Every request carries `Authorization: Bearer <token>` (the forwarder requires it, llama-server ignores it).
Nothing here prints the token.
"""
from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import signal
import statistics
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import m1_real  # noqa: E402

TOKEN = ""


def headers(extra: dict | None = None) -> dict:
    h = {"Content-Type": "application/json", "Accept": "text/event-stream"}
    if TOKEN:
        h["Authorization"] = f"Bearer {TOKEN}"
    h.update(extra or {})
    return h


def stream(addr: str, path: str, body: dict, timeout: float = 900.0, on_event=None):
    """POST and read the SSE body line by line. Returns dict with status, t0, events [(t, obj|str)], raw tail."""
    host, port = addr.rsplit(":", 1)
    t0 = time.perf_counter()
    conn = http.client.HTTPConnection(host, int(port), timeout=timeout)
    conn.request("POST", path, body=json.dumps(body).encode(), headers=headers())
    resp = conn.getresponse()
    out = {"status": resp.status, "t0": t0, "t_head": time.perf_counter() - t0, "events": [], "error": None}
    if resp.status != 200:
        out["body"] = resp.read().decode(errors="replace")
        conn.close()
        return out
    try:
        while True:
            line = resp.readline()
            if not line:
                break
            line = line.decode(errors="replace").strip()
            if not line.startswith("data:"):
                continue
            data = line[5:].strip()
            t = time.perf_counter() - t0
            if data == "[DONE]":
                out["events"].append((t, "[DONE]"))
                continue
            try:
                obj = json.loads(data)
            except json.JSONDecodeError:
                obj = data
            out["events"].append((t, obj))
            if on_event:
                on_event(t, obj)
    except Exception as e:  # noqa: BLE001 - record how the stream ended
        out["error"] = repr(e)
    out["t_end"] = time.perf_counter() - t0
    conn.close()
    return out


def get(addr: str, path: str, timeout: float = 5.0):
    host, port = addr.rsplit(":", 1)
    conn = http.client.HTTPConnection(host, int(port), timeout=timeout)
    conn.request("GET", path, headers=headers({"Accept": "application/json"}))
    r = conn.getresponse()
    body = r.read().decode(errors="replace")
    conn.close()
    return r.status, body


def content_times(ev):
    ts = []
    for t, o in ev:
        if isinstance(o, dict):
            ch = o.get("choices") or []
            if ch:
                c = ch[0]
                txt = (c.get("delta") or {}).get("content") or c.get("text") or ""
                if txt:
                    ts.append(t)
    return ts


# ---------------------------------------------------------------- fidelity (a) + (b)
def cmd_fidelity(a) -> int:
    # (b): an automatic tool call next to M1's forced (named) tool_choice case.
    m1_real.CASES["tool_auto"] = {
        "messages": [{"role": "user", "content": "List the files in the directory src. Use the tool."}],
        "tools": [{"type": "function", "function": {"name": "list_dir", "description": "List a directory",
                   "parameters": {"type": "object", "required": ["path"], "properties": {"path": {"type": "string"}}}}}],
        "tool_choice": "auto",
    }
    m1_real.CASES["tool_required"] = {**m1_real.CASES["tool_auto"], "tool_choice": "required"}
    return m1_real.cmd_fidelity(a)


# ---------------------------------------------------------------- long agent prompt (c)
def cmd_longprompt(a) -> int:
    prompt = Path(a.prompt).read_text()
    body = {"model": "slipstream", "prompt": prompt, "stream": True, "max_tokens": a.max_tokens, "temperature": 0,
            "seed": 1, "cache_prompt": False, "return_progress": True}
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    results = {}
    for arm, addr in (("direct", a.direct), ("fwd", a.fwd)):
        slots: list = []
        stop = threading.Event()
        t_start = time.perf_counter()

        def poll():
            while not stop.is_set():
                try:
                    st, b = get(a.fwd, "/slots")
                    t = time.perf_counter() - t_start
                    if st == 200:
                        for s in json.loads(b):
                            pp = s.get("prompt_progress") or {}
                            if s.get("is_processing"):
                                slots.append((round(t, 2), pp.get("processed"), pp.get("total"), s.get("phase")))
                    else:
                        slots.append((round(t, 2), "status", st, None))
                except Exception as e:  # noqa: BLE001
                    slots.append((round(time.perf_counter() - t_start, 2), "err", repr(e)[:80], None))
                stop.wait(a.poll)

        th = threading.Thread(target=poll, daemon=True)
        if arm == "fwd":
            th.start()
        r = stream(addr, "/v1/completions", body)
        stop.set()
        prog = [(round(t, 2), o["prompt_progress"].get("processed"), o["prompt_progress"].get("total"))
                for t, o in r["events"] if isinstance(o, dict) and o.get("prompt_progress")]
        ct = content_times(r["events"])
        text = "".join(((o.get("choices") or [{}])[0].get("text") or "") for _, o in r["events"] if isinstance(o, dict))
        timings = next((o.get("timings") for _, o in reversed(r["events"]) if isinstance(o, dict) and o.get("timings")), {})
        results[arm] = {"status": r["status"], "first_token_s": round(ct[0], 2) if ct else None, "n_progress_events": len(prog),
                        "progress_first": prog[:2], "progress_last": prog[-2:], "slots_samples": len(slots),
                        "slots_first": slots[:3], "slots_last": slots[-3:], "text": text, "error": r["error"],
                        "prompt_n": timings.get("prompt_n"), "prompt_ms": timings.get("prompt_ms")}
        (out / f"longprompt-{arm}.json").write_text(json.dumps({**results[arm], "progress": prog, "slots": slots}, indent=1))
        print(f"longprompt arm={arm} status={r['status']} prompt_n={timings.get('prompt_n')} prompt_ms={timings.get('prompt_ms')} "
              f"first_token_s={results[arm]['first_token_s']} sse_progress_events={len(prog)} first={prog[:2]} last={prog[-1:]} "
              f"slots_polls_processing={len(slots)} slots_first={slots[:2]} slots_last={slots[-1:]} text={json.dumps(text[:60])}")
        time.sleep(a.cooldown)
    same = results["direct"]["text"] == results["fwd"]["text"]
    ok = all(results[k]["status"] == 200 and results[k]["n_progress_events"] > 1 for k in results) and \
        len(results["fwd"]["slots_last"]) > 0 and same
    print(f"longprompt identical_text={same} ok={ok}")
    return 0 if ok else 1


# ---------------------------------------------------------------- cancel (d)
def cmd_cancel(a) -> int:
    return m1_real.cmd_cancel(a)


# ---------------------------------------------------------------- SIGSTOP share mid-stream (e)
def cmd_sigstop(a) -> int:
    body = {"model": "slipstream", "stream": True, "max_tokens": 400, "temperature": 0, "seed": 1, "ignore_eos": True,
            "messages": [{"role": "user", "content": "Write a long story about a lighthouse keeper."}]}
    state = {"n": 0, "t_stop": None}

    def on_event(t, o):
        state["n"] += 1
        if state["n"] == a.after_events and state["t_stop"] is None:
            os.kill(a.share_pid, signal.SIGSTOP)
            state["t_stop"] = t

    try:
        r = stream(a.fwd, "/v1/chat/completions", body, on_event=on_event)
    finally:
        if state["t_stop"] is not None and a.keep_stopped <= 0:
            os.kill(a.share_pid, signal.SIGCONT)
    ev = r["events"]
    lost = [o for _, o in ev if isinstance(o, dict) and "error" in o]
    detect = r["t_end"] - state["t_stop"] if state["t_stop"] is not None else None
    print(f"sigstop stopped_after_events={a.after_events} t_stop={state['t_stop']:.2f}s stream_end_after_stop={detect:.2f}s "
          f"events={len(ev)} last_events={[o if isinstance(o, str) else (o.get('error') or 'chunk') for _, o in ev[-2:]]} "
          f"error_event={json.dumps(lost[-1])[:240] if lost else None} reader_error={r['error']}")
    # While the share is still stopped: what does a new editor request get?
    t = time.perf_counter()
    host, port = a.fwd.rsplit(":", 1)
    conn = http.client.HTTPConnection(host, int(port), timeout=30)
    conn.request("POST", "/v1/chat/completions", body=json.dumps({**body, "max_tokens": 8}).encode(), headers=headers())
    rr = conn.getresponse()
    b = rr.read().decode(errors="replace")
    print(f"sigstop request_while_stopped status={rr.status} after={time.perf_counter() - t:.2f}s body={b[:200]}")
    conn.close()
    os.kill(a.share_pid, signal.SIGCONT)
    t_cont = time.perf_counter()
    ok_after = None
    for _ in range(300):
        try:
            st, _b = get(a.fwd, "/health", timeout=3)
            if st == 200:
                ok_after = time.perf_counter() - t_cont
                break
        except Exception:  # noqa: BLE001
            pass
        time.sleep(0.1)
    print(f"sigstop health_200_after_sigcont={ok_after if ok_after is None else round(ok_after, 2)}s")
    # Next request works and equals direct.
    nb = {**body, "max_tokens": 64}
    rf = stream(a.fwd, "/v1/chat/completions", nb)
    rd = stream(a.direct, "/v1/chat/completions", nb)
    tf = "".join(((o.get("choices") or [{}])[0].get("delta") or {}).get("content") or "" for _, o in rf["events"] if isinstance(o, dict))
    td = "".join(((o.get("choices") or [{}])[0].get("delta") or {}).get("content") or "" for _, o in rd["events"] if isinstance(o, dict))
    print(f"sigstop next_request status={rf['status']} identical_to_direct={tf == td} chars={len(tf)}")
    if a.log and Path(a.log).exists():
        tail = Path(a.log).read_text(errors="replace").splitlines()[-400:]
        hits = [l for l in tail if re.search(r"cancel|disconnect|stop processing|srv\s+stop|connection closed", l, re.I)]
        print("sigstop llama_log_hits: " + " | ".join(h.strip()[:160] for h in hits[-4:]))
    ok = detect is not None and detect <= 12 and lost and rr.status == 503 and ok_after is not None and rf["status"] == 200 and tf == td
    return 0 if ok else 1


# ---------------------------------------------------------------- overhead (f)
def cmd_perf(a) -> int:
    msgs = [{"role": "user", "content": "Write a Python module that parses a CSV of orders, groups them by customer, "
             "and prints a monthly revenue report with type hints and docstrings."}]
    ttft_body = {"model": "slipstream", "stream": True, "max_tokens": 4, "temperature": 0, "seed": 1, "cache_prompt": True, "messages": msgs}
    dec_body = {**ttft_body, "max_tokens": a.decode_tokens, "ignore_eos": True}
    rows = ["iter\tarm\tkind\ttfb_s\tfirst_token_s\tn_tokens\tevent_rate"]
    arms = (("direct", a.direct), ("fwd", a.fwd))
    for w in range(2):  # warm the prompt cache on both paths
        for _, addr in arms:
            stream(addr, "/v1/chat/completions", ttft_body)
    res = {"ttft": {"direct": [], "fwd": []}, "decode": {"direct": [], "fwd": []}}
    for i in range(a.n_ttft):
        order = arms if i % 2 == 0 else arms[::-1]
        for name, addr in order:
            r = stream(addr, "/v1/chat/completions", ttft_body)
            ct = content_times(r["events"])
            res["ttft"][name].append(ct[0] if ct else float("nan"))
            rows.append(f"{i}\t{name}\tttft\t{r['t_head']:.4f}\t{ct[0] if ct else 'nan'}\t{len(ct)}\t")
            time.sleep(a.pause)
    for i in range(a.n_decode):
        order = arms if i % 2 == 0 else arms[::-1]
        for name, addr in order:
            r = stream(addr, "/v1/chat/completions", dec_body)
            ct = content_times(r["events"])
            rate = (len(ct) - 1) / (ct[-1] - ct[0]) if len(ct) > 2 else float("nan")
            res["decode"][name].append(rate)
            rows.append(f"{i}\t{name}\tdecode\t{r['t_head']:.4f}\t{ct[0] if ct else 'nan'}\t{len(ct)}\t{rate:.4f}")
            print(f"perf decode iter={i} arm={name} tokens={len(ct)} rate={rate:.2f} tok/s", flush=True)
            time.sleep(a.cooldown)
    Path(a.out).write_text("\n".join(rows) + "\n")

    def summ(kind, unit_ms):
        d, f = res[kind]["direct"], res[kind]["fwd"]
        if unit_ms:
            pairs = [(y - x) * 1000 for x, y in zip(d, f)]
        else:
            pairs = [(y / x - 1) * 100 for x, y in zip(d, f)]
        m = statistics.mean(pairs)
        sd = statistics.stdev(pairs) if len(pairs) > 1 else 0.0
        half = m1_real.t95(len(pairs) - 1) * sd / len(pairs) ** 0.5 if len(pairs) > 1 else float("nan")
        conv = (lambda v: v * 1000) if unit_ms else (lambda v: v)
        srt = sorted(pairs)
        print(f"perf {kind} n_pairs={len(pairs)} direct_median={conv(statistics.median(d)):.2f} fwd_median={conv(statistics.median(f)):.2f} "
              f"direct_mean={conv(statistics.mean(d)):.2f} fwd_mean={conv(statistics.mean(f)):.2f} "
              f"paired_delta_mean={m:+.2f}{'ms' if unit_ms else '%'} 95%CI=[{m - half:+.2f},{m + half:+.2f}] sd={sd:.2f} "
              f"p95_delta={srt[round((len(srt) - 1) * 0.95)]:+.2f} per_pair={[round(v, 2) for v in pairs]}")

    summ("ttft", True)
    summ("decode", False)
    return 0


def main() -> int:
    global TOKEN
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--token-file")
    sub = p.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fidelity")
    f.add_argument("--direct", required=True)
    f.add_argument("--fwd", required=True)
    f.add_argument("--reps", type=int, default=2)
    f.add_argument("--max-tokens", type=int, default=160)
    f.add_argument("--cooldown", type=float, default=2.0)
    f.add_argument("--out", required=True)
    lp = sub.add_parser("longprompt")
    lp.add_argument("--direct", required=True)
    lp.add_argument("--fwd", required=True)
    lp.add_argument("--prompt", required=True)
    lp.add_argument("--max-tokens", type=int, default=32)
    lp.add_argument("--poll", type=float, default=2.0)
    lp.add_argument("--cooldown", type=float, default=25.0)
    lp.add_argument("--out", required=True)
    c = sub.add_parser("cancel")
    c.add_argument("--fwd", required=True)
    c.add_argument("--log", required=True)
    c.add_argument("--reps", type=int, default=3)
    c.add_argument("--wait", type=float, default=6.0)
    c.add_argument("--words", type=int, default=9000)
    c.add_argument("--timeout", type=float, default=30.0)
    c.add_argument("--cooldown", type=float, default=10.0)
    s = sub.add_parser("sigstop")
    s.add_argument("--direct", required=True)
    s.add_argument("--fwd", required=True)
    s.add_argument("--share-pid", type=int, required=True)
    s.add_argument("--after-events", type=int, default=20)
    s.add_argument("--keep-stopped", type=int, default=1)
    s.add_argument("--log")
    pf = sub.add_parser("perf")
    pf.add_argument("--direct", required=True)
    pf.add_argument("--fwd", required=True)
    pf.add_argument("--n-ttft", type=int, default=20)
    pf.add_argument("--n-decode", type=int, default=6)
    pf.add_argument("--decode-tokens", type=int, default=256)
    pf.add_argument("--pause", type=float, default=1.0)
    pf.add_argument("--cooldown", type=float, default=25.0)
    pf.add_argument("--out", required=True)
    a = p.parse_args()
    if a.token_file:
        TOKEN = Path(a.token_file).read_text().strip()
        m1_real.EXTRA_HEADERS = f"Authorization: Bearer {TOKEN}\r\n"
    return {"fidelity": cmd_fidelity, "longprompt": cmd_longprompt, "cancel": cmd_cancel, "sigstop": cmd_sigstop,
            "perf": cmd_perf}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
