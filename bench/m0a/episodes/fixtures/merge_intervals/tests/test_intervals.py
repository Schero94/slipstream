import unittest
from src.intervals import merge_intervals

class IntervalTests(unittest.TestCase):
    def test_overlap_and_sort(self):
        self.assertEqual(merge_intervals([(5, 8), (1, 3), (2, 6)]), [[1, 8]])
    def test_disjoint(self):
        self.assertEqual(merge_intervals([(1, 2), (4, 5)]), [[1, 2], [4, 5]])
