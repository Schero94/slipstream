#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


MODULE_PATH = Path(__file__).with_name("qualify_app_tool_prime.py")
SPEC = importlib.util.spec_from_file_location("qualify_app_tool_prime", MODULE_PATH)
assert SPEC and SPEC.loader
prime = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = prime
sys.path.insert(0, str(MODULE_PATH.parent))
SPEC.loader.exec_module(prime)


class AppToolPrimeHarnessTest(unittest.TestCase):
    def test_tool_contract_matches_the_visible_app_tools(self) -> None:
        names = [tool["function"]["name"] for tool in prime.APP_TOOLS]
        self.assertEqual(names, ["get_current_time", "calculator"])
        request = prime.body("fixture", "warm", 1)
        self.assertEqual(request["max_tokens"], 1)
        self.assertEqual(request["tool_choice"], "auto")
        self.assertEqual(request["tools"], prime.APP_TOOLS)

    def test_cached_tokens_defaults_safely(self) -> None:
        self.assertEqual(prime.cached_tokens({"usage": {}}), 0)
        self.assertEqual(
            prime.cached_tokens({"usage": {"prompt_tokens_details": {"cached_tokens": 301}}}),
            301,
        )


if __name__ == "__main__":
    unittest.main()
