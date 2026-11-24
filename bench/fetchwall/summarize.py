#!/usr/bin/env python3
"""Summarize fetch-wall arm logs: per (label, cache) decode tok/s mean/min/max, output-hash parity."""
import glob, hashlib, os, re, statistics, sys
from collections import defaultdict
d = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "results")
since = sys.argv[2] if len(sys.argv) > 2 else ""
rows = defaultdict(list)
for log in sorted(glob.glob(os.path.join(d, "*-c*.log"))):
    name = os.path.basename(log)
    if name < since or "-kld-" in name: continue
    m = re.match(r"(\d{8}-\d{6})-(.+)-c(\d+)\.log$", name)
    if not m: continue
    txt = open(log, errors="replace").read()
    ev = re.search(r"common_perf_print: +eval time .*?([0-9.]+) tokens per second", txt)
    if not ev: continue
    hit = re.findall(r"PGRN cache = .*?\(([0-9.]+)%\)", txt)
    rec = re.findall(r"lookahead recall = ([0-9.]+)%", txt)
    h = hashlib.sha256(open(log + ".out", "rb").read()).hexdigest()[:12] if os.path.exists(log + ".out") else "NA"
    rows[(int(m.group(3)), m.group(2))].append((float(ev.group(1)), hit[-1] if hit else "NA", rec[-1] if rec else "NA", h))
base = {c: v for (c, l), v in rows.items() if l == "base"}
print(f"{'cache':>5} {'arm':<8} {'n':>2} {'mean':>6} {'min':>6} {'max':>6} {'vs base':>8}  hit   recall  hashes")
for (c, l) in sorted(rows):
    v = rows[(c, l)]; t = [x[0] for x in v]
    b = statistics.mean(x[0] for x in base[c]) if c in base else None
    rel = f"{100*(statistics.mean(t)/b-1):+.1f}%" if b else "-"
    print(f"{c:>5} {l:<8} {len(t):>2} {statistics.mean(t):6.2f} {min(t):6.2f} {max(t):6.2f} {rel:>8}  {v[-1][1]:>5} {v[-1][2]:>6}  {','.join(sorted(set(x[3] for x in v)))}")
