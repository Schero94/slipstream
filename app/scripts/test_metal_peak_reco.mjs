#!/usr/bin/env node
/**
 * Contract: Metal Peak / Apply Best bands + io=4 product default.
 * No model load. Sources: METAL_PEAK_VS_SMOKE / BEST_DUAL_ENGINE_RECIPE.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const appJs = fs.readFileSync(path.join(root, "dist/app.js"), "utf8");
const html = fs.readFileSync(path.join(root, "dist/index.html"), "utf8");

assert.match(html, /value="4"[^>]*selected|selected[^>]*value="4"/);
assert.doesNotMatch(html, /value="8"[^>]*selected/);
assert.match(html, /id="applyPeak"/);
assert.match(html, /id="applyGoodTokens"/);
assert.match(html, /id="obsCfg"/);
assert.match(html, /hint\.p2pFreeze/);

assert.match(appJs, /function computeReco\(/);
// Peak preferred quiet ≥22 with 0.5 GiB tolerance → admit band ≥21.5
assert.match(appJs, /peakAdmit = 22 - 0\.5/);
assert.match(appJs, /free >= peakAdmit[\s\S]*cache = 14/);
assert.match(appJs, /free >= 17[\s\S]*cache = 10/);
assert.match(appJs, /const io = 4/);
assert.match(appJs, /function applyPeak\(/);
assert.match(appJs, /function applyGoodTokens\(/);
assert.match(appJs, /Metal peak gate/);
assert.match(appJs, /cache_gb >= 14/);
assert.match(appJs, /warn\.peakNeeds17/);
assert.match(appJs, /admit ≥21\.5/);
assert.match(appJs, /function formatObsCfg\(/);
assert.match(appJs, /Default 4 = 2×-qualified Metal balanced recipe/);

console.log("test_metal_peak_reco.mjs: OK");
