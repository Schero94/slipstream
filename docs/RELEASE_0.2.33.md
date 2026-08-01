# Slipstream 0.2.33

**Canonical product repo.** Releases ship here (`Schero94/slipstream`), not LLM-BOOM.

## Highlights

- **Peak free tolerance:** preferred quiet window stays **≥22 GiB** free+inactive, but admit band is **≥21.5** (`PEAK_FREE_GIB=22` − `PEAK_FREE_TOLERANCE_GIB=0.5`). A 21.81 near-miss no longer hard-aborts peak.
- **Smoke `PROFILE=peak`:** same env knobs; hard refuse still if free &lt; 17 (C admission) or free &lt; cache+headroom+2 (post-load floor estimate).
- **UI aligned:** `computeReco` / `applyPeak` / Start with cache≥14 use admit **21.5**; marginal confirm only below that. Copy still says prefer ≥22.

## Measured this pass (internal Qwen Metal smoke)

| | |
|--|--|
| free before | **24.74** GiB |
| knobs | cache **14** · io **4** · headroom **3** |
| decode (first short req, `--no-warmup`) | **8.37** tok/s |
| hit-rate | **77.69%** |
| swap | flat ~1229 MiB used |
| Stop | OK → free end **24.54** GiB |

Honest note: 8.37 is a cold/short first request, not a claim of historical warm peak ~18.9 tok/s.

## Build

`cargo tauri build --bundles app` from `app/src-tauri`, then `ditto` into `/Applications/Slipstream.app`.
