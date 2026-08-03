#!/usr/bin/env node
/**
 * Click and assistive-tech contract for the Slipstream shell.
 *
 * Found by driving the real UI in a browser, not by reading source: the tab strip
 * announced eight plain buttons with no indication of which one is current, every
 * toast (including the first-run "choose a model folder" error) was silent for
 * screen readers, and the four path fields under "Quelle & Speicherort" had labels
 * that were never associated — so they had no accessible name and clicking the
 * label text did not focus the field.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const html = fs.readFileSync(path.join(root, "dist/index.html"), "utf8");
const appJs = fs.readFileSync(path.join(root, "dist/app.js"), "utf8");

// --- Path inputs must be reachable by their visible label ---
for (const id of ["pDir", "pPgrn", "pUrl", "pServer"]) {
  assert.match(
    html,
    new RegExp(`<label[^>]*\\sfor="${id}"`),
    `label for="${id}" missing — the field has no accessible name and its label is not clickable`,
  );
}

// --- Tab strip must expose tablist semantics ---
assert.match(html, /<nav[^>]*role="tablist"/, "nav is not a tablist");
const tabButtons = html.match(/<button[^>]*class="tab[^"]*"[^>]*>/g) || [];
assert.ok(tabButtons.length >= 8, `expected at least 8 tab buttons, found ${tabButtons.length}`);
for (const tag of tabButtons) {
  assert.match(tag, /role="tab"/, `tab button without role=tab: ${tag.slice(0, 80)}`);
  assert.match(tag, /aria-selected="(true|false)"/, `tab button without aria-selected: ${tag.slice(0, 80)}`);
}

// --- showTab must keep aria-selected in sync with the visual active class ---
assert.match(
  appJs,
  /setAttribute\("aria-selected"/,
  "showTab never updates aria-selected, so the active tab is only visual",
);

// --- Toasts must be announced ---
assert.match(
  html,
  /<div id="toast"[^>]*role="status"/,
  'toast lacks role="status" — error messages are silent for screen readers',
);
assert.match(html, /<div id="toast"[^>]*aria-live="polite"/, "toast lacks aria-live");

console.log("a11y click contract: PASS");
