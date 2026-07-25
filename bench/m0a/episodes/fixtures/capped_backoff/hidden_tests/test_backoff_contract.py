import unittest
from src.backoff import backoff_delays

class BackoffContractTests(unittest.TestCase):
    def test_numeric_values_and_large_attempt_count(self):
        self.assertEqual(backoff_delays(4, base=0.5, cap=2), [0.5, 1.0, 2.0, 2.0])
        self.assertEqual(backoff_delays(1000, base=1, cap=2)[-1], 2)
    def test_invalid_inputs(self):
        for args in ((True,), (-1,), (1, True, 2), (1, 0, 2), (1, 3, 2), (1, 1, False)):
            with self.subTest(args=args), self.assertRaises(ValueError):
                backoff_delays(*args)
