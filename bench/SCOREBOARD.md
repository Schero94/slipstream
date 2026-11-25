# Scoreboard — what users feel

Qwen3.6-35B-A3B Q4_K_XL, M3 Pro / 36 GB, GGUF + PGRN on the internal NVMe, app regime
(llama-server with the app's flags) unless noted. Update a row only from a measured,
recorded result in `bench/RESULTS.md`.

| # | Metric | Target | Current | Since | Source |
|---|---|---|---|---|---|
| 1 | Time to first token, ~35k-token agent prompt, 10 GiB cache | < 30 s | measuring (est. ~150 s) | — | v0.5 baseline pending |
| 2 | Decode, lossless default, 4 / 10 GiB | ≥ 12 / ≥ 20 tok/s | 10.3 / 17.5 | v0.4.0 | RESULTS 2026-11-25 |
| 3 | Decode, Turbo, 4 / 10 GiB | ≥ 16 / ≥ 22 tok/s | 14.5 / 19.6 | v0.4.0 | RESULTS 2026-11-25 |
| 4 | Turbo quality (decode KLD / same top-1) | ≤ 0.02 / ≥ 93 % | 0.020 / 92.5 % | v0.4.0 | RESULTS 2026-11-25 |
| 5 | Lossless parity (e2e nmse, output hash) | exact | exact | v0.4.0 | ctest 14/14 |
| 6 | First run: clicks until model runs and editor is connected | ≤ 4 | 4 | v0.4.0 | app/ui-review/JOURNEY.md |
| 7 | Tests green (engine / Rust / Tauri / Python / UI) | all | 14 / 183 / 96 / 44 / 7 | v0.4.0 | RELEASE_0.4.0 |
| 8 | Swap growth during any benchmark | 0 MB | 0 MB | v0.4.0 | RESULTS |
| 9 | Release completeness (dmg + zip + node, verified) | complete | complete | v0.4.0 | RELEASE_0.4.0 |
| 10 | llama.cpp distance to upstream | < 1 month | 0 days at release | v0.4.0 | PORT notes |
