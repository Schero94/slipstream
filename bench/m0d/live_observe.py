"""M0d live in-engine expert-cache observation (35B, guard-approved).

Runs the REAL llama-server fork with `PGR_STREAM_EXPERTS` so `pgr_observe`
(src/peregrine_observe.c) feeds every runtime expert selection through the
native cloxcache at a bounded capacity. Reports the LIVE in-engine hit-rate
— the number the offline replay (95.3–95.6% at 10 GB) predicted — plus a
steady-state tail rate that excludes the cold-start fill, and decode tok/s.

Observation only: compute is unchanged, output unaffected. Zero M0a tokens.
"""
import re
import subprocess
import time
import uuid
from pathlib import Path

from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    routing_environment,
    _unused_port,
    _json_request,
    _wait_for_health,
)
from bench.m0a.constants import INT4_EXPERT_BYTES

GGUF = Path("/Users/schero/.cache/peregrine/models/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf")
SHA = "55983c5a75a1ab969824077b3bb3de4146e82a9234072b48ad4e8f92ad3fe9f1"
manifest = {"sha256": SHA, "geometry": {"layers": 40, "experts": 256, "top_k": 8}}

CACHE_BYTES = 10e9
CAP = int(CACHE_BYTES / INT4_EXPERT_BYTES)  # same 10-GB budget as the offline replays

scratch = Path(__file__).parent
routing_path = scratch / "live-observe-routing.bin"
stderr_path = scratch / "live-observe-server.log"
if routing_path.exists():
    routing_path.unlink()

session = uuid.uuid4()
port = _unused_port()

cmd = [
    str(DEFAULT_SERVER), "--model", str(GGUF), "--host", "127.0.0.1", "--port", str(port),
    "--parallel", "1", "--ctx-size", "16384", "--fit", "off", "--gpu-layers", "99",
    "--no-warmup", "--alias", "peregrine-m0",
]
env = routing_environment(manifest, routing_path, session)
env["PGR_STREAM_EXPERTS"] = "1"
env["PGR_STREAM_CAP"] = str(CAP)
env["PGR_STREAM_K"] = "4"

PROMPTS = [
    "Write a Python function `merge_intervals(intervals)` that merges overlapping "
    "[start, end] intervals and returns them sorted. Include three doctest examples "
    "and handle empty input.",
    "Review this function and fix the bug, then add a short unit test:\n\n"
    "def rolling_mean(xs, w):\n    out = []\n    for i in range(len(xs)):\n"
    "        window = xs[i:i+w]\n        out.append(sum(window)/w)\n    return out\n\n"
    "The last windows are shorter than w, so the mean is wrong.",
    "Implement an LRU cache class in Python with get/put in O(1) using a doubly "
    "linked list plus dict (no functools). Then explain the eviction order in two sentences.",
]

line_re = re.compile(
    r"\[peregrine\] live expert-cache: ([0-9.]+)% hit \((\d+)/(\d+)\), cap=(\d+)"
)

print(f"cap={CAP} experts (= {CACHE_BYTES/1e9:.0f} GB at {INT4_EXPERT_BYTES} B/expert), k=4")
stderr_file = stderr_path.open("wb")
proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=stderr_file)
try:
    _wait_for_health(proc, port)
    url = f"http://127.0.0.1:{port}/v1/chat/completions"
    for i, prompt in enumerate(PROMPTS, 1):
        body = {
            "model": "peregrine-m0",
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0,
            "max_tokens": 700,
            "chat_template_kwargs": {"enable_thinking": False},
        }
        t0 = time.monotonic()
        resp = _json_request(url, body, timeout=1800)
        wall = time.monotonic() - t0
        timings = resp.get("timings", {})
        usage = resp.get("usage", {})
        print(
            f"request {i}: {usage.get('completion_tokens')} tokens, "
            f"{timings.get('predicted_per_second'):.2f} tok/s decode, wall {wall:.1f}s"
        )
finally:
    proc.terminate()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
    stderr_file.close()

samples = []
for line in stderr_path.read_text(errors="replace").splitlines():
    m = line_re.search(line)
    if m:
        samples.append((float(m.group(1)), int(m.group(2)), int(m.group(3)), int(m.group(4))))

if not samples:
    raise SystemExit("no [peregrine] live expert-cache lines captured — observer inactive?")

pct, hits, acc, cap = samples[-1]
print(f"\nLIVE in-engine cache (cumulative, incl. cold start + prefill):")
print(f"  {pct:.2f}% hit ({hits}/{acc}), cap={cap} experts, {len(samples)} samples")

# steady-state tail: rate over the second half of accesses (cold fill excluded)
mid = next((s for s in samples if s[2] >= acc / 2), samples[0])
if acc > mid[2]:
    tail_rate = 100.0 * (hits - mid[1]) / (acc - mid[2])
    print(f"  steady-state tail (last {acc - mid[2]} accesses): {tail_rate:.2f}% hit")
print(f"ROUTING_TRACE_BYTES {routing_path.stat().st_size}")
