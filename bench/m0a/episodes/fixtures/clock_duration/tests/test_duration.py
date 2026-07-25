import unittest
from src.duration import parse_duration

class DurationTests(unittest.TestCase):
    def test_supported_forms(self):
        self.assertEqual(parse_duration("42"), 42)
        self.assertEqual(parse_duration("02:03"), 123)
        self.assertEqual(parse_duration("1:02:03"), 3723)

    def test_rejects_out_of_range_clock_fields(self):
        for value in ("1:60", "1:60:00", "1:00:60"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                parse_duration(value)
