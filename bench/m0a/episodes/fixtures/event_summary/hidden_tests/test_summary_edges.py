import unittest

from src.summary import summarize_events


class OneShot:
    def __init__(self, values):
        self.values = iter(values)
        self.iterations = 0

    def __iter__(self):
        self.iterations += 1
        if self.iterations > 1:
            raise AssertionError("iterable consumed more than once")
        return self.values


class SummaryEdgeTests(unittest.TestCase):
    def test_one_shot_and_sorted_components(self):
        source = OneShot(["warning|zeta|hot", "info|alpha|ok"])
        result = summarize_events(source)
        self.assertEqual(result["components"], ["alpha", "zeta"])
        self.assertEqual(source.iterations, 1)

    def test_invalid_nonblank_record_propagates(self):
        with self.assertRaises(ValueError):
            summarize_events(["info|api|ok", "broken"])
