import unittest
from src.unique import unique_by

class UniqueTests(unittest.TestCase):
    def test_preserves_first_seen_order(self):
        values = [{"id": 2, "v": "a"}, {"id": 1, "v": "b"}, {"id": 2, "v": "c"}]
        self.assertEqual(unique_by(values, lambda item: item["id"]), values[:2])
