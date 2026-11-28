#!/usr/bin/env node
/**
 * Contract: the "#DownloadMoreRAM" share card (Live tab) and the low-cache easter egg.
 * - drawShareCard renders on a canvas the model name, the decode tok/s, RAM, "Swap 0 MB" (only when
 *   swap is 0) and the cache hit rate; unknown values are left out (never invented)
 * - the share-card code makes no network call (no fetch / XMLHttpRequest / WebSocket / invoke)
 * - EN/DE strings exist for every share.* key and the vibes tooltip; the button has an a11y label
 * - the status tooltip says "Running on vibes and NVMe" only for an expert cache under 3 GiB
 */
import assert from "node:assert/strict";
import fs from "node:fs";

const html = fs.readFileSync(new URL("../dist/index.html", import.meta.url), "utf8");
const js = fs.readFileSync(new URL("../dist/app.js", import.meta.url), "utf8");
const mock = fs.readFileSync(new URL("../ui-review/mock/tauri_mock.js", import.meta.url), "utf8");

// ---- strings
const s0 = js.indexOf("const SHARE_I18N = {");
const s1 = js.indexOf("Object.keys(SHARE_I18N)", s0);
assert.ok(s0 > 0 && s1 > s0, "SHARE_I18N block missing");
const SH = new Function(js.slice(s0, s1) + "\nreturn SHARE_I18N;")();
assert.deepEqual(Object.keys(SH.de).sort(), Object.keys(SH.en).sort(), "EN/DE share keys differ");
for (const k of Object.keys(SH.en)) assert.ok(SH.en[k].trim() && SH.de[k].trim(), `empty ${k}`);
assert.equal(SH.en["pill.vibes"], "Running on vibes and NVMe");
assert.equal(SH.de["pill.vibes"], "Läuft auf Vibes und NVMe");
for (const m of html.matchAll(/data-i18n(?:-aria)?="(share\.[a-zA-Z]+)"/g)) assert.ok(SH.en[m[1]], `missing ${m[1]}`);
assert.match(html, /id="shareCardBtn"[^>]*aria-label="[^"]+"/, "share button needs an aria-label");
assert.match(html, /id="shareCardBtn"[^>]*data-i18n-aria="share.aria"/);

// ---- no network in the share-card code
const c0 = js.indexOf("// ---------------------------------------------------------------- share card ----");
assert.ok(c0 > 0, "share card section missing");
const code = js.slice(c0);
for (const bad of [/\bfetch\s*\(/, /XMLHttpRequest/, /WebSocket/, /\binvoke\s*\(/, /sendBeacon/]) {
  assert.ok(!bad.test(code), `share card must not use ${bad}`);
}

// ---- render with a recording canvas
const grab = (name) => {
  const i = js.indexOf(`function ${name}(`);
  assert.ok(i >= 0, `missing ${name}`);
  return js.slice(i, js.indexOf("\n}\n", i) + 2);
};
const texts = [];
const ctx = new Proxy({}, {
  get: (o, k) => {
    if (k === "fillText") return (s) => texts.push(String(s));
    if (k === "measureText") return (s) => ({ width: String(s).length * 20 });
    if (k === "createLinearGradient") return () => ({ addColorStop() {} });
    if (k in o) return o[k];
    return () => {};
  },
  set: (o, k, v) => { o[k] = v; return true; },
});
const canvas = { getContext: () => ctx };
const env = new Function("SH", `
  const LANG = "en";
  const t = (k) => SH.en[k] || k;
  const dec = (v, d) => Number(v).toFixed(d);
  const document = { querySelector: () => null };
  ${grab("fillVars")}${grab("pickObsStrip")}${grab("vibesMode")}${grab("shareCardFacts")}${grab("drawShareCard")}
  return { shareCardFacts, drawShareCard, vibesMode };`)(SH);

const GiB = 1073741824;
const live = { last_tps: 17.6, experts: { hit_rate: 78.4 }, serving_available: true, session: {},
  system: { memory: { total_bytes: 36 * GiB, swap_used_bytes: 0 } } };
const facts = env.shareCardFacts(live, { running: true, model: { name: "Groot-19B-A3B" } });
assert.deepEqual(facts, { model: "Groot-19B-A3B", ramGb: 36, tps: 17.6, swapZero: true, hit: 78.4 });
env.drawShareCard(canvas, facts);
const all = texts.join(" | ");
for (const want of ["Slipstream", "Groot-19B-A3B", "17.6", "tok/s", "36 GB RAM", "Swap 0 MB", "78 % expert cache hits",
  "#DownloadMoreRAM · github.com/Schero94/slipstream"]) {
  assert.ok(all.includes(want), `card lacks "${want}": ${all}`);
}
// unknown values are omitted, swap shown only when it is 0
texts.length = 0;
const f2 = env.shareCardFacts({ system: { memory: { total_bytes: 16 * GiB, swap_used_bytes: 5e8 } } }, { running: false, model: { name: "X" } });
assert.deepEqual(f2, { model: null, ramGb: 16, tps: null, swapZero: false, hit: null });
env.drawShareCard(canvas, f2);
const all2 = texts.join(" | ");
assert.ok(!/tok\/s|Swap 0|cache hits|X\b/.test(all2), `stopped card shows invented values: ${all2}`);
assert.ok(all2.includes("16 GB RAM") && all2.includes("No model running"));

// ---- easter egg threshold
assert.equal(env.vibesMode({ cache: 2 }), true);
assert.equal(env.vibesMode({ cache: 2.9 }), true);
assert.equal(env.vibesMode({ cache: 3 }), false);
assert.equal(env.vibesMode({ cache: 0 }), false);
assert.equal(env.vibesMode(null), false);
assert.match(js, /if \(vibesMode\(state\.runningCfg\)\) p\.title = t\("pill\.vibes"\)/);

// ---- mock knobs for the screenshot
assert.match(mock, /M\.lastTps \?\? 12\.4/);
assert.match(mock, /M\.hitRate \?\? 78/);

console.log("test_share_card.mjs: OK");
