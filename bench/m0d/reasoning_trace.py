"""M0d reasoning smoke on the small model (Qwen3.6-35B) — thinking ON, phase-split hit-rate."""
import subprocess, time, uuid, re
from pathlib import Path
from bench.m0a.smoke_server import DEFAULT_SERVER, routing_environment, _unused_port, _json_request, _wait_for_health
from bench.m0a.routing_format import iter_records, PHASE_DECODE
from bench.m0a.constants import DEV_LAYERS, INT4_EXPERT_BYTES
from bench.m0d.streaming_store import StreamingStore
from bench.m0d.streaming_replay import replay_streaming

GGUF = Path("/Users/schero/.cache/peregrine/models/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf")
SHA = "55983c5a75a1ab969824077b3bb3de4146e82a9234072b48ad4e8f92ad3fe9f1"
manifest = {"sha256": SHA, "geometry": {"layers": 40, "experts": 256, "top_k": 8}}
scratch = Path(__file__).parent
routing_path = scratch / "reasoning-routing.bin"
if routing_path.exists(): routing_path.unlink()
session = uuid.uuid4()
port = _unused_port()

cmd = [str(DEFAULT_SERVER), "--model", str(GGUF), "--host", "127.0.0.1", "--port", str(port),
       "--parallel", "1", "--ctx-size", "16384", "--fit", "off", "--gpu-layers", "99",
       "--no-warmup", "--alias", "peregrine-m0"]
env = routing_environment(manifest, routing_path, session)

prompt = ("You are fixing a bug. This function should return the number of unique "
          "elements that appear exactly once, but it's wrong:\n\n"
          "def count_singletons(xs):\n    c = {}\n    for x in xs:\n        c[x] = c.get(x,0)+1\n"
          "    return len([k for k in c if c[k] >= 1])\n\n"
          "Reason step by step about what's wrong, then give the corrected function.")

proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    _wait_for_health(proc, port)
    url = f"http://127.0.0.1:{port}/v1/chat/completions"
    body = {"model": "peregrine-m0", "messages": [{"role": "user", "content": prompt}],
            "temperature": 0, "max_tokens": 3000,
            "chat_template_kwargs": {"enable_thinking": True}}
    t0 = time.monotonic()
    resp = _json_request(url, body, timeout=1800)
    wall = time.monotonic() - t0
    msg = resp["choices"][0]["message"]
    reasoning = msg.get("reasoning_content") or ""
    content = msg.get("content") or ""
    if not reasoning:
        m = re.search(r"<think>(.*?)</think>", content, re.DOTALL)
        if m: reasoning = m.group(1)
    timings = resp.get("timings", {})
    usage = resp.get("usage", {})
    tps = timings.get("predicted_per_second")
    # tokenize reasoning to get think-token count
    n_think = 0
    if reasoning:
        tk = _json_request(f"http://127.0.0.1:{port}/tokenize", {"content": reasoning}, timeout=120)
        n_think = len(tk.get("tokens", []))
    print(f"REASONING: wall {wall:.1f}s, decode {tps} tok/s, completion_tokens {usage.get('completion_tokens')}")
    print(f"  reasoning chars {len(reasoning)}, think-tokens ~{n_think}")
    print(f"  answer sample: {content[:200].strip()!r}")
finally:
    proc.terminate()
    try: proc.wait(timeout=30)
    except subprocess.TimeoutExpired: proc.kill()

# parse routing binary -> per decode-token per-layer experts
by_pos = {}
for rec in iter_records(routing_path):
    if rec.phase != PHASE_DECODE: continue
    by_pos.setdefault(rec.token_pos, {})[rec.layer] = list(rec.experts)
positions = sorted(by_pos)
tokens = []
for pos in positions:
    layers = by_pos[pos]
    if len(layers) != DEV_LAYERS: continue
    tokens.append([layers[L] for L in range(DEV_LAYERS)])
n_dec = len(tokens)
n_think = min(n_think, n_dec)
phases = ["think"] * n_think + ["answer"] * (n_dec - n_think)
print(f"  decode tokens in trace: {n_dec}  (think {n_think} / answer {n_dec-n_think})")

cap = int(10e9 / INT4_EXPERT_BYTES)
with StreamingStore(str(GGUF)) as store:
    r = replay_streaming(tokens, layer_count=DEV_LAYERS, store=store, capacity=cap,
                         prefetch_budget=16, compute_ms=30.0, holdout_frac=0.30, phases=phases)
print(f"\n35B THINKING-ON M0d, 10GB cache, held-out {r['held_out_tokens']} tok:")
print(f"  OVERALL held-out hit-rate: {r['hit_rate']*100:.2f}%  ({r['hits']}/{r['accesses']})")
for label, p in r["by_phase"].items():
    print(f"    {label:7s}: {p['hit_rate']*100:.2f}%  ({p['hits']}/{p['accesses']})")
print(f"  real SSD: {r['ssd_gb_s_heldout']:.2f} GB/s, {r['ssd_ms_per_token']:.2f} ms/tok -> proj {r['projected_tok_s']} tok/s")
print(f"ROUTING_TRACE_BYTES {routing_path.stat().st_size}")
