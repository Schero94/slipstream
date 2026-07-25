import unittest

from src.summary import summarize_events


class SummaryTests(unittest.TestCase):
    def test_summary_shape(self):
        lines = ["info|api|up", "error|db|down", "info|api|ready"]
        self.assertEqual(
            summarize_events(lines),
            {
                "total": 3,
                "levels": {"info": 2, "warning": 0, "error": 1},
                "components": ["api", "db"],
            },
        )

    def test_ignores_blanks(self):
        self.assertEqual(summarize_events(["", "  "])["total"], 0)
