# Iterative Product Loop 1–10 — Slipstream

**Date:** 2026-08-01  
**Orchestrator:** iterative product loop  
**Inputs:** `METAL_PEAK_VS_SMOKE.md`, `BEST_DUAL_ENGINE_RECIPE.md`, `P2P_PRODUCT_PLAN.md`  
**Canonical ship repo:** https://github.com/Schero94/slipstream (`_refs/slipstream`)  
**Baseline smoke:** cache=6 · io=1 · decode **6.24 tok/s** (`QWEN36_INTERNAL_SMOKE_20260801-114310.md`)

Hard constraints honored: ≥2 GiB free+inactive floor, one heavy serve, tear down after tests, internal `~/Modelle` primary, Qwen 3.6 first.

---

## Loop 1 — Auto/larger cache bands + io=4 default

**Goal:** When free allows, Apply Best prefers qualified 10/14 GiB cache with io=4 (not inventing io=8).

**Changes:**
- `computeReco()` bands: free≥22 → cache **14**; ≥17 → **10**; ≥12 → **8**; ≥8 → **6**; else **4**; always **io=4**, headroom **3**, post-load floor ≥2 GiB
- HTML `#io` default **8 → 4** (slipstream synced from LLM-BOOM)
- Rust `default_backend` **metal → auto** (slipstream)

**Tests:** `test_metal_peak_reco.mjs` OK; full `test_*.mjs` suite later green  
**RAM:** free+inactive ~17–20 GiB during work; no live serve

---

## Loop 2 — Metal Peak preset + Start-time gate

**Goal:** One-click qualified peak knobs + refuse/confirm when cache≥14 and free is tight.

**Changes:**
- Buttons: **Metal Peak** (`applyPeak` → 14/4/3, compact on) + confirm if free&lt;22, refuse if &lt;17
- Metal Start path: same gate when `cache_gb ≥ 14`
- Cache slider copy: `reco.peak` at 14 GiB; tip.cache mentions measured peak

**Tests:** contract + i18n keys EN=DE  
**RAM:** n/a (no serve)

---

## Loop 3 — Smoke script PROFILE / IO_THREADS

**Goal:** Path smoke stays conservative; warm/peak arms pass `--pgrn-io-threads`.

**Changes:** `live_qwen_metal_smoke.sh`
- `PROFILE=path|warm|peak` (default path = 6/1/4k)
- `IO_THREADS` → `--pgrn-io-threads` when &gt;1
- warm/peak preflight ≥17 GiB free+inactive

**Tests:** script syntax + header logging  
**Live:** deferred (see Loop 10) — free dropped below comfortable warm window

---

## Loop 4 — Observability: configured cache/io/headroom

**Goal:** A 6 GiB smoke cannot be misread as “peak failed.”

**Changes:** obs strip `cfg` = `cache/io/3` (e.g. `10/4/3`); title includes cfg

**Tests:** `test_obs_strip.mjs` + `test_metal_peak_reco.mjs` OK

---

## Loop 5 — Good-tokens coding preset

**Goal:** Shared Metal/oMLX coding path: thinking off (temp 0 already on send when think off).

**Changes:** **Good tokens** button → thinking/chatThink off + toast

**Tests:** HTML/JS contract + i18n

---

## Loop 6 — P2P CLI `--spawn-engine` freeze guard

**Goal:** Must-fix I1 — refuse second heavy serve from CLI.

**Changes (slipstream):**
- New `apps/p2p-node/src/spawn_guard.rs` — live lock pid + healthy endpoint refuse
- Wired in `runtime.rs` before launch; DANGER text in `--help`

**Tests:** `cargo test -p p2p-node` — spawn_guard unit tests + suite green  
**Refuse messages:** live lock / healthy `:8080` dual-serve risk

---

## Loop 7 — Prefer-local Chat gate regression

**Goal:** Must-fix — `state.running` ⇒ never `p2p_chat`.

**Changes:** `scripts/test_prefer_local_p2p.mjs` (slipstream + LLM-BOOM mirror)

**Tests:** OK both trees

---

## Loop 8 — Cluster engine honesty + freeze tip

**Goal:** Mock vs HTTP-attach clarity; freeze tip on Cluster.

**Changes:**
- `hint.p2pEngine` copy: HTTP attach to already-running Slipstream
- New `hint.p2pFreeze` in Cluster help

**Tests:** i18n + HTML data-i18n coverage OK

---

## Loop 9 — Seal JobResult (TM-007)

**Goal:** Must-fix — seal results on respond path; client open.

**Changes (slipstream):**
- `NetMessage::EncryptedJobResult` + `seal_job_result` / `open_job_result` on wire
- Cleartext `JobResult` kept as loopback/test exception only
- Tauri `p2p.rs` passes client keypair

**Tests:** `cargo test -p p2p-net -p p2p-crypto -p p2p-node` green (50+)  
**Backlog:** sticky peer / `choose_route` still open; cleartext accept fallback to remove later

---

## Loop 10 — Final verification + ship prep

**Goal:** Contracts green; document vs 6.2 tok/s; ship 0.2.32 if coherent.

**Tests run:**
- All `apps/peregrine-control/scripts/test_*.mjs` → **21/21 OK** (en=473=de)
- `cargo test -p p2p-node --lib` → **12 OK** (incl. spawn_guard + sealed wire)
- `cargo test -p p2p-net -p p2p-crypto -p p2p-node` → green

**Live smoke (installed 0.2.32):** `PROFILE=path` **PASS**
- Artifact: `QWEN36_INTERNAL_SMOKE_20260801-131629.md`
- free before **16.47** → ready **10.76** → after Stop **20.48** GiB; swap flat ~1.23 GiB used; Stop OK; RSS ~10.1 GiB; chat 64 tok / ~14 s wall
- **SKIP peak/warm** this arc (pre-smoke free ~16.5 &lt; preferred warm window comfort; peak needs ≥22). After Stop free recovered to **20.48** — warm reconfirm is now feasible as a follow-up.

**Product improvement vs 6.2 tok/s smoke:**
| Lever | Was (0.2.31 smoke) | Now (0.2.32 product) |
|--------|---------------------|----------------------|
| Default io | 8 (UI) / 1 (smoke) | **4** (qualified) |
| Apply Best cache | free-formula | **banded 6/8/10/14** |
| Peak path | not exposed | **Metal Peak** + Start gate |
| Smoke peak arm | impossible (no io flag) | `PROFILE=peak` + io threads |
| Obs honesty | hit% only | **cfg cache/io/headroom** |
| P2P freeze | UI spawn false only | **CLI spawn guard** + sealed results |

Absolute tok/s reconfirm of ~18.9 still needs a quiet ≥22 GiB free window with `PROFILE=peak` or UI Metal Peak — not claimed shipped as measured this pass.

---

## Remaining backlog (stop after 10)

1. Live reconfirm cache=10 (warm) and cache=14 (peak) when free+inactive ≥17 / ≥22, flat swap, sole serve
2. P2P sticky coding peer + Auto `choose_route` (plan loops 5–6)
3. Remove cleartext JobResult client fallback once no callers need it
4. Capability advert unify (`TODO(core)`)
5. DeepSeek V4 Flash later; MLX twin on internal when disk allows
6. Optional io=8 2× requal before offering as “fast IO” tier

---

## Commits / version

- Shipped: **Slipstream 0.2.32** — https://github.com/Schero94/slipstream/releases/tag/v0.2.32
- Installed: `/Applications/Slipstream.app` (CFBundleShortVersionString 0.2.32)
- Asset: `Slipstream_0.2.32_aarch64.zip` (app-only bundle; no notarization)
- LLM-BOOM keeps artifacts + control mirror; not the release remote
