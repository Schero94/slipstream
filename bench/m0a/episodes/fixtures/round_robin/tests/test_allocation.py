import unittest
from src.allocation import allocate

class AllocationTests(unittest.TestCase):
    def test_round_robin(self):
        self.assertEqual(allocate([1, 2, 3, 4, 5], ["a", "b"]), {"a": [1, 3, 5], "b": [2, 4]})
    def test_empty_items_keeps_workers(self):
        self.assertEqual(allocate([], ["a"]), {"a": []})
