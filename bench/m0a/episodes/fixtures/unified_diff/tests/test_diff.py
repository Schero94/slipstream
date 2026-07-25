import unittest

from src.diff import parse_unified_diff


class UnifiedDiffTests(unittest.TestCase):
    def test_parses_changes_and_line_numbers(self):
        lines = iter([
            "--- a/demo.py\n", "+++ b/demo.py\n", "@@ -2,3 +2,3 @@\n",
            " keep\n", "-old\n", "+new\n", " tail\n",
        ])
        self.assertEqual(parse_unified_diff(lines), [
            {"path": "demo.py", "kind": "delete", "old_line": 3, "new_line": None, "text": "old"},
            {"path": "demo.py", "kind": "add", "old_line": None, "new_line": 3, "text": "new"},
        ])

    def test_parses_created_file(self):
        self.assertEqual(parse_unified_diff([
            "--- /dev/null", "+++ b/new.txt", "@@ -0,0 +1,2 @@", "+a", "+b",
        ]), [
            {"path": "new.txt", "kind": "add", "old_line": None, "new_line": 1, "text": "a"},
            {"path": "new.txt", "kind": "add", "old_line": None, "new_line": 2, "text": "b"},
        ])
