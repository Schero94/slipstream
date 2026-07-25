import unittest
from src.kv import parse_lines

class KvTests(unittest.TestCase):
    def test_values_and_ignored_lines(self):
        self.assertEqual(parse_lines([" A = one ", "", " # note", "B=two"]), {"A": "one", "B": "two"})
