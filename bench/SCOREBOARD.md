# Scoreboard — what users feel

Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF + PGRN on the internal NVMe, app regime
(llama-server with the app's flags) unless noted. Update a row only from a measured,
recorded result in `bench/RESULTS.md`.

| # | Metric | Target | Current | Since | Source |
|---|---|---|---|---|---|
| 1a | TTFT cold first turn, 39.5k-token agent prompt (app regime, 4 GiB) | ≤ 150 s | 183–190 s default; 137 s with Fast prompt reading (opt-in, chat API); dev engine with last-layer crop 131.5 s (E4 S1; scaling 39k 132 → 127.3 s, bitwise at real shapes since the 64-row padding); was 235 s; ~37 % is attention (compute floor) | v0.5 dev | RESULTS E4 + scaling + last-layer crop (+ padding follow-up) 2026-11-28 |
| 1b | TTFT follow-up agent turn (append-only) after an interleaved request | < 3 s | **0.8 s** (was 229 s) | v0.4.1 | RESULTS E1 2026-11-26 |
| 1c | TTFT new agent conversation, same system prompt + tools (app regime, 4 GiB) | < 5 s | **2.1 s** (was 190 s) | v0.5 dev | RESULTS E4 2026-11-27 |
| 1d | TTFT first agent request after an app/server restart (app regime, 4 GiB) | < 5 s | **2.2 s** (was ~187 s) | v0.5 dev | RESULTS E5 2026-11-28 |
| 2 | Decode, lossless default, 4 / 10 GiB | ≥ 12 / ≥ 20 tok/s | 10.3 / 17.5 | v0.4.0 | RESULTS 2026-11-25 |
| 3 | Decode, Turbo, 4 / 10 GiB | ≥ 16 / ≥ 22 tok/s | 14.5 / 19.6 | v0.4.0 | RESULTS 2026-11-25 |
| 4 | Turbo quality (decode KLD / same top-1) | ≤ 0.02 / ≥ 93 % | 0.020 / 92.5 % | v0.4.0 | RESULTS 2026-11-25 |
| 5 | Lossless parity (e2e nmse, output hash) | exact | exact | v0.4.0 | ctest 14/14 |
| 6 | First run: clicks until model runs and editor is connected | ≤ 4 | **3** (was 5) | v0.5 dev | app/ui-review/JOURNEY.md 2026-11-28 |
| 7 | Tests green (engine / Rust / Tauri / Python / UI) | all | 14 / 183 / 96 / 44 / 7 | v0.4.1 | RELEASE_0.4.1 |
| 8 | Swap growth during any benchmark | 0 MB | 0 MB | v0.4.0 | RESULTS |
| 9 | Release completeness (dmg + zip + node, verified) | complete | complete | v0.4.1 | RELEASE_0.4.1 |
| 10 | llama.cpp distance to upstream | < 1 month | 0 days at release | v0.4.0 | PORT notes |
