import unittest
from src.duration import parse_duration

class DurationEdgeTests(unittest.TestCase):
    def test_large_leading_field_and_zero(self):
        self.assertEqual(parse_duration("100:00:00"), 360000)
        self.assertEqual(parse_duration("0"), 0)

    def test_rejects_noncanonical_input(self):
        for value in ("", " 1", "1 ", "+1", "-1", "1::2", "1:2:3:4", "１２", 12, None):
            with self.subTest(value=value), self.assertRaises(ValueError):
                parse_duration(value)
