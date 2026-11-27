# First-run journey review

## v0.5 review (2026-11-27): 5 clicks → 3, and the engine always says what it is doing

**Goal of the user:** "I have a Mac. I want a fast local coding model running and connected to my editor —
and I want to know what it is doing while I wait."

### How to run the UI without the native app

```sh
python3 app/ui-review/mock/serve.py 4317     # serves app/dist with mock/tauri_mock.js injected
```

Open `http://127.0.0.1:4317/` (Playwright MCP: `browser_navigate`, `browser_click`, `browser_take_screenshot`).
The mock is stateful: a fresh Mac (nothing downloaded) → download → prepare → start → running, plus a
llama-server HTTP mock for `/v1/models` and streamed `/v1/chat/completions` (incl. `prompt_progress`
when the request asks for `return_progress`, and engine-format log lines for the activity banner).
Knobs on `window.__M`: `freeRam`, `diskFree`, `speed`, `prefillTokens` (e.g. 40000), `startCrash`
(engine exits while loading with a RAM-admission refusal in the log), `startErr` (backend refuses Start).
`app/scripts/browser_click_walk.js` passes against it (`browser_run_code_unsafe` with that file).
Screenshots: `journey/v05-before-en-*.png` and `journey/v05-after-{en,de}-*.png` (1200×800).

### Before (v0.4.1): 5 clicks on 3 screens

| # | Click | Screen |
|---|---|---|
| 1 | "Set up a model" | Chat → Model |
| 2 | "Download & prepare · ~21 GiB" | Model (wait: download, then preparation) |
| 3 | "Start" | Model (wait ~1 min) |
| 4 | "Connect your editor →" | Model → Connect editor |
| 5 | "Patch in" (Kilo Code / OpenCode) or "Copy config" | Connect editor |

### After: 3 clicks, 2 screens, no decisions forced

| # | Click | What happens |
|---|---|---|
| 1 | Chat step 1: **"Download the recommended model (Qwen3.6-35B-A3B, about 40 GiB on disk)"** | Download starts right from the first screen; step 1 shows "Download running… 13 %", then "Preparing for SSD streaming… 50 %"; the tip under the steps says the wait is expected. "Choose a different model" link for people who want to pick. |
| — | wait | Step 1 turns into "✓ Model ready", step 2 is highlighted. |
| 2 | Chat step 2: **"Start it"** (or Start top right) | "Starting…" everywhere; after the model is up, **Connect editor opens by itself** (first run only, only if the user is still on Chat/Model and has not chatted) with keyboard focus on the first "Set up" button. |
| 3 | **"Set up"** (Kilo Code / OpenCode) or "Copy config" | Editor configured; step 3 gets its check mark. |

Scoreboard row 6 (≤ 4 clicks): **3** (was 4–5 depending on how the editor click was counted; strictly 5).

### Findings

Fixed (in `app/dist`, commit on `feat/ux-v05`):

| # | Finding (as a newcomer) | Fix |
|---|---|---|
| F1 | **A 40k-token prompt is ~150–180 s of silence in Chat.** The assistant bubble was an empty dot; nothing said the prompt is being read or how long it takes. | Chat sends `return_progress: true` (Metal/llama-server only) and shows (mock values) *"Reading your prompt: 12,288 of 40,000 tokens (30 %) · 4 s · about 8 s left · 6,000 reused from earlier"*, then *"Prompt read in N s — writing the answer…"*. Before progress arrives: *"Waiting for the model… N s"*. Wait text never ends up as an answer or hides an error. |
| F2 | **Chat speed was wrong**: tok/s divided by time incl. prompt reading (0.6 tok/s shown for a 12 tok/s answer). | Speed counts from the first word; *"first word after 12 s"* shown next to it. |
| F3 | **Activity banner's decode state never appeared**: the regex expected `n_decoded = N, tg = X`, the engine prints `n_gen = N, tg = X t/s`. | Matches both. |
| F4 | Banner at prefill start showed **"0 / 0 Tokens"**, a token total computed from a progress value that includes cached tokens, jargon "Prefill — reading the prompt … Decode speed …", and the explanation was cut off after one line. | "Reading your prompt · 31 % · 12,288 tokens read in 4 s · about 8 s left"; ETA from elapsed time and progress; plain explanation with the recorded numbers (40k agent prompt ≈ 3 min the first time, follow-ups ≈ 1 s), wraps to two lines. Title is a polite live region (announced once per phase, not per second). |
| F5 | **Start that dies is a silent dead end.** If the engine exits while loading (RAM admission refuses, model too big), the app went Starting → Stopped with no reason. Also: `server_state` is "down" until the HTTP port opens, so the pill could flip Starting/Stopped and the top button read "Start" while starting. | 10 s grace before "down" counts; Stop works during it. A start that ends without Stop reads the log and says why in plain words — not enough free memory (with what to do), model too large for this Mac, disk full — plus the engine's own line (`[peregrine] … would break the 3072 MiB reserve …`). Button: "Try again". |
| F6 | **English UI showed German toasts**: "Auto → metal: Server gestartet - lädt Modell (~60s)...", "Modell (.gguf) fehlt - erst herunterladen.", etc. (backend strings). | Metal start toast uses the UI string; known German backend refusals (model/PGRN missing, already running, download running, PGRN exists, converter missing) are mapped to EN/DE. MLX start messages (English, carry RAM/disk numbers) still pass through. |
| F7 | **Disk need hidden**: the button promised "~21 GiB" but the setup writes ~40 GiB (download + prepared copy). No check before a 21 GiB download. | Status line: "Needs about 40 GiB of free disk space (21 GiB download + 19 GiB prepared copy) · 412 GiB free." If it cannot fit, the button reads "Not enough disk space" (disabled) with the way out (free space, smaller model, other folder). Same check before preparation. |
| F8 | Download showed only bytes, no speed or time left. | e.g. "4.2 / 21.0 GiB · 310 MB/s · about 55 s left" (smoothed). |
| F9 | Low free RAM had no warning before Start (the app picks a 4 GiB cache and starts). | Under 8 GiB free: "Only 6.0 GiB of memory is free right now. Close some apps first so the model starts smoothly." (warning only; admission stays in the engine). |
| F10 | **Settings changed while running did nothing visible** (e.g. turning Turbo on); only a general sentence said "Changes apply at the next Start". | A "Some settings changed. They apply when the model restarts. [Restart now]" note appears only when a start-relevant setting differs from what the engine was started with; Restart now = stop + start. Turning the setting back hides it. |
| F11 | "Connect your editor →" (the next step) was a grey button. | Primary button. |
| F12 | Jargon on everyday screens: "Patch in", top bar "cache 78% · RSS 17 GiB", unknown converter phase leaked as raw key (`conv.<phase>`), "Conversion started" toast, compatibility box (MoE, Q4_K, IQ/Q2/Q3/Q8_0/MXFP4) always open under the model card, 13 models with no hint which one to take. | "Set up" / "one click"; top bar "from RAM 78% · memory 17 GiB" (DE "aus RAM · RAM") with a plain tooltip; unknown phases read "working"; "Preparing the model for SSD streaming…"; compatibility note collapsed under "Which models work?"; default model labelled "— recommended". |
| F13 | Download progress status was one live region with the byte counter, so screen readers would hear it every second. | Only the status sentence is a live region; the new warning line is its own polite region. |

Open (with reason):

| # | Finding | Why not fixed here / what is needed |
|---|---|---|
| O1 | **Fixed (2026-11-28).** Editor requests now show "Reading your prompt X of Y (N %) · about M left" from the start and switch to "Writing the answer" the moment the prompt is done. Source: the engine's `/slots` (`phase` + `prompt_progress`, read by the menubar poller every 3 s and passed through `live_stats.slot_activity`, slotactivity.rs), with the new log lines `slot prompt_start / prompt_progress / prompt_done / prompt_abort` as fallback and for transitions (read every second). The menubar status line also shows "liest Prompt N %" / "schreibt Antwort…". Still open: a decode line before 100 tokens (until then the strip says "running…"); the menubar is German-only like the rest of its rows. | — |
| O2 | **Fixed (2026-11-28).** Layer-major now reports progress (`slot prompt_progress` every 3 s, `/slots` live) and is cancellable (31712b2). The last ~15 s of a long prompt (the server's checkpoint chunk, no finer progress) is shown honestly as "Almost done — finishing the last part (usually about 15 s more)" with a breathing bar (static under reduced motion), not a frozen percentage. "Fast prompt reading" is now **on by default** (lossless, RESULTS E4 190 → 137 s); existing users who stored "off" keep it (we cannot tell an explicit choice from the old all-toggles write), everyone else gets it. | — |
| O3 | **Fixed for start/stop/download/convert (2026-11-28).** main.rs returns `[code k=v] English text` (`ui_err`); `backendMsg` maps the codes to EN/DE and fills `{vars}` / `{detail}`. Success messages are English. Still German: embedder/Qdrant messages (out of scope; the indexing card shows them rarely) and the menubar. | Embedder/Qdrant: same `ui_err` pattern. |
| O4 | **Fixed (2026-11-28).** Before a Metal + PGRN start, `metal_memory_check` compares free + inactive memory (the engine's own admission basis) with a lower bound of what the engine plans (non-expert weights GGUF − PGRN + expert cache capped at the PGRN size + the 3 GiB reserve; KV/compute/staging left out, 0.5 GiB tolerance), so it only refuses starts the engine would refuse too, and returns "Not enough free memory: X GiB free, Y GiB needed … lower the expert cache to N GiB" (EN/DE). Dense models and unreadable memory skip the check (the engine/OS still decide). | Exact parity with the engine plan would need KV and staging sizes from the GGUF/PGRN headers. |
| O5 | Download speed/ETA comes from polling file size once a second; it is smoothed but rough on slow or resumed downloads. | Backend: report bytes/s from the downloader. |
| O6 | The model list still offers 400B–744B "XL" models on a 36 GB Mac (in their own group). | Product decision: hide or grey them out by RAM; left as is. |
| O7 | MLX settings (env names such as `SLIPSTREAM_PGRN_*`, `iogpu.wired_limit_mb`) are visible under Advanced even when MLX is not installed. | Low impact (inside Advanced); could be hidden until the MLX runtime is installed. |
| O8 | The disk-need estimate uses 0.9 × the download for the prepared copy (measured 19.0 / 21.0 GiB for the default); other models may differ slightly. | Catalog could carry the real `.pgrn` size per quant. |
| O9 | `journey.*` strings for Chinese and Spanish fall back to English (as before). | Translation pass. |

Tests after this pass: `node app/scripts/test_*.mjs` — all 7 green (a11y click contract, api parity, cross-engine,
metal peak reco, native runtime UI, prefer local P2P, server setup); `browser_click_walk.js` — 0 failures, no
console errors.

---

## v0.4 review (history)

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
