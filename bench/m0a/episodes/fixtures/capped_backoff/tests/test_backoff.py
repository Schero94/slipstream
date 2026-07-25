import unittest
from src.backoff import backoff_delays

class BackoffTests(unittest.TestCase):
    def test_doubles_and_caps(self):
        self.assertEqual(backoff_delays(6, base=2, cap=10), [2, 4, 8, 10, 10, 10])
    def test_zero_attempts(self):
        self.assertEqual(backoff_delays(0), [])
