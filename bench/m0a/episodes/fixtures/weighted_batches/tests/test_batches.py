import unittest

from src.batches import weighted_batches


class BatchTests(unittest.TestCase):
    def test_greedy_consecutive_packing(self):
        items = [{"w": 2}, {"w": 3}, {"w": 4}, {"w": 1}]
        self.assertEqual(weighted_batches(items, lambda item: item["w"], 5), [items[:2], items[2:]])

    def test_empty_stream(self):
        self.assertEqual(weighted_batches([], lambda item: item, 2), [])
