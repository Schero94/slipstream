import unittest
from src.planner import retry_plan

class PlannerTests(unittest.TestCase):
    def test_retries_until_success(self):
        self.assertEqual(retry_plan([500, 503, 200]), [1, 2])

    def test_immediate_success(self):
        self.assertEqual(retry_plan([204]), [])
