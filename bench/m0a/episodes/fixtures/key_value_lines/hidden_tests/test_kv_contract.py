import unittest
from src.kv import parse_lines

class KvContractTests(unittest.TestCase):
    def test_one_shot_and_order(self):
        self.assertEqual(list(parse_lines(iter(["Z=1", "A=2"]))), ["Z", "A"])
    def test_malformed_and_duplicates(self):
        for lines in (["A=1", "A=2"], ["=x"], ["A="], ["A=B=C"], ["not valid=x"], [1]):
            with self.subTest(lines=lines), self.assertRaises(ValueError): parse_lines(lines)
