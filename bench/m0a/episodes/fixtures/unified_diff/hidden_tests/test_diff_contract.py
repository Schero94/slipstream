import unittest

from src.diff import parse_unified_diff


class UnifiedDiffContractTests(unittest.TestCase):
    def test_multiple_files_deleted_file_and_marker(self):
        lines = [
            "--- a/a.txt", "+++ b/a.txt", "@@ -1 +1 @@", "-x", "+y",
            "\\ No newline at end of file",
            "--- a/gone.txt", "+++ /dev/null", "@@ -4,2 +0,0 @@", "-d", "-e",
        ]
        self.assertEqual([row["path"] for row in parse_unified_diff(iter(lines))], [
            "a.txt", "a.txt", "gone.txt", "gone.txt",
        ])

    def test_rejects_bad_order_and_truncated_counts(self):
        invalid = [
            ["@@ -1 +1 @@", " x"],
            ["--- a/x", "+++ b/x", "+x"],
            ["--- a/x", "+++ b/x", "@@ -1,2 +1,2 @@", " x"],
            ["--- a/x", "+++ b/x", "@@ -0,1 +1 @@", "-x", "+x"],
        ]
        for lines in invalid:
            with self.subTest(lines=lines), self.assertRaises(ValueError):
                parse_unified_diff(iter(lines))
