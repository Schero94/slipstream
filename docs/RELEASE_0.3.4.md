# Slipstream 0.3.4 — the shell is clickable and announced

A usability and accessibility release. **No engine, streaming, or performance
behaviour changed.** The v0.3.3 ad-hoc bundle seal and the measured v0.3.2 prompt-cache
and tool-schema gains carry over untouched.

## Why this exists

Every UI test in this repo is a regex over `app/dist/app.js` and
`app/dist/index.html`. That kind of test can prove a string is present; it cannot
prove a click does anything. Driving the real DOM instead surfaced four defects that
had survived every green run.

## Fixed

| Defect | Effect before |
|---|---|
| The four first-run path fields (`pDir`, `pPgrn`, `pUrl`, `pServer`) had `<label>`s that were never associated with them | The fields had **no accessible name**, and clicking the visible label text did not focus the field |
| The tab strip had no `role="tablist"` / `role="tab"` / `aria-selected` | Eight tabs announced as plain buttons with no indication of which one is current |
| `showTab()` moved only a CSS class | Even with the attributes present, the announced tab would never change |
| `#toast` had no `role="status"` / `aria-live` | Every message, including the first-run "choose a model folder" error, was silent for screen readers |

A new `data-i18n-aria` channel in `applyLang()` keeps the tablist label localized, so
it follows the EN/DE switch the way tips and placeholders already did.

## Verified

In a real browser against `app/dist`, with the HTTP cache disabled:

- 8 tabs render, no horizontal overflow, **zero console or page errors**
- `aria-selected` tracks **every** tab click and stays consistent with the visual state
- all four labels focus their input when clicked
- the tablist label switches *Hauptbereiche* ↔ *Main areas*
- the toast is a polite status region
- Tools / JSON / schema selections survive MLX → Metal → Auto and a full reload

Suites: **7/7** `app/scripts/test_*.mjs`, **8/8** `app/scripts/test_*.py`.

Re-runnable: `app/scripts/browser_click_walk.js` (serve `app/dist`, stub `__TAURI__`
via `addInitScript`, supply `page`). The same invariants are pinned statically in
`app/scripts/test_a11y_click_contract.mjs`, so they cannot silently regress.

## Two traps that produced false findings first

Both are worth knowing before the next browser session, because each one made a
correct app look broken:

- **Stub timing.** Patching `window.__TAURI__.core.invoke` *after* app init leaves the
  readiness gate holding stale state. The Start button then looks enabled with no
  model and the first-run journey CTA looks dead. Install the stub with
  `addInitScript` before load. With a correct fresh-install stub the journey behaves
  properly: Start is disabled and reads "Modell nicht bereit", and step 2 toasts and
  routes to Models.
- **Browser cache.** After editing `app/dist/app.js`, `page.reload()` can keep the old
  script while a separate `fetch()` shows the new file — so a working fix reads as
  broken. Disable the cache over CDP (`Network.setCacheDisabled`) before asserting.

## Artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.3.4_aarch64.dmg` | 67,325,951 | `f1e09b38ed98a39a2bbef5cb1a79b1a764ceec2a09facacd78eefd19b87afe3b` |
| `Slipstream_0.3.4_aarch64.zip` | 65,780,310 | `8f2106b66ac1f809f0d1d82d1f1b17ad404974f158ce3d5e361ce0eb11fb9ce7` |
| `slipstream-node_0.3.4_macos_aarch64.tar.gz` | 4,340,819 | `132da4aacdedac059d9664d0c39327d6a660e6812d337cd8572f37c79f7d8db5` |

The node archive reports version 0.3.4 and contains the arm64 binary, launchd,
systemd and non-root Docker templates plus the community-mesh operations guide —
the same layout as 0.3.3, verified by diffing the file lists. Linux stays supported
from source and through the documented Docker/systemd path; this release claims no
cross-compiled Linux binary and no CUDA result.

Bundle verification: every component the runtime manifest marks `required` is present
in the built `.app`, `codesign --verify --deep --strict` reports valid and
"satisfies its Designated Requirement", and `spctl` still rejects — the expected
outcome for an ad-hoc, unnotarized bundle.

## A release gate was missing, and it nearly shipped a hole

The first 0.3.4 candidate built, signed, verified and launched cleanly — and was
**incomplete**. `runtime-manifest.json` declares `omlx-pgrn/uv` as `required: true`,
but uv was never staged, so the bundle shipped without the 44 MB binary the MLX
runtime bootstrap needs to install anything on a user's machine.

Nothing caught it. `test_runtime_manifest.py` validates the manifest's shape, not
whether the files it declares exist. `codesign --deep --strict` passed because a
correctly signed bundle can still be missing content. The only signal was the
`.dmg` coming out 22 MB smaller than 0.3.3.

`app/scripts/test_staged_resources_complete.py` now fails the build when a required
component is unstaged, when a declared-executable component is not `+x`, or when the
staged uv drifts from the pinned version. Verified in both directions: green with uv
staged, and red — naming `omlx_uv -> omlx-pgrn/uv` and pointing at
`stage_uv_runtime.sh` — with it removed.

## Install

The app is signed **ad-hoc**, not with an Apple Developer ID, and is **not
notarized**. macOS may require right-click → Open on first launch.
