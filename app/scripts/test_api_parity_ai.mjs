#!/usr/bin/env node
/** Pure request-shape fixture for the common local OpenAI coding contract. */
import assert from "node:assert/strict";

function buildResponseFormat(enabled, schemaText) {
  if (!enabled) return null;
  const raw = String(schemaText || "").trim();
  if (!raw) return { type: "json_object" };
  let schema;
  try {
    schema = JSON.parse(raw);
  } catch (error) {
    return { error: String(error) };
  }
  return {
    type: "json_schema",
    json_schema: {
      name: String(schema.title || schema.name || "response").replace(/[^a-zA-Z0-9_-]/g, "_"),
      schema,
      strict: true,
    },
  };
}

function buildChatRequestBody({ backend, tools = false, json = false, schema = "" }) {
  const body = {
    model: backend === "metal" ? "slipstream" : "Qwen3.6-35B-A3B-4bit",
    messages: [{ role: "user", content: "Return code" }],
    stream: true,
    stream_options: { include_usage: true },
    temperature: 0,
    chat_template_kwargs: { enable_thinking: false },
  };
  if (tools) {
    body.tools = [{ type: "function", function: { name: "calculator" } }];
    body.tool_choice = "auto";
  }
  if (json) {
    const responseFormat = buildResponseFormat(true, schema);
    if (responseFormat.error) return { error: responseFormat.error };
    body.response_format = responseFormat;
  }
  return body;
}

for (const backend of ["metal", "mlx"]) {
  const body = buildChatRequestBody({
    backend,
    tools: true,
    json: true,
    schema: '{"title":"CodeOut","type":"object"}',
  });
  assert.equal(body.tool_choice, "auto");
  assert.equal(body.tools[0].function.name, "calculator");
  assert.equal(body.response_format.type, "json_schema");
  assert.equal(body.response_format.json_schema.name, "CodeOut");
  assert.deepEqual(body.stream_options, { include_usage: true });
}

assert.deepEqual(
  buildChatRequestBody({ backend: "mlx", json: true }).response_format,
  { type: "json_object" },
);
assert.ok(buildChatRequestBody({ backend: "metal", json: true, schema: "{" }).error);

console.log("test_api_parity_ai.mjs: OK");
