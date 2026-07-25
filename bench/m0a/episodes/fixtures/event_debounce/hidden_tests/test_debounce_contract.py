import math
import unittest

from src.debounce import debounce


class DebounceContractTests(unittest.TestCase):
    def test_one_shot_and_identity(self):
        events = [[0.0, "a"], [2.0, "a"]]
        result = debounce(iter(events), 2)
        self.assertIs(result[0], events[0])
        self.assertIs(result[1], events[1])

    def test_rejects_invalid_cooldown_event_and_order(self):
        for cooldown in (-1, True, math.inf):
            with self.subTest(cooldown=cooldown), self.assertRaises(ValueError):
                debounce([], cooldown)
        for events in ([(2, "a"), (1, "b")], [(math.nan, "a")], [(1,)], ["bad"]):
            with self.subTest(events=events), self.assertRaises(ValueError):
                debounce(events, 1)

    def test_unhashable_key_propagates_type_error(self):
        with self.assertRaises(TypeError):
            debounce([(0, [])], 1)
