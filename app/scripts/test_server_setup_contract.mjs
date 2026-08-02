import assert from "node:assert/strict";
import fs from "node:fs";

const html = fs.readFileSync(new URL("../dist/index.html", import.meta.url), "utf8");
const js = fs.readFileSync(new URL("../dist/app.js", import.meta.url), "utf8");
const rust = fs.readFileSync(new URL("../src-tauri/src/p2p.rs", import.meta.url), "utf8");

for (const id of ["p2pMode", "p2pDonate", "p2pRemoteChat", "p2pWorkerDisclosure"]) {
  assert.match(html, new RegExp(`id=["']${id}["']`), `missing #${id}`);
}

assert.match(html, /selected worker[^<]*(decrypts|sees)[^<]*plaintext/i);
assert.match(html, /Sensitive[^<]*Secret[^<]*local/i);
assert.match(js, /p2pRemoteChat:\s*localStorage\.getItem\("slipstream\.p2p\.remoteChat"\)\s*===\s*"1"/);
assert.match(js, /state\.p2p\s*&&\s*state\.p2pRemoteChat/);
assert.match(js, /donateCapacity:\s*!!/);
assert.match(js, /mode:\s*p2pModeFromUi\(\)/);
assert.match(js, /p2pMode[\s\S]*st\.mode/);
assert.match(js, /p2pDonate[\s\S]*st\.donate_capacity/);
assert.match(rust, /mode:\s*Option<String>/);
assert.match(rust, /donate_capacity:\s*Option<bool>/);
assert.match(rust, /NodePolicy::for_mode/);

console.log("server setup contract: PASS");
