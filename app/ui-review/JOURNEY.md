# First-run journey review

**Goal of the user:** "I have a Mac. I want a fast local coding model running and connected to my editor."

Walked with Playwright against `app/dist` and a stateful `__TAURI__` stub. The stub simulates a fresh
Mac: nothing downloaded, download → convert → start all succeed. 1200×800, English. Screenshots are
in `journey/before-*.png` and `journey/after-{en,de}-*.png`.

## Before: 10 actions, 3 forced decisions, 2 dead ends

| # | What the user does | Notes |
|---|---|---|
| 1 | Opens the app on **Chat**. Clicks step 1, "Choose model folder". | A folder picker opens before any model has been chosen. A default (`~/Modelle`) exists, but the user is still asked a file-system question first (**decision 1**). |
| 2 | Confirms the folder, then lands on **Models**. | |
| 3 | Picks a model from 13 entries such as "Qwen3.6-35B-A3B (MTP)" or "Laguna S 2.1 (118B-A8B, DFlash)". | **Decision 2.** The names carry parameter counts and speculative-decoding brands. |
| 4 | Picks a quality such as "Q4_K_XL · fast (default)". | **Decision 3.** The quant names are jargon. |
| — | Looks for a download button on Models. There is none. The badge says "not loaded" and the Start button reads "Model not ready" and is **disabled**. | **Dead end 1.** Nothing says where to go next. |
| 5 | Finds the **Downloads** tab, which reads "Fetch the GGUF, then convert it into the streamable PGRN format". | |
| 6 | Clicks **Download** and waits several minutes. | |
| 7 | Comes back and clicks **Convert**, then waits again. | **Dead end 2.** Download does not continue into convert, so the user has to remember a second step whose name ("convert to PGRN") means nothing to them. |
| 8 | Clicks **Start** (top right). | Cache, context and I/O stay at the HTML defaults (10 GiB / 40k / 4) unless the user finds "Apply best" in Settings. |
| 9 | Opens **Settings** and scrolls about 1,400 px, past runtime check, backend, MLX, recommendation, cache, context, MTP, compact, grammar drafts, P2P and SSD streaming. | "Connect coding agent" is the tenth block on a 2,300 px page. |
| 10 | Clicks "Patch in" (Kilo/OpenCode) or "Copy config". | The confirmation was hard-coded German ("gepatcht … neu starten") even in English. |

**Tabs visited:** Chat → Models → Downloads → (topbar) → Settings.

### Places where two controls do the same thing
- Start in the top bar **and** "Start server" as journey step 2.
- Models folder on Models, the GGUF folder and PGRN path under "Source & location", **and** path overrides in Settings.
- Thinking in the chat composer **and** in Settings. Tools in the composer **and** "Enable tools" in Settings.
- "Enable P2P" on Cluster **and** in Settings.
- The "Models folder → Backend Auto → Start" hint in the top bar **and** again in Settings.
- Expected speed on Models **and** the recommendation in Settings.
- Three presets side by side (Apply best / Metal Peak / Good tokens) with no explanation of the difference.
- A "Send" prompt on Benchmarks besides the Chat composer.

### Terms a normal developer would not know
GGUF, **PGRN**, "convert", **sidecar**, experts.pgrn, MLX / oMLX, Metal, "Backend Auto (hybrid)", MTP,
DFlash, Q4_K_XL / UD-Q5_K_XL, A3B / A12B, **I/O threads**, Compact (slots), Grammar drafts, residency /
mlock / keep-hot, L3 peer, MCP, KV, RSS, PGRN high-water, prefill / decode, "cfg 10/4/3" in the top bar,
"cold_io", "io16".

## After: 4 clicks, 0 forced decisions, no dead ends

| # | What the user does | What the app does |
|---|---|---|
| 1 | Clicks step 1, "Set up a model", on Chat, or the top-right button, which now reads **"Set up model…"** and is never disabled. | Opens **Model** with the recommended model and quality already selected and focuses one button: **"Download & prepare · ~21 GiB"**. |
| 2 | Clicks **Download & prepare**. | Downloads, then starts the one-time SSD-streaming preparation (convert) by itself once the file is complete. One progress area and one status line. The button reads **Cancel** while busy. |
| — | Waits. No click needed. | The button turns into **Start**, and so does the top-right button. |
| 3 | Clicks **Start**. | If the user has not tuned by hand, the cache, context and I/O recommendation computed for this Mac is applied automatically. Lookahead prefetch is on by default. The button turns into **"Connect your editor →"**. |
| 4 | Clicks **Connect your editor →**, then clicks the editor's button ("Patch in" for Kilo/OpenCode, or "Copy config"). | **Connect editor** shows "The model is running. Your editor can connect now", the base URL, the model and the key. Step 3 on Chat gets a check mark. |

Counting the editor's own button, that is 4–5 clicks on 2 screens (Model → Connect editor), down from 10 actions over 4 screens.

## What moved where (nothing removed; every id and `invoke()` call kept)

- **Nav:** Chat · **Model** · **Connect editor** (new) · Live, then a separator, then Settings · Library (was Downloads) · Benchmarks · Logs, with Cluster soft-hidden at the bottom.
- **Model tab:** status, progress (`dlStatus`/`dlBar`/`dlNote`) and the resume row moved here from Downloads. A new `#prepareBtn` drives the whole flow by clicking the original `#dlBtn` / `#convBtn` / `#dlCancel`. Those buttons live on under "Manual steps". The models folder and the four path fields live under "Where models are stored".
- **Library tab:** now only the list of installed models.
- **Connect editor tab:** the agent card (Base URL / Model / API key / one-click patches) moved from Settings, with a running / not-running line on top. Codebase indexing is a collapsed "optional" block underneath.
- **Settings:** shows only "Tuned for your Mac" (Apply best; applied automatically until you change something). Everything else is under one **Advanced engine settings** disclosure with five flat groups and no disclosure inside it:
  - **Memory & speed:** presets with a one-line explanation, cache (tagged "auto" while auto-tuned), context, I/O, MTP, compact, grammar drafts, lookahead, predictive prefetch.
  - **Turbo.**
  - **Backend:** backend, MLX runtime, MLX options.
  - **Chat behaviour:** thinking, tools.
  - **Storage overrides:** mirror, buffered reads, MLX/GGUF paths.

  The runtime check is a collapsed "System check" that opens itself when something is not ready.
- **One source of truth for cache, context and I/O:** the slider always holds the value Start will use. While a model runs, the recommendation line shows the values it was started with ("In use now"), not a recomputation from free RAM that the engine itself is using.
- **Prepare card states:**

  | State | Status | Button |
  |---|---|---|
  | Not downloaded | "Not downloaded yet." | Download & prepare |
  | Downloading | "Download running…" | Cancel |
  | Preparing | "Preparing for SSD streaming…" | Cancel |
  | Download incomplete | "Download incomplete." | Resume download |
  | Preparation failed | "Preparation failed. Try again, or see Logs." | Try again |
  | Ready / stopped | "Ready to start." | Start |
  | Starting | "Starting… the model is loading into memory (about a minute)." | Starting… (disabled) |
  | Running | "Running — your editor can connect." | Connect your editor → |

  "Manual steps (advanced)" is a collapsed disclosure with one sentence of explanation and the original download, convert and cancel buttons.
- **Top bar:** the cryptic `cfg 10/4/3` readout is hidden (`#obsCfg` kept), and the hint reads "Set up a model, then Start".
- **Engine options:**
  - **Lookahead prefetch** is an on/off switch, **on by default**, and sends `pgrn_lookahead: 1` (0 when off). The copy uses the measured "lossless, 29–33 % faster decoding at 4–8 GiB cache".
  - **Turbo (experimental)** is a plain toggle, off by default, that sends `pgrn_steer: 0.01` when on. Only 0.01 was measured as acceptable: +28 % decode, ≈1.6 % higher perplexity. The copy says exactly that.
- **Copy:** "convert → PGRN" is now "prepare for SSD streaming", and statuses are plain sentences ("Not downloaded yet.", "Downloaded. One step left…", "Ready to start."). The agent confirmations are translated (EN/DE).

## Still open

- Choosing a model is still a list of 13 technical names. The default is sensible, but a short "recommended for your Mac" label (or hiding XL models on small machines) would help further.
- "Thinking" and "Tools" still exist both in the chat composer and in Advanced settings; they share one state, so it is not confusing, just duplicated.
- The recommendation text is recomputed from *current* free RAM, so while a model is running it shows a smaller cache than the one in use. The values Start uses are not affected, because auto-apply only runs while stopped.
- Chinese and Spanish fall back to English for the new strings (the language menu already shows their coverage %).
