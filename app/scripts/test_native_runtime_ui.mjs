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
assert.match(rust, /"--cache-ram",\s*"512"/);

console.log("native runtime UI contract: PASS");
