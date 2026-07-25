import unittest

from src.join import inner_join


class JoinTests(unittest.TestCase):
    def test_inner_join_preserves_left_order(self):
        left = [{"id": 2, "name": "b"}, {"id": 1, "name": "a"}, {"id": 3}]
        right = [{"id": 1, "score": 9}, {"id": 2, "score": 8}, {"id": 4}]
        self.assertEqual(inner_join(left, right), [
            {"id": 2, "name": "b", "score": 8},
            {"id": 1, "name": "a", "score": 9},
        ])

    def test_conflicting_fields_fail(self):
        with self.assertRaises(ValueError):
            inner_join([{"id": 1, "x": 1}], [{"id": 1, "x": 2}])

    def test_duplicate_id_fails_even_when_other_side_is_empty(self):
        with self.assertRaises(ValueError):
            inner_join([{"id": 1}, {"id": 1}], [])

    def test_accepts_one_shot_iterators(self):
        left = [{"id": "a", "x": 1}]
        right = [{"id": "a", "y": 2}]
        self.assertEqual(
            inner_join(iter(left), iter(right)),
            [{"id": "a", "x": 1, "y": 2}],
        )
