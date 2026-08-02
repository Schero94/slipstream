#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


MODULE_PATH = (
    Path(__file__).resolve().parents[1]
    / "src-tauri/resources/omlx-pgrn/omlx/api/forced_tool_choice.py"
)
SPEC = importlib.util.spec_from_file_location("forced_tool_choice", MODULE_PATH)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = module
SPEC.loader.exec_module(module)


TOOLS = [
    {"type": "function", "function": {"name": "add", "parameters": {"type": "object"}}},
    {"type": "function", "function": {"name": "weather", "parameters": {"type": "object"}}},
]


class ForcedToolChoiceTest(unittest.TestCase):
    def test_specific_function_is_mandatory_and_other_tools_are_hidden(self) -> None:
        messages, tools = module.enforce_tool_choice(
            [{"role": "user", "content": "19+23"}],
            {"type": "function", "function": {"name": "add"}},
            TOOLS,
        )
        self.assertEqual([tool["function"]["name"] for tool in tools], ["add"])
        self.assertEqual(messages[0]["role"], "system")
        self.assertIn("MUST call the `add` function", messages[0]["content"])
        self.assertIn("Do not answer with text", messages[0]["content"])

    def test_required_choice_keeps_all_tools_but_forbids_plain_text(self) -> None:
        messages, tools = module.enforce_tool_choice(
            [{"role": "system", "content": "Original"}, {"role": "user", "content": "go"}],
            "required",
            TOOLS,
        )
        self.assertEqual(tools, TOOLS)
        self.assertTrue(messages[0]["content"].startswith("Original\n\n"))
        self.assertIn("MUST call one available function", messages[0]["content"])

    def test_auto_and_none_do_not_mutate_the_request(self) -> None:
        original = [{"role": "user", "content": "hello"}]
        for choice in (None, "auto", "none"):
            messages, tools = module.enforce_tool_choice(original, choice, TOOLS)
            self.assertEqual(messages, original)
            self.assertEqual(tools, TOOLS)

    def test_unknown_specific_function_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "not present"):
            module.enforce_tool_choice(
                [{"role": "user", "content": "go"}],
                {"type": "function", "function": {"name": "missing"}},
                TOOLS,
            )


if __name__ == "__main__":
    unittest.main()
