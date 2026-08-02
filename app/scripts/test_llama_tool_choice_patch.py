#!/usr/bin/env python3
"""Release contract for llama.cpp's OpenAI-specific tool choice seam."""

from __future__ import annotations

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
PATCH = ROOT / "patches" / "slipstream-seams.patch"


class LlamaToolChoicePatchTest(unittest.TestCase):
    def test_specific_openai_tool_choice_is_preserved_in_release_patch(self) -> None:
        source = PATCH.read_text(encoding="utf-8")
        self.assertIn("requested_choice.is_object()", source)
        self.assertIn("tools = std::move(selected)", source)
        self.assertIn('tool_choice = \"required\"', source)
        self.assertIn("test_specific_openai_tool_choice_object", source)
        self.assertIn('@pytest.mark.parametrize(\"stream\"', source)


if __name__ == "__main__":
    unittest.main()
