# Slipstream 0.5.2 — a faster first answer, and a smarter cache on smaller Macs

Two engine improvements that change nothing about the answers: the first long prompt is read
about 5 seconds faster, and Macs that can only give the model a small memory cache keep the
right parts of the model in it more often. Plus screenshots of the app in the README.

All numbers: Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF and PGRN on the internal NVMe,
llama-server with the app's flags (no MTP for streamed runs), swap growth 0 MB. Raw data in
`bench/RESULTS.md`.

## What changed for you

| | 0.5.1 | 0.5.2 |
|---|---:|---:|
| First long agent prompt (39k tokens, 4 GiB cache, app chat path) | 136 s | **131 s** |
| Writing speed, 4 GiB cache | 9.7 tok/s | **9.9 tok/s** (+2 %) |
| Writing speed, 6 GiB cache | 12.1 tok/s | **12.6 tok/s** (+3.5 %) |
| Writing speed, 10–14 GiB cache | unchanged | unchanged |

Answers are bit-for-bit the same as before.

- **Faster first answer.** After reading a prompt, only the last word's result is needed to
  start answering. The model's last layer now skips the attention work for all the other words
  (it still stores what later turns need). That saves about 5 seconds on a 39k-token prompt. A
  reviewer found that the first version changed the result in the last decimal places on real
  prompt sizes; the shipped version keeps the same computation kernel and was checked
  bit-for-bit on 248,320 output values.
- **A smarter cache when memory is tight.** When the model's expert cache is small (up to about
  8 GiB for the 35B model), Slipstream now keeps the experts that are used most often and stops
  the look-ahead loader from throwing out experts it has just fetched. That means fewer SSD reads
  and 2–3.5 % faster writing on 16–24 GB Macs. With bigger caches it makes no difference, so it
  only switches on below that size.
- **Screenshots** of the app in the README and on the website.

## Under the hood

- Our benchmark script measured with MTP speculation on, which the app does not use for
  streamed models; it now matches the app. Earlier rows are marked in `bench/RESULTS.md`.
- Tried and dropped: freeing the look-ahead loader from old pins ("kick-free", no gain without
  MTP).

## Validation

VALIDATION_PLACEHOLDER
