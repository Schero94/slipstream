#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


MODULE_PATH = Path(__file__).with_name("qualify_tool_schema_cache.py")
SPEC = importlib.util.spec_from_file_location("qualify_tool_schema_cache", MODULE_PATH)
assert SPEC and SPEC.loader
tool_cache = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = tool_cache
sys.path.insert(0, str(MODULE_PATH.parent))
SPEC.loader.exec_module(tool_cache)


class ToolSchemaCacheHarnessTest(unittest.TestCase):
    def test_scenarios_hold_schema_constant_before_changing_it(self) -> None:
        cases = tool_cache.scenarios("fixture")
        self.assertEqual([case["tool"] for case in cases], ["add", "add", "multiply", "add"])
        self.assertEqual(cases[0]["body"]["tools"], cases[1]["body"]["tools"])
        self.assertNotEqual(cases[1]["body"]["tools"], cases[2]["body"]["tools"])
        self.assertEqual(cases[0]["body"]["tool_choice"]["function"]["name"], "add")

    def test_cached_tokens_defaults_safely(self) -> None:
        self.assertEqual(tool_cache.cached_tokens({"usage": {}}), 0)
        self.assertEqual(
            tool_cache.cached_tokens({"usage": {"prompt_tokens_details": {"cached_tokens": 123}}}),
            123,
        )


if __name__ == "__main__":
    unittest.main()
