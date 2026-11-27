import assert from "node:assert/strict";
import fs from "node:fs";

const html = fs.readFileSync(new URL("../dist/index.html", import.meta.url), "utf8");
const js = fs.readFileSync(new URL("../dist/app.js", import.meta.url), "utf8");
const rust = fs.readFileSync(new URL("../src-tauri/src/main.rs", import.meta.url), "utf8");

for (const id of [
  "runtimeStatusCard",
  "runtimeBadge",
  "runtimeLlama",
  "runtimeConvert",
  "runtimeOmlx",
  "runtimeVersion",
  "runtimeModelDevice",
  "runtimePgrnDevice",
  "runtimeDiskFree",
  "runtimeNote",
]) {
  assert.match(html, new RegExp(`id=["']${id}["']`), `missing #${id}`);
}

assert.match(js, /invoke\("runtime_preflight"\)/);
assert.match(js, /invoke\("inspect_storage",\s*\{/);
assert.match(js, /function formatStorageDevice\([\s\S]*report\.internal/);
assert.match(js, /async function ensureNativeStartReady\(/);
assert.match(js, /async function startServer\(\)[\s\S]*await ensureNativeStartReady\(/);
assert.match(js, /if \(!storage\.admitted\)/);
assert.match(js, /if \(!runtime\.ready\)/);
assert.match(js, /filter\(\(c\) => c\.applicable && c\.required/);
assert.match(js, /c\.name\.startsWith\("omlx_"\)/);
assert.match(js, /c\.name\.startsWith\("pgrn_host"\)/);
assert.doesNotMatch(html, /value=["']ollama["']/i);
assert.doesNotMatch(js, /backend\.ollama|start_ollama|OLLAMA_MODELS/);
// Prompt-cache RAM is bounded and scaled by machine RAM (E1/E2 2026-11-26): 2048 MiB on
// >= 32 GB, 1024 MiB below. It must never be unbounded (llama.cpp default is 8 GiB).
assert.match(rust, /"--cache-ram",\s*cache_ram/);
assert.match(rust, /g >= 32\.0 => "2048"/);
assert.match(rust, /_ => "1024"/);

// Fast prompt reading (layer-major prefill) is ON by default (lossless, cancellable, live
// progress; RESULTS E4 190 -> 137 s): checked in the HTML, default-on when nothing is stored,
// serde default true in Rust, and each toggle stores only its own key.
assert.match(html, /id="pgrnLayerMajor" checked/);
assert.match(js, /function layerMajorPreference\(\)[\s\S]*?v === null \? true : v === "1"/);
assert.match(js, /pgrn_layer_major:\s*pgrnLayerMajorFromUi\(\)/);
assert.match(rust, /#\[serde\(default = "default_true"\)\]\s*pgrn_layer_major: bool/);
assert.match(rust, /let layer_major = ub_streamed && cfg\.pgrn_layer_major;/);
assert.match(js, /function persistNextEngineSettings\(ev\)/);

// Backend errors are "[code ...] English" and mapped to EN/DE; no German literals on the
// start/stop/download/convert paths.
for (const lit of ["Server läuft bereits", "Modell (.gguf) fehlt", "PGRN-Sidecar fehlt", "Download läuft bereits",
  "Konvertierung läuft", "Start fehlgeschlagen", "Server gestoppt", "Log-Fehler"]) {
  assert.ok(!rust.includes(lit), `German backend message left: ${lit}`);
}
assert.match(rust, /metal_memory_check\(free, gguf_total_bytes\(&cfg\.model\)/);

// Prompt progress for editor requests: evaluate the pure activity helpers on real log lines.
const grab = (name) => {
  const i = js.indexOf(`function ${name}(`);
  assert.ok(i >= 0, `missing ${name}`);
  return js.slice(i, js.indexOf("\n}\n", i) + 2);
};
const env = new Function(`
  const ACT_STAGE = { waiting: 1, prefill: 2, decode: 3, idle: 4 };
  const ACT_FINISH_STALL_MS = 6000, ACT_FINISH_FRACTION = 0.8;
  const t = (k) => k, LANG = "en", dec = (v) => String(v);
  const fillVars = (text, vars) => Object.keys(vars).reduce((o, k) => o.split("{" + k + "}").join(vars[k]), text);
  const tSafe = t;
  ${grab("parseActivity")}${grab("activityFromSlots")}${grab("mergeActivity")}${grab("prefillView")}
  ${grab("parseBackendErr")}
  return { parseActivity, activityFromSlots, mergeActivity, prefillView, parseBackendErr };`)();
const L = (s) => s.join("\n");
let a = env.parseActivity(L([
  "I slot prompt_start: id  0 | task 7 | n_prompt = 39517, n_cached = 0, n_process = 39517",
  "I slot prompt_progress: id  0 | task 7 | n_processed = 12000, n_prompt = 39517, progress = 0.304, t = 41000 ms",
]));
assert.equal(a.phase, "prefill"); assert.equal(a.task, 7);
assert.equal(a.pp.total, 39517); assert.equal(a.pp.processed, 12000); assert.equal(a.pp.lm, true);
let v = env.prefillView(a.pp, null, 0);
assert.equal(v.pct, 30); assert.ok(v.eta > 90 && v.eta < 100, `eta ${v.eta}`); assert.equal(v.finishing, false);
// the final checkpoint chunk after the layer-major pass reads as "finishing", not a frozen %
a = env.parseActivity(L([
  "I slot prompt_start: id  0 | task 7 | n_prompt = 39517, n_cached = 0, n_process = 39517",
  "I slot prompt_progress: id  0 | task 7 | n_processed = 36000, n_prompt = 39517, progress = 0.911, t = 120000 ms",
  "I slot print_timing: id  0 | task 7 | prompt processing, n_tokens =  36864, progress = 0.93, t = 121.00 s / 304.66 tokens per second",
]));
assert.equal(a.pp.tail, true); assert.equal(env.prefillView(a.pp, null, 0).finishing, true);
// prompt_done switches to the answer at once, prompt_abort / release to idle
assert.equal(env.parseActivity("I slot prompt_done: id  0 | task 7 | n_prompt = 1, n_cached = 0, n_processed = 1, t = 5 ms, tps = 2.0").phase, "decode");
assert.equal(env.parseActivity("I slot prompt_abort: id  0 | task 7 | n_prompt = 1, t = 5 ms").phase, "idle");
// stall near the end also counts as finishing (no log tail line yet)
v = env.prefillView({ total: 100, cache: 0, processed: 90, timeMs: 9000 }, { since: 0 }, 7000);
assert.equal(v.finishing, true);
// /slots preferred for numbers; the log wins a transition it already shows for the same task
const slot = env.activityFromSlots({ phase: "prompt", task: 7, total: 39517, cache: 0, processed: 13000, time_ms: 44000 });
const logP = env.parseActivity("I slot prompt_progress: id  0 | task 7 | n_processed = 12000, n_prompt = 39517, progress = 0.304, t = 41000 ms");
assert.equal(env.mergeActivity(slot, logP).pp.processed, 13000);
assert.equal(env.mergeActivity(slot, logP).src, "slots");
const logDone = env.parseActivity("I slot prompt_done: id  0 | task 7 | n_prompt = 1, n_cached = 0, n_processed = 1, t = 5 ms, tps = 2.0");
assert.equal(env.mergeActivity(slot, logDone).phase, "decode");
assert.equal(env.mergeActivity(null, logP).src, "log");
assert.equal(env.mergeActivity(env.activityFromSlots({ phase: "idle", task: -1 }), logP).src, "log");
// error codes
const e = env.parseBackendErr("[low_memory free=5.0 need=9.0 fit=2] Not enough free memory: 5.0 GiB free");
assert.deepEqual([e.code, e.vars.free, e.vars.need, e.vars.fit], ["low_memory", "5.0", "9.0", "2"]);
assert.equal(env.parseBackendErr("[start_failed] The engine could not be started: No such file").vars.detail, "No such file");
assert.equal(env.parseBackendErr("MLX start failed: x").code, null);

console.log("native runtime UI contract: PASS");
