import unittest
from src.rolling import rolling_average

class RollingTests(unittest.TestCase):
    def test_complete_windows(self):
        self.assertEqual(rolling_average([1, 2, 3, 4], 2), [1.5, 2.5, 3.5])
    def test_short_input(self):
        self.assertEqual(rolling_average([1], 2), [])
