import unittest
from src.planner import retry_plan

class ExplodesAfterSuccess:
    def __iter__(self):
        yield 500
        yield 200
        raise AssertionError("read after success")

class PlannerContractTests(unittest.TestCase):
    def test_stops_consuming_and_caps_policy_delay(self):
        self.assertEqual(retry_plan(ExplodesAfterSuccess()), [1])
        self.assertEqual(retry_plan([500] * 7 + [201]), [1, 2, 4, 8, 16, 16, 16])

    def test_non_retryable_and_invalid_status_rejected(self):
        for statuses in ([500, 400], [999], ["500"]):
            with self.subTest(statuses=statuses), self.assertRaises(ValueError):
                retry_plan(statuses)
