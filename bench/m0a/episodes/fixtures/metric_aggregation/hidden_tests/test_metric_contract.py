import math
import unittest

from src.metrics import aggregate_metrics


class MetricContractTests(unittest.TestCase):
    def test_one_shot_order_and_no_mutation(self):
        first = {"z": 2, "a": {"b": 4}}
        second = {"z": 4, "a": {"b": 8}}
        self.assertEqual(aggregate_metrics(iter([first, second])), {"z": 3.0, "a.b": 6.0})
        self.assertEqual(first, {"z": 2, "a": {"b": 4}})

    def test_rejects_shape_drift_and_invalid_trees(self):
        cases = ([{"a": 1}, {"b": 2}], [{"a": {}}], [{"bad.key": 1}], [{"a": True}], [{"a": math.inf}])
        for snapshots in cases:
            with self.subTest(snapshots=snapshots), self.assertRaises(ValueError):
                aggregate_metrics(snapshots)
