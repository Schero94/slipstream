#!/usr/bin/env node
/**
 * Static product contract: Tools and JSON Schema are common OpenAI Chat
 * features; multimodal/file parts remain capability-scoped.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const html = fs.readFileSync(path.join(root, "dist/index.html"), "utf8");
const appJs = fs.readFileSync(path.join(root, "dist/app.js"), "utf8");

// Common request contract — no backend return before tools / response_format.
assert.match(appJs, /function toolsEnabledForRequest\(/);
assert.doesNotMatch(appJs, /if \(!mlx\) return body;/);

// Common controls and persisted preferences use backend-neutral names.
assert.match(appJs, /function syncChatToolsUi\(/);
assert.match(appJs, /function syncChatJsonUi\(/);
assert.match(appJs, /slipstream\.chatTools/);
assert.match(appJs, /slipstream\.chatJson/);
assert.match(appJs, /slipstream\.chatJsonSchema/);
assert.match(appJs, /function pgrnProfileForStart\(resolved, pendingText\)/);
assert.match(appJs, /return "contract";/);
assert.match(appJs, /pgrn_profile: pgrnProfileForStart\(resolved, pending\)/);
assert.match(appJs, /state\.runningPgrnProfile = goingMlx \? cfg\.pgrn_profile/);
assert.match(appJs, /err\.mlxContractRestart/);
assert.match(html, /id="chatToolsSettingsWrap"/);
assert.match(html, /id="settingsChatTools"/);
assert.match(html, /id="chatToolsPrime"/);
assert.match(html, /data-i18n="lbl\.chatTools"/);
assert.match(appJs, /function maybePrimeLlamaTools\(\)/);
assert.match(appJs, /effectiveBackend\(\) === "mlx"/);
assert.match(appJs, /max_tokens:\s*1/);
assert.match(appJs, /cache_prompt:\s*true/);
assert.match(appJs, /toolPrimeStatus === "warming"/);
assert.match(appJs, /if \(sstate === "ready"\) maybePrimeLlamaTools\(\)/);

// Schema edits validate immediately and persist for the next backend/session.
assert.match(appJs, /const schema = \$\("chatSchema"\);/);
assert.match(appJs, /schema\.addEventListener\("input"/);
assert.match(appJs, /persistChatSchema\(schema\.value\)/);
assert.match(appJs, /updateChatSchemaStatus\(\)/);

// Backend switches may not hide or clear common controls.
assert.doesNotMatch(appJs, /\["chatToolsWrap", "chatJsonWrap"[^\n]+hidden = !mlx/);
assert.doesNotMatch(appJs, /if \(!mlx\)[\s\S]{0,220}chatTools[^\n]+checked = false/);

// Real extensions stay scoped: documents to MLX; images to VLM models.
assert.match(appJs, /\$\("chatDocAttach"\)\.hidden = !mlx/);
assert.match(appJs, /\$\("chatAttach"\)\.hidden = !vlm/);
assert.match(appJs, /if \(!vlm && state\.chatAttach\)/);
assert.match(appJs, /if \(!mlx && state\.chatDoc\)/);

console.log("test_cross_engine_contract.mjs: OK");
