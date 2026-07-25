import math
import unittest
from src.intervals import merge_intervals

class IntervalContractTests(unittest.TestCase):
    def test_touching_one_shot_and_empty(self):
        self.assertEqual(merge_intervals(iter([(3, 4), (1, 3)])), [[1, 4]])
        self.assertEqual(merge_intervals([]), [])
    def test_invalid_intervals(self):
        for value in ([(2, 1)], [(True, 2)], [(1, math.inf)], [(1,)], ["12"]):
            with self.subTest(value=value), self.assertRaises(ValueError):
                merge_intervals(value)
