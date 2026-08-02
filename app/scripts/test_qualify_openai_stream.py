#!/usr/bin/env python3
from __future__ import annotations

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import sys
import threading
import unittest


MODULE_PATH = Path(__file__).with_name("qualify_openai_stream.py")
SPEC = importlib.util.spec_from_file_location("qualify_openai_stream", MODULE_PATH)
assert SPEC and SPEC.loader
qualify = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = qualify
SPEC.loader.exec_module(qualify)


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: object) -> None:
        pass

    def _json(self, payload: object) -> None:
        raw = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self._json({"status": "ok"})
        elif self.path == "/v1/models":
            self._json({"data": [{"id": "fixture"}]})
        else:
            self.send_error(404)

    def do_POST(self) -> None:  # noqa: N802
        size = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(size))
        if body.get("tools"):
            prompt = str((body.get("messages") or [{}])[0].get("content") or "")
            arguments = {"a": 20, "b": 22} if "20 + 22" in prompt else {"a": 19, "b": 23}
            raw_arguments = json.dumps(arguments, separators=(",", ":"))
            split = len(raw_arguments) // 2
            deltas = [
                {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "add", "arguments": raw_arguments[:split]}}]},
                {"tool_calls": [{"index": 0, "function": {"arguments": raw_arguments[split:]}}]},
            ]
            finish = "tool_calls"
        elif body.get("response_format"):
            deltas = [{"content": "{\"answer\":42,"}, {"content": "\"label\":\"ok\"}"}]
            finish = "stop"
        else:
            deltas = [{"content": "4"}, {"content": "2"}]
            finish = "stop"

        events = [
            {"id": "x", "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
            for delta in deltas
        ]
        events.append({"id": "x", "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
        events.append({"id": "x", "choices": [], "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}})
        raw = b"".join(f"data: {json.dumps(event)}\n\n".encode() for event in events) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


class QualificationHarnessTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base_url = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def test_plain_stream_reaches_done_and_reports_metrics(self) -> None:
        result = qualify.run_case(self.base_url, "fixture", "plain", timeout=5)
        self.assertTrue(result["passed"])
        self.assertEqual(result["content"], "42")
        self.assertTrue(result["done"])
        self.assertEqual(result["usage"]["completion_tokens"], 2)
        self.assertGreaterEqual(result["content_chunks"], 2)
        self.assertGreaterEqual(result["decode_tokens_per_second"], 0)

    def test_json_schema_case_validates_the_exact_object(self) -> None:
        result = qualify.run_case(self.base_url, "fixture", "json", timeout=5)
        self.assertTrue(result["passed"])
        self.assertEqual(json.loads(result["content"]), {"answer": 42, "label": "ok"})

    def test_tool_case_assembles_streamed_arguments(self) -> None:
        result = qualify.run_case(self.base_url, "fixture", "tool", timeout=5)
        self.assertTrue(result["passed"])
        call = result["tool_calls"][0]
        self.assertEqual(call["function"]["name"], "add")
        self.assertEqual(json.loads(call["function"]["arguments"]), {"a": 19, "b": 23})

    def test_tool_case_accepts_an_explicit_schema_expectation(self) -> None:
        body = qualify.build_case_body("fixture", "tool")
        body["messages"][0]["content"] = "Use add to calculate 20 + 22."
        result = qualify.run_case(
            self.base_url,
            "fixture",
            "tool",
            timeout=5,
            body_override=body,
            expected_tool_arguments={"a": 20, "b": 22},
        )
        self.assertTrue(result["passed"])

    def test_endpoint_probe_checks_health_and_models(self) -> None:
        result = qualify.probe_endpoints(self.base_url, timeout=5)
        self.assertTrue(result["passed"])
        self.assertEqual(result["model_ids"], ["fixture"])

    def test_summary_rejects_nondeterministic_repetitions(self) -> None:
        first = qualify.run_case(self.base_url, "fixture", "plain", timeout=5)
        second = dict(first, output_sha256="different")
        summary = qualify.summarize([first, second], repetitions=2)
        self.assertFalse(summary["passed"])
        self.assertFalse(summary["cases"]["plain"]["deterministic"])

    def test_tool_determinism_ignores_ephemeral_call_ids(self) -> None:
        first = [{"id": "call_a", "type": "function", "function": {"name": "add", "arguments": "{\"a\":19,\"b\":23}"}}]
        second = [{"id": "call_b", "type": "function", "function": {"name": "add", "arguments": "{\"a\":19,\"b\":23}"}}]
        self.assertEqual(
            qualify._canonical_output("", first),
            qualify._canonical_output("", second),
        )

    def test_engine_decode_timing_wins_for_buffered_tool_events(self) -> None:
        seconds, rate = qualify._resolve_decode_metrics(
            completion_tokens=36,
            measured_seconds=0.0003,
            usage={"generation_duration": 11.0, "generation_tokens_per_second": 3.27},
        )
        self.assertEqual(seconds, 11.0)
        self.assertEqual(rate, 3.27)


if __name__ == "__main__":
    unittest.main()
