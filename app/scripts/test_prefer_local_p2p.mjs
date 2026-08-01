#!/usr/bin/env node
/**
 * Contract: Chat prefer-local gate (P2P must-fix #3).
 * When state.running is true, Chat must NEVER call p2p_chat.
 * P2P is only the offline fallback. No live serve.
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
function preferLocalChatRoute(running, p2p) {
  if (!running) {
    if (p2p) return "p2p";
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
  /if\s*\(\s*!state\.running\s*\)\s*\{\s*if\s*\(\s*state\.p2p\s*\)\s*\{\s*await\s+sendChatViaP2p\(/,
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
assert.equal(preferLocalChatRoute(true, true), "local");
assert.equal(preferLocalChatRoute(true, false), "local");
assert.equal(preferLocalChatRoute(false, true), "p2p");
assert.equal(preferLocalChatRoute(false, false), "hint");

// Must-fix #3: running + any p2p flag → never p2p
for (const p2p of [true, false, 1, 0, null, undefined]) {
  assert.notEqual(
    preferLocalChatRoute(true, p2p),
    "p2p",
    `state.running=true must never route to p2p (p2p=${p2p})`,
  );
}

console.log("test_prefer_local_p2p.mjs: OK");
