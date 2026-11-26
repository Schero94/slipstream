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

console.log("native runtime UI contract: PASS");
