import unittest

from solution import stable_unique


class StableUniqueTests(unittest.TestCase):
    def test_preserves_first_seen_order(self):
        self.assertEqual(stable_unique([3, 1, 3, 2, 1]), [3, 1, 2])

    def test_supports_hashable_non_numbers(self):
        self.assertEqual(stable_unique(["a", "b", "a"]), ["a", "b"])

    def test_empty_input(self):
        self.assertEqual(stable_unique([]), [])
