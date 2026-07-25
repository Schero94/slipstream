import unittest

from src.records import parse_record
from src.replay import replay_log


class ReplayContractTests(unittest.TestCase):
    def test_rejects_malformed_lifecycle(self):
        cases = [
            [("set", "a", "x", 1)], [("commit", "a")], [("begin", "a")],
            [("begin", "a"), ("begin", "a"), ("rollback", "a")],
            [("begin", "a"), ("commit", "a"), ("begin", "a"), ("commit", "a")],
            [("begin", "a"), ("rollback", "a"), ("commit", "a")],
        ]
        for records in cases:
            with self.subTest(records=records), self.assertRaises(ValueError):
                replay_log(iter(records))

    def test_record_validation(self):
        for value in ["begin", (), ("begin", ""), ("set", "a", "", 1), ("commit", "a", 1), ("wat", "a")]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                parse_record(value)

    def test_does_not_mutate_records(self):
        records = [["begin", "a"], ["set", "a", "x", []], ["commit", "a"]]
        before = [row[:] for row in records]
        state, committed = replay_log(iter(records))
        self.assertEqual(records, before)
        self.assertEqual(state, {"x": []})
        self.assertEqual(committed, ["a"])

