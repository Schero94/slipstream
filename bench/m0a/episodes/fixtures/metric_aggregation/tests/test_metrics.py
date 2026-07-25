import unittest

from src.metrics import aggregate_metrics


class MetricTests(unittest.TestCase):
    def test_nested_means(self):
        snapshots = [
            {"cpu": {"user": 2, "sys": 4}, "rss": 10},
            {"cpu": {"user": 4, "sys": 8}, "rss": 14},
        ]
        self.assertEqual(aggregate_metrics(snapshots), {"cpu.user": 3.0, "cpu.sys": 6.0, "rss": 12.0})

    def test_requires_snapshot(self):
        with self.assertRaises(ValueError):
            aggregate_metrics([])

    def test_accepts_one_shot_snapshot_iterable(self):
        snapshots = iter([{"latency": 2}, {"latency": 4}])
        self.assertEqual(aggregate_metrics(snapshots), {"latency": 3.0})
