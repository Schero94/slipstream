import unittest

from src.replay import replay_log


class ReplayTests(unittest.TestCase):
    def test_commit_and_rollback(self):
        records = [
            ("begin", "a"), ("set", "a", "x", 1), ("commit", "a"),
            ("begin", "b"), ("set", "b", "x", 2), ("rollback", "b"),
        ]
        self.assertEqual(replay_log(records), ({"x": 1}, ["a"]))

    def test_interleaved_transactions_commit_atomically(self):
        records = iter([
            ("begin", "a"), ("begin", "b"), ("set", "a", "x", 1),
            ("set", "b", "y", 2), ("commit", "b"), ("commit", "a"),
        ])
        self.assertEqual(replay_log(records), ({"y": 2, "x": 1}, ["b", "a"]))

