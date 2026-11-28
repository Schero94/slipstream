#!/usr/bin/env node
/**
 * Contract: Chat prefer-local gate (P2P must-fix #3).
 * When state.running is true, Chat must NEVER call p2p_chat.
 * P2P is only the explicitly enabled offline fallback. No live serve.
 *
 * Source: docs/pgrn-mlx/artifacts/P2P_PRODUCT_PLAN.md
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const appJs = fs.readFileSync(path.join(root, "dist/app.js"), "utf8");

/** Pure mirror of sendChat routing: local | p2p | hint */
function preferLocalChatRoute(running, p2p, remoteChat) {
  if (!running) {
    if (p2p && remoteChat) return "p2p";
    return "hint";
  }
  return "local";
}

// --- Source contract: prefer-local gate in sendChat ---
assert.match(appJs, /async function sendChat\(/);
assert.match(
  appJs,
  /Prefer local Metal\/MLX whenever the server is up;\s*P2P only as offline fallback/,
);
assert.match(
  appJs,
  /if\s*\(\s*!state\.running\s*\)\s*\{\s*if\s*\(\s*state\.p2p\s*&&\s*state\.p2pRemoteChat\s*\)\s*\{\s*await\s+sendChatViaP2p\(/,
);

// Chat's only p2p_chat path is sendChatViaP2p (Cluster may also call it).
assert.match(appJs, /async function sendChatViaP2p\(/);
assert.match(
  appJs,
  /async function sendChatViaP2p\([\s\S]*?invoke\(\s*"p2p_chat"/,
);

// sendChat must not invoke p2p_chat directly — only via the gated helper.
const sendChatStart = appJs.indexOf("async function sendChat(");
assert.ok(sendChatStart >= 0, "sendChat not found");
const afterSendChat = appJs.slice(sendChatStart);
// Next top-level-ish function after sendChat body — stop at initChat / similar
const sendChatEnd = afterSendChat.search(/\n(?:async )?function (?!sendChat)/);
const sendChatBody = sendChatEnd > 0 ? afterSendChat.slice(0, sendChatEnd) : afterSendChat.slice(0, 8000);
assert.doesNotMatch(
  sendChatBody,
  /invoke\(\s*"p2p_chat"/,
  "sendChat must not call invoke(\"p2p_chat\") directly",
);
assert.match(sendChatBody, /sendChatViaP2p\(/);
assert.match(sendChatBody, /if\s*\(\s*!state\.running\s*\)/);

// Doc comment on sendChatViaP2p: never used when Metal/MLX ready
assert.match(
  appJs,
  /Local server down \+ P2P flag on → sealed job \(never used when Metal\/MLX is ready\)/,
);

// --- EDGE: route table (running ⇒ never p2p) ---
assert.equal(preferLocalChatRoute(true, true, true), "local");
assert.equal(preferLocalChatRoute(true, true, false), "local");
assert.equal(preferLocalChatRoute(true, false, true), "local");
assert.equal(preferLocalChatRoute(false, true, true), "p2p");
assert.equal(preferLocalChatRoute(false, true, false), "hint");
assert.equal(preferLocalChatRoute(false, false, true), "hint");

// Must-fix #3: running + any p2p flag → never p2p
for (const p2p of [true, false, 1, 0, null, undefined]) {
  for (const remoteChat of [true, false, 1, 0, null, undefined]) {
    assert.notEqual(
      preferLocalChatRoute(true, p2p, remoteChat),
      "p2p",
      `state.running=true must never route to p2p (p2p=${p2p}, remote=${remoteChat})`,
    );
  }
}

// Privacy default: an available P2P connection alone never exports a prompt.
assert.equal(preferLocalChatRoute(false, true, false), "hint");

// --- v0.6 "Use another Mac": the paired Mac is reached through 127.0.0.1:8080 (the
// forwarder), so Chat treats it like the local server and never falls back to the community
// mesh while it answers. pollRemote sets state.running from the paired Mac's model.
assert.match(appJs, /async function pollRemote\(\)[\s\S]*?state\.running = !!\(u && u\.state === "connected" && u\.model_running\);/);
assert.match(appJs, /if \(sstate === "remote"\) \{\s*await pollRemote\(\);/);
const pollRemoteBody = appJs.slice(appJs.indexOf("async function pollRemote("), appJs.indexOf("async function poll()"));
assert.doesNotMatch(pollRemoteBody, /p2p_chat|sendChatViaP2p/, "remote mode never routes Chat to the mesh");
// Mirror: paired Mac in use + its model running ⇒ "local" route (forwarder on 8080).
const remoteRoute = (useConnected, modelRunning, p2p, remoteChat) =>
  preferLocalChatRoute(useConnected && modelRunning, p2p, remoteChat);
assert.equal(remoteRoute(true, true, true, true), "local");
assert.equal(remoteRoute(true, false, false, false), "hint", "remote model off: hint, no silent export");

console.log("test_prefer_local_p2p.mjs: OK");
