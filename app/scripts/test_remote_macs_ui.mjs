#!/usr/bin/env node
/**
 * Contract: v0.6 "Your Macs" (use another Mac / share this one).
 * - EN/DE string parity for every key the feature uses (REMOTE_I18N + data-i18n on the panel)
 * - top bar pill states (remote-on / waking / off / busy) from remote_status
 * - start_server is refused with [remote_active] while this Mac uses another Mac (main.rs + mock)
 * - all remote_* commands are registered and mocked; the community mesh stays gated
 * - the app's own requests to 8080 carry the local key only while using another Mac
 * Plan: docs/plans/2026-11-28-use-your-other-mac.md §6 / §9 M4.
 */
import assert from "node:assert/strict";
import fs from "node:fs";

const html = fs.readFileSync(new URL("../dist/index.html", import.meta.url), "utf8");
const js = fs.readFileSync(new URL("../dist/app.js", import.meta.url), "utf8");
const rust = fs.readFileSync(new URL("../src-tauri/src/main.rs", import.meta.url), "utf8");
const remoteRs = fs.readFileSync(new URL("../src-tauri/src/remote.rs", import.meta.url), "utf8");
const mock = fs.readFileSync(new URL("../ui-review/mock/tauri_mock.js", import.meta.url), "utf8");

// ---- strings: evaluate the REMOTE_I18N block in isolation ----
const start = js.indexOf("const REMOTE_I18N = {");
const end = js.indexOf("Object.keys(REMOTE_I18N)", start);
assert.ok(start > 0 && end > start, "REMOTE_I18N block missing");
const REMOTE = new Function(js.slice(start, end) + "\nreturn REMOTE_I18N;")();
const en = Object.keys(REMOTE.en).sort();
const de = Object.keys(REMOTE.de).sort();
assert.deepEqual(de, en, "EN/DE key sets differ");
for (const k of en) {
  assert.ok(REMOTE.en[k].trim() && REMOTE.de[k].trim(), `empty string ${k}`);
  // placeholders must match ({name}, {pct}, ...)
  const ph = (s) => (s.match(/\{[a-z]+\}/g) || []).sort().join(",");
  assert.equal(ph(REMOTE.de[k]), ph(REMOTE.en[k]), `placeholders differ for ${k}`);
}
// the keys the plan names
for (const k of ["nav.cluster", "pill.remote", "pill.remoteWaking", "pill.remoteOff", "pill.remoteBusy",
  "btn.stopRemote", "err.remote_active", "macs.useTitle", "macs.shareTitle", "macs.mayStart", "macs.privacy",
  "macs.removedYou", "macs.pairingsReset", "macs.advancedMesh", "macs.sharedReq"]) {
  assert.ok(REMOTE.en[k] && REMOTE.de[k], `missing ${k}`);
}
assert.equal(REMOTE.en["nav.cluster"], "Your Macs");
assert.equal(REMOTE.de["nav.cluster"], "Deine Macs");
// honest privacy text: names who could see prompts, and that nothing leaves the network
assert.match(REMOTE.en["macs.privacy"], /administers that Mac could see them/);
assert.match(REMOTE.en["macs.privacy"], /Nothing goes to the internet/);
assert.match(REMOTE.de["macs.privacy"], /könnte sie sehen/);

// every data-i18n key inside the Your Macs part of the panel resolves in EN and DE
const panel = html.slice(html.indexOf('data-panel="cluster"'), html.indexOf('id="meshAdvanced"'));
const panelKeys = [...panel.matchAll(/data-i18n(?:-aria)?="([^"]+)"/g)].map((m) => m[1]);
assert.ok(panelKeys.length > 20, "Your Macs panel has too few strings");
for (const k of panelKeys) assert.ok(REMOTE.en[k] && REMOTE.de[k], `panel key ${k} not in EN+DE`);
// every t("macs.*") / t("pill.remote*") used in app.js exists
for (const m of js.matchAll(/t\("((?:macs|pill\.remote|err\.)[A-Za-z_.]+)"\)/g)) {
  if (m[1].startsWith("err.") && !(m[1] in REMOTE.en)) continue; // older err.* keys live elsewhere
  assert.ok(REMOTE.en[m[1]] && REMOTE.de[m[1]], `t("${m[1]}") missing in EN/DE`);
}
for (const m of js.matchAll(/:\s*"((?:err|macs)\.[A-Za-z_]+)"/g)) {
  if (js.slice(js.indexOf("const REMOTE_BE_KEYS")).indexOf(m[1]) < 0) continue;
  assert.ok(REMOTE.en[m[1]], `REMOTE_BE_KEYS → ${m[1]} missing`);
}

// ---- pill states ----
const grab = (name) => {
  const i = js.indexOf(`function ${name}(`);
  assert.ok(i >= 0, `missing ${name}`);
  return js.slice(i, js.indexOf("\n}\n", i) + 2);
};
const env = new Function(`${grab("remotePillMode")}${grab("srvHeaders")}${grab("remoteUsing")}
  const state = { remote: null, apiKey: "k" };
  return { remotePillMode, srvHeaders, state, setRemote: (r) => { state.remote = r; } };`)();
const use = (o) => ({ mode: "using", use: { state: "connected", busy: false, queue_len: 0, ...o } });
assert.equal(env.remotePillMode(use({})), "remote-on");
assert.equal(env.remotePillMode(use({ busy: true, queue_len: 1 })), "remote-busy");
assert.equal(env.remotePillMode(use({ busy: true, queue_len: 0 })), "remote-on", "own request in flight is not 'busy'");
assert.equal(env.remotePillMode(use({ state: "connecting" })), "remote-waking");
assert.equal(env.remotePillMode(use({ state: "reconnecting" })), "remote-waking");
assert.equal(env.remotePillMode(use({ state: "unreachable" })), "remote-off");
assert.equal(env.remotePillMode(use({ state: "port_busy" })), "remote-off");
assert.equal(env.remotePillMode({ mode: "off", use: null }), "off");
for (const m of ["remote-on", "remote-waking", "remote-off", "remote-busy"]) {
  assert.match(fs.readFileSync(new URL("../dist/style.css", import.meta.url), "utf8"), new RegExp(`\\.pill-${m}\\b`));
}
// key only while using another Mac
env.setRemote({ mode: "off" });
assert.equal(env.srvHeaders({ a: 1 }).Authorization, undefined);
env.setRemote(use({}));
assert.equal(env.srvHeaders({ a: 1 }).Authorization, "Bearer k");
assert.doesNotMatch(js, /Authorization: "Bearer sk-local"/);
assert.ok((js.match(/headers: srvHeaders\(/g) || []).length >= 3, "chat/prime/models fetches use srvHeaders");

// ---- start_server refused while using another Mac ----
assert.match(rust, /fn start_server\([\s\S]*?if remote::using\(\) \{\s*return Err\(ui_err\(\s*"remote_active"/);
assert.match(rust, /fn start_server_impl\(app: &tauri::AppHandle, cfg: ServerConfig, state: &AppState\)/);
assert.match(rust, /fn server_state\(\) -> String \{\s*if remote::using\(\) \{\s*return "remote"\.into\(\);/);
assert.match(rust, /\|p\| !using && port_alive\(p\)/, "stop must never probe/kill the own forwarder");
assert.match(rust, /let api_key = remote::api_key\(\)\.unwrap_or\(cfg\.api_key\)/, "editor patch writes the local token");
assert.match(mock, /start_server: \(\{ cfg \}\) => \{[\s\S]*?if \(R\.mode === "using"\) throw "\[remote_active\]/);
assert.match(js, /remote_active: "err\.remote_active"/);

// ---- commands registered, mocked, implemented ----
const cmds = ["remote_status", "remote_share_start", "remote_share_stop", "remote_pair_code", "remote_discover",
  "remote_pair", "remote_use", "remote_stop_using", "remote_trusted", "remote_revoke", "remote_start_model",
  "remote_stop_model", "remote_set_model_control", "remote_api_key", "remote_pair_cancel", "remote_dismiss_removed"];
for (const c of cmds) {
  assert.match(rust, new RegExp(`remote::${c}\\b`), `${c} not in generate_handler!`);
  assert.match(remoteRs, new RegExp(`pub (async )?fn ${c}\\(`), `${c} not implemented`);
  assert.match(mock, new RegExp(`\\b${c}: `), `${c} not mocked`);
}
// first browse only after a click: remote_discover is called from the Find button only
const discoverCalls = [...js.matchAll(/invoke\("remote_discover"\)/g)].length;
assert.ok(discoverCalls >= 1);
assert.match(js, /if \(macs\.searching\) \{\s*try \{ macs\.found = await invoke\("remote_discover"\)/);
assert.match(js, /\$\("macsFind"\)\.onclick = macsFind;/);
// community mesh moved into a collapsed Advanced section, still gated by slipstream.p2p
assert.match(html, /<details class="card macs-mesh" id="meshAdvanced">/);
assert.doesNotMatch(html, /id="meshAdvanced" open/);
assert.match(js, /p2p: localStorage\.getItem\("slipstream\.p2p"\) === "1"/);
assert.match(html, /data-tab="cluster" id="navCluster"/);
assert.doesNotMatch(html, /nav-soft" data-tab="cluster"/, "Your Macs is always visible");

console.log("test_remote_macs_ui.mjs: OK");
