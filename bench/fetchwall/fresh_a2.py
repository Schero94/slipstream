# Fresh control for E1: build the same A2 prompt (A + A's 48-token answer + new user turn) and
# send only A2 to a fresh server, 64 tokens; prints its output hash.
import json, sys, urllib.request, hashlib
pf, port = sys.argv[1], sys.argv[2]
A = open(pf).read()
def req(p, n):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/completion", data=json.dumps(
        {"prompt": p, "n_predict": n, "temperature": 0, "seed": 1, "cache_prompt": False}).encode(),
        headers={"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(r, timeout=3600))
ra = req(A, 48)
A2 = A + ra["content"] + "<|im_end|>\n<|im_start|>user\nGood. Now show the exact diff for the first file.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
open(sys.argv[3], "w").write(A2)
print("A2 prompt written")
