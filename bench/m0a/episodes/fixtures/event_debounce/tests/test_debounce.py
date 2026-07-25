import unittest

from src.debounce import debounce


class DebounceTests(unittest.TestCase):
    def test_filters_per_key(self):
        events = [(0, "a"), (1, "a"), (2, "b"), (3, "a"), (3, "b")]
        self.assertEqual(debounce(events, 3), [events[0], events[2], events[3]])

    def test_zero_cooldown_keeps_all(self):
        events = [(1, "a"), (1, "a")]
        self.assertEqual(debounce(events, 0), events)

    def test_rejects_globally_decreasing_timestamps(self):
        with self.assertRaises(ValueError):
            debounce([(2, "a"), (1, "b")], 1)
