#!/usr/bin/env python3
"""Release contract for llama.cpp's OpenAI-specific tool choice seam."""

from __future__ import annotations

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
PATCH = ROOT / "patches" / "slipstream-seams.patch"


class LlamaToolChoicePatchTest(unittest.TestCase):
    def test_specific_openai_tool_choice_is_preserved_in_release_patch(self) -> None:
        # Since the 8e23278c7c36 port the seam filters `tools` to the named function and leaves
        # tool_choice as the object; upstream's common_chat_tool_choice_parse_oaicompat maps
        # {"type":"function"} to COMMON_CHAT_TOOL_CHOICE_REQUIRED (smoke: bench/fetchwall/toolchoice_smoke.sh).
        source = PATCH.read_text(encoding="utf-8")
        self.assertIn('tool_choice.is_object() && json_value(tool_choice, "type", std::string()) == "function"', source)
        self.assertIn("tools = std::move(selected)", source)
        self.assertIn("is not present in tools", source)
        self.assertIn("test_specific_openai_tool_choice_object", source)
        self.assertIn('@pytest.mark.parametrize(\"stream\"', source)


if __name__ == "__main__":
    unittest.main()
