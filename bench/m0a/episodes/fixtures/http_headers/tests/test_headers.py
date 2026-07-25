import unittest

from src.headers import parse_headers


class HeaderTests(unittest.TestCase):
    def test_normalizes_and_collects_duplicates(self):
        self.assertEqual(
            parse_headers(["Host: example.test", "X-Tag: one", "x-tag: two"]),
            {"host": ["example.test"], "x-tag": ["one", "two"]},
        )

    def test_ignores_blank_lines(self):
        self.assertEqual(parse_headers(["", " \t", "A: b"]), {"a": ["b"]})
