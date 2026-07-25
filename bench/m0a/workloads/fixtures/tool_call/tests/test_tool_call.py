import json
from pathlib import Path
import unittest


class ToolCallTests(unittest.TestCase):
    def test_exact_search_call(self):
        value = json.loads(Path("tool_call.json").read_text(encoding="utf-8"))
        self.assertEqual(
            value,
            {"tool": "search_code", "arguments": {"query": "draft-mtp"}},
        )
