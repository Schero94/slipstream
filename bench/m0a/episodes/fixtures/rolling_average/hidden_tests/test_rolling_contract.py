import unittest
from src.rolling import rolling_average

class OneShot:
    def __init__(self, values):
        self.values = iter(values)
        self.calls = 0
    def __iter__(self):
        self.calls += 1
        if self.calls > 1:
            raise AssertionError("iterated twice")
        return self.values

class RollingContractTests(unittest.TestCase):
    def test_one_shot_and_full_window(self):
        source = OneShot([2, 4, 8])
        self.assertEqual(rolling_average(source, 3), [14 / 3])
        self.assertEqual(source.calls, 1)
    def test_invalid_windows(self):
        for window in (True, 0, -1, 1.5, "2"):
            with self.subTest(window=window), self.assertRaises(ValueError):
                rolling_average([1], window)
