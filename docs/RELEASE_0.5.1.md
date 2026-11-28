# Slipstream 0.5.1 — real agents start new conversations in under two seconds

0.5.0 showed that a new conversation can reuse the agent's setup. With the request shape real
coding agents send (OpenCode, Kilo Code: a system prompt with the date and working directory, a
list of tool definitions, files arriving as tool results), one short side request — like the
title an agent asks for — made the server throw that saved setup away. 0.5.1 keeps it, moves the
engine to the newest llama.cpp, and fixes forced tool calls.

All numbers: Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF and PGRN on the internal NVMe,
llama-server with the app's flags, 4 GiB expert cache, swap growth 0 MB. Raw data in
`bench/RESULTS.md`.

## What changed for you

| | 0.5.0 | 0.5.1 |
|---|---:|---:|
| New conversation in a real agent (same system prompt + 12 tools), after a side request | 18.3 s | **1.6 s** |
| Decode speed at 10 GiB cache | 18.2 tok/s | **19.1 tok/s** (+4.7 %) |
| Decode speed at 4 GiB cache | 10.5 tok/s | 10.5 tok/s |

Answers are identical to before (same output hashes in every check).

- **The agent's setup survives side requests.** The server keeps recent prompts in memory. Its
  rule was never to load a big saved prompt when only a small part of it fits, because loading
  used to remove it from memory. Slipstream now loads a copy and keeps the original, so a new
  conversation only reads its own words (26 tokens instead of 2,745). Known cost: on the day the
  date in the agent's system prompt changes, the first new conversation takes ~22 s instead of
  ~12 s. On Macs with less than 32 GB the prompt memory (1 GB) holds only one long context, so
  this helps less there.
- **Newest llama.cpp.** The engine now builds on upstream llama.cpp from 27 Nov 2026
  (`8e23278c7c36`, 106 commits newer). One new upstream Metal code path made decoding 3–5 %
  slower here; it is switched off (`GGML_METAL_CHUNK_COMMIT=1` turns it back on).
- **Forced tool calls work again.** When an app asks for one specific tool
  (`tool_choice: {"type":"function", ...}`), the model calls exactly that tool. The new upstream
  code had lost this for Qwen3.6.

## Validation

VALIDATION_PLACEHOLDER
