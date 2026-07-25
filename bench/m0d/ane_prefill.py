"""ANE hybrid-prefill block — the dense half of the hybrid path, on the Neural Engine.

The measured ANE win is in prefill: dense fp16 matmuls at large sequence length. This
module builds the ACTUAL dense prefill block (QKV projections -> scaled-dot-product
attention -> output projection -> residual -> FFN) as a CoreML model with FIXED weights,
runs it on CPU_ONLY vs CPU_AND_NE, and verifies the result against a numpy reference — so
we prove the ANE produces the CORRECT output at the flagship's dense width, not just that
it is fast. This is the reusable dense-prefill kernel a hybrid ANE+GPU runtime would call
(ANE does the dense prefill compute while Metal keeps decoding).

Honest scope: this is the dense block only — the MoE expert GEMMs stay bandwidth-bound on
the GPU, and RMSNorm is omitted from the MIL block for simplicity (it is a small,
elementwise op). coremltools loads only under the py3.11 probe venv, so the CoreML run is
skipped under the project's 3.14 venv; the numpy reference is always available.
"""

from __future__ import annotations

from typing import Any

import numpy as np


class AnePrefillError(Exception):
    """Raised for invalid inputs."""


def _gelu(x: np.ndarray) -> np.ndarray:
    return 0.5 * x * (1.0 + np.tanh(0.7978845608028654 * (x + 0.044715 * x**3)))


def make_weights(hidden: int, ff: int, *, seed: int = 0) -> dict[str, np.ndarray]:
    if hidden % 8 != 0:
        raise AnePrefillError("hidden must be a multiple of 8 (ANE fp16 alignment)")
    rng = np.random.default_rng(seed)
    s = 1.0 / np.sqrt(hidden)
    return {
        "wq": (rng.standard_normal((hidden, hidden)) * s).astype(np.float32),
        "wk": (rng.standard_normal((hidden, hidden)) * s).astype(np.float32),
        "wv": (rng.standard_normal((hidden, hidden)) * s).astype(np.float32),
        "wo": (rng.standard_normal((hidden, hidden)) * s).astype(np.float32),
        "w1": (rng.standard_normal((hidden, ff)) * s).astype(np.float32),
        "w2": (rng.standard_normal((ff, hidden)) * s).astype(np.float32),
    }


def dense_prefill_ref(x: np.ndarray, w: dict[str, np.ndarray], *, n_heads: int) -> np.ndarray:
    """numpy reference for one dense prefill block. x is (seq, hidden). Non-causal MHA."""
    seq, hidden = x.shape
    if hidden % n_heads != 0:
        raise AnePrefillError("hidden must divide by n_heads")
    hd = hidden // n_heads
    scale = 1.0 / np.sqrt(hd)

    def heads(t):
        return t.reshape(seq, n_heads, hd).transpose(1, 0, 2)  # (H, seq, hd)

    q, k, v = heads(x @ w["wq"]), heads(x @ w["wk"]), heads(x @ w["wv"])
    scores = (q @ k.transpose(0, 2, 1)) * scale                # (H, seq, seq)
    scores = scores - scores.max(-1, keepdims=True)
    attn = np.exp(scores)
    attn = attn / attn.sum(-1, keepdims=True)
    ctx = attn @ v                                             # (H, seq, hd)
    ctx = ctx.transpose(1, 0, 2).reshape(seq, hidden)
    x2 = x + ctx @ w["wo"]
    return x2 + _gelu(x2 @ w["w1"]) @ w["w2"]


def build_prefill_coreml(hidden: int, ff: int, seq: int, n_heads: int, w: dict[str, np.ndarray]):
    import coremltools as ct
    from coremltools.converters.mil import Builder as mb

    hd = hidden // n_heads
    scale = float(1.0 / np.sqrt(hd))
    wq, wk, wv, wo, w1, w2 = (w[k] for k in ("wq", "wk", "wv", "wo", "w1", "w2"))

    @mb.program(input_specs=[mb.TensorSpec(shape=(1, seq, hidden))])
    def prog(x):
        x2d = mb.reshape(x=x, shape=(seq, hidden))

        def heads(t):
            t = mb.reshape(x=t, shape=(seq, n_heads, hd))
            return mb.transpose(x=t, perm=[1, 0, 2])           # (H, seq, hd)

        q = heads(mb.matmul(x=x2d, y=wq))
        k = heads(mb.matmul(x=x2d, y=wk))
        v = heads(mb.matmul(x=x2d, y=wv))
        scores = mb.matmul(x=q, y=k, transpose_y=True)         # (H, seq, seq)
        scores = mb.mul(x=scores, y=scale)
        attn = mb.softmax(x=scores, axis=-1)
        ctx = mb.matmul(x=attn, y=v)                           # (H, seq, hd)
        ctx = mb.transpose(x=ctx, perm=[1, 0, 2])
        ctx = mb.reshape(x=ctx, shape=(seq, hidden))
        x2 = mb.add(x=x2d, y=mb.matmul(x=ctx, y=wo))
        hddn = mb.gelu(x=mb.matmul(x=x2, y=w1))
        out = mb.add(x=x2, y=mb.matmul(x=hddn, y=w2))
        return mb.reshape(x=out, shape=(1, seq, hidden))

    return ct.convert(prog, compute_precision=ct.precision.FLOAT16,
                      minimum_deployment_target=ct.target.macOS13)


def run_ane_prefill(*, hidden: int = 2048, ff: int = 2048, seq: int = 512,
                    n_heads: int = 16, iters: int = 10, seed: int = 0) -> dict[str, Any]:
    """Build the block, verify ANE output vs numpy ref, and measure CPU vs ANE latency."""
    import tempfile
    import time

    import coremltools as ct

    w = make_weights(hidden, ff, seed=seed)
    rng = np.random.default_rng(seed + 1)
    x = (rng.standard_normal((seq, hidden)) * 0.1).astype(np.float32)
    ref = dense_prefill_ref(x, w, n_heads=n_heads)
    sample = {"x": x.reshape(1, seq, hidden)}

    model = build_prefill_coreml(hidden, ff, seq, n_heads, w)
    with tempfile.TemporaryDirectory() as d:
        pkg = f"{d}/prefill.mlpackage"
        model.save(pkg)

        def load(unit):
            return ct.models.MLModel(pkg, compute_units=unit)

        def out_of(m):
            name = m.get_spec().description.output[0].name
            return np.asarray(m.predict(sample)[name]).reshape(seq, hidden)

        def timed(m):
            m.predict(sample)  # warmup / compile onto target
            best = float("inf")
            for _ in range(iters):
                t = time.monotonic()
                m.predict(sample)
                best = min(best, (time.monotonic() - t) * 1000.0)
            return best

        cpu = load(ct.ComputeUnit.CPU_ONLY)
        ane = load(ct.ComputeUnit.CPU_AND_NE)
        ane_out = out_of(ane)
        cpu_ms = timed(cpu)
        ane_ms = timed(ane)

    def cosine(a, b):
        a, b = a.reshape(-1), b.reshape(-1)
        return float(a @ b / (np.linalg.norm(a) * np.linalg.norm(b)))

    return {
        "hidden": hidden, "ff": ff, "seq": seq, "n_heads": n_heads,
        "cpu_ms": round(cpu_ms, 4), "ane_ms": round(ane_ms, 4),
        "speedup": round(cpu_ms / ane_ms, 3) if ane_ms else 0.0,
        "cosine_ane_vs_ref": round(cosine(ane_out, ref), 6),
        "max_abs_err_ane_vs_ref": float(np.max(np.abs(ane_out - ref))),
    }


def main(argv: list[str] | None = None) -> int:
    import argparse
    import json

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--hidden", type=int, default=2048)
    p.add_argument("--ff", type=int, default=2048)
    p.add_argument("--seq", type=int, default=512)
    p.add_argument("--n-heads", type=int, default=16)
    p.add_argument("--iters", type=int, default=10)
    args = p.parse_args(argv)
    print(json.dumps(run_ane_prefill(hidden=args.hidden, ff=args.ff, seq=args.seq,
                                     n_heads=args.n_heads, iters=args.iters), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
